//! MUXL fMP4: fragmented MP4 with empty-stbl init segment + per-track
//! fragment runs.
//!
//! ```text
//! ftyp
//! moov     (empty stbl, mvex/trex present — canonical init segment)
//! [track 1 moof+mdat, moof+mdat, ...]
//! [track 2 moof+mdat, moof+mdat, ...]
//! ...
//! ```
//!
//! This module is the I/O layer for the fMP4 wrapper:
//!
//! - [`read`] / [`read_at`] — parse an fMP4 or any MP4-with-fMP4-body into a
//!   [`Source`] (catalog + per-track sample plan, no sample bytes).
//! - [`write`] — emit an fMP4 from a `Source`, streaming sample bytes from
//!   the original input.
//! - [`init_segment`] — build just the canonical `ftyp+moov` init segment
//!   from a catalog (no fragments). Suitable for HLS `#EXT-X-MAP`.
//! - [`read_stream`] — single-pass streaming reader for live ingest, no
//!   seek required.
//!
//! Spec: `canonical-form.md § MUXL fMP4`, `§ Init Segment moov`.

use std::io::{Read, Write};

use crate::catalog::Catalog;
use crate::error::Result;
use crate::io::ReadAt;
use crate::source::Source;

// Re-exports of the streaming reader primitives.
pub use crate::fragment::{FMP4Reader as StreamReader, Frame, fragment_fmp4};

/// Read an fMP4 (or fMP4-bodied file) into a [`Source`].
///
/// Requires random access. For a live fMP4 stream without seek, use
/// [`read_stream`] or [`StreamReader`].
pub fn read<R: ReadAt + ?Sized>(input: &R) -> Result<Source> {
    let (catalog, tracks) = crate::flat::plan_from_fmp4(input)?;
    Ok(Source {
        catalog,
        plan: crate::source::Plan::new(tracks),
    })
}

/// Single-pass streaming fMP4 reader — live-ingest path. Emits each frame
/// as it arrives. No seek required.
pub fn read_stream<R: Read>(input: R) -> Result<StreamReader<R>> {
    StreamReader::new(input)
}

/// Build the canonical `ftyp+moov` init segment for a catalog.
///
/// No fragments are written; callers pair this with an external fragment
/// stream (HLS, Hang over MoQ, etc.) or with [`write`] output minus the
/// init bytes.
pub fn init_segment(catalog: &Catalog) -> Result<Vec<u8>> {
    crate::init::build_init_segment(catalog)
}

/// Per-track init segments keyed by `track_id` — one small init per
/// rendition. Useful for HLS where each rendition needs its own map.
pub fn init_segments_per_track(
    catalog: &Catalog,
) -> Result<std::collections::BTreeMap<u32, Vec<u8>>> {
    crate::init::build_track_init_segments(catalog)
}

/// Write a `Source` as an fMP4 to `output`, streaming sample bytes from
/// `input` (the original ReadAt the source was built from).
///
/// Layout: init segment + the canonical time-sliced segment body — each GoP's
/// moof+mdat pairs for all tracks contiguously (track_id-ascending), then the
/// next GoP (see `flat::write_canonical_body`), NOT grouped per track.
///
/// Returns per-track HLS metadata (byte ranges, codec info, init CIDs)
/// collected during the write. HLS callers consume this directly; other
/// callers can ignore the return value.
pub fn write<R: ReadAt + ?Sized, W: Write>(
    source: &Source,
    input: &R,
    output: &mut W,
) -> Result<Vec<crate::hls::BlobTrack>> {
    use crate::cid;
    use crate::flat::{plan_canonical_body, write_canonical_body};
    use crate::hls::BlobTrack;

    let catalog = source.catalog.clone();
    let init = crate::init::build_init_segment(&catalog)?;
    let track_inits = crate::init::build_track_init_segments(&catalog)?;

    output.write_all(&init)?;
    let body_start_offset = init.len() as u64;

    // Plans sorted by track_id (Plan::new already does this; defensive sort).
    let mut ordered_plans: Vec<&crate::source::TrackPlan> = source.plan.tracks.iter().collect();
    ordered_plans.sort_by_key(|p| p.track_id);

    // Stream the canonical-segment body — same path the flat writer uses.
    let plan = plan_canonical_body(&catalog, &ordered_plans)?;
    let body = write_canonical_body(&ordered_plans, &plan, body_start_offset, input, output)?;
    let write_offset = body_start_offset + body.total_bytes;

    let mut tracks: Vec<BlobTrack> = Vec::with_capacity(ordered_plans.len());
    for (ti, plan_) in ordered_plans.iter().enumerate() {
        let tid = plan_.track_id;
        let ts = plan_.timescale;
        let segments = body.per_track_segments[ti].clone();
        let init_data = track_inits.get(&tid).cloned().unwrap_or_default();
        let init_cid = cid::from_bytes(&init_data);

        let (track_type, codec, width, height, channels, sample_rate): (
            &str,
            String,
            u32,
            u32,
            u32,
            u32,
        ) = if let Some(v) = catalog.video_configs().find(|v| v.track_id() == tid) {
            ("video", v.codec.clone(), v.coded_width, v.coded_height, 0, 0)
        } else if let Some(a) = catalog.audio_configs().find(|a| a.track_id() == tid) {
            ("audio", a.codec.clone(), 0, 0, a.number_of_channels, a.sample_rate)
        } else {
            ("unknown", String::new(), 0, 0, 0, 0)
        };

        tracks.push(BlobTrack {
            track_id: tid,
            track_type: track_type.to_string(),
            codec,
            timescale: ts,
            init_cid,
            init_data,
            blob_cid: String::new(), // HLS caller fills after hashing
            blob_size: 0,
            segments,
            width,
            height,
            channels,
            sample_rate,
        });
    }

    let total_gops: usize = tracks
        .iter()
        .filter(|t| t.track_type == "video")
        .flat_map(|t| &t.segments)
        .count();
    eprintln!("fMP4 written ({total_gops} GOPs, {write_offset} bytes)");
    Ok(tracks)
}
