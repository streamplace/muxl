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
/// Layout: init segment + [track 1 moof+mdat …] + [track 2 …] + ….
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
    use crate::error::Error;
    use crate::fragment::{extract_flat_track_info, write_frame_fragment};
    use crate::hls::{BlobSegment, BlobTrack};
    use crate::io::ReadAtCursor;

    // Re-parse the moov from `input` so we can reuse the existing flat
    // sample-table extractor. The `source` argument is carried for
    // symmetry with `flat::write` and for callers that want to stabilize
    // the catalog before writing; the per-sample layout still comes from
    // `input`'s moov because that's where co64 / chunk offsets live.
    let _ = source;
    let mut cursor = ReadAtCursor::new(input).map_err(Error::Io)?;
    let catalog = crate::init::catalog_from_mp4(&mut cursor)?;
    let init = crate::init::build_init_segment(&catalog)?;
    let moov = crate::init::read_moov(&mut cursor)?;
    let track_inits = crate::init::build_track_init_segments(&catalog)?;

    let mut track_ids: Vec<u32> = moov.trak.iter().map(|t| t.tkhd.track_id).collect();
    track_ids.sort();

    output.write_all(&init)?;
    let mut write_offset = init.len() as u64;

    let mut sequence_number: u32 = 1;
    let mut tracks: Vec<BlobTrack> = Vec::new();

    for &tid in &track_ids {
        let trak = moov
            .trak
            .iter()
            .find(|t| t.tkhd.track_id == tid)
            .ok_or_else(|| Error::InvalidMp4(format!("track {tid} not found")))?;
        let samples = extract_flat_track_info(trak)?;
        // Bake leading-empty-edit into first fragment's tfdt — canonical
        // form has no elst in the init segment.
        let mut decode_time: u64 = crate::init::start_offset_from_trak(trak, moov.mvhd.timescale);
        let mut segments: Vec<BlobSegment> = Vec::new();
        let mut cur_seg_offset = write_offset;
        let mut cur_seg_size: u64 = 0;
        let mut cur_seg_dur: u64 = 0;
        let mut cur_seg_samples: u32 = 0;

        let is_video = catalog.video_configs().any(|v| v.track_id() == tid);
        let ts = if is_video {
            catalog
                .video_configs()
                .find(|v| v.track_id() == tid)
                .map(|v| v.timescale())
                .unwrap_or(1)
        } else {
            catalog
                .audio_configs()
                .find(|a| a.track_id() == tid)
                .map(|a| a.timescale())
                .unwrap_or(1)
        };
        // Video: flush at each keyframe. Audio: flush every ~2s.
        let audio_target_ticks = ts as u64 * 2;

        for sample in &samples {
            let should_flush = if is_video {
                sample.frame.is_sync && cur_seg_size > 0
            } else {
                cur_seg_dur >= audio_target_ticks
            };

            if should_flush {
                segments.push(BlobSegment {
                    offset: cur_seg_offset,
                    size: cur_seg_size,
                    duration_ticks: cur_seg_dur,
                    sample_count: cur_seg_samples,
                });
                cur_seg_offset = write_offset;
                cur_seg_size = 0;
                cur_seg_dur = 0;
                cur_seg_samples = 0;
            }

            let mut data = vec![0u8; sample.frame.size as usize];
            input
                .read_exact_at(sample.file_offset, &mut data)
                .map_err(Error::Io)?;

            let bytes_written = write_frame_fragment(
                output,
                sequence_number,
                tid,
                decode_time,
                &sample.frame,
                &data,
            )?;

            cur_seg_size += bytes_written;
            cur_seg_dur += sample.frame.duration as u64;
            cur_seg_samples += 1;
            write_offset += bytes_written;
            sequence_number += 1;
            decode_time += sample.frame.duration as u64;
        }

        if cur_seg_size > 0 {
            segments.push(BlobSegment {
                offset: cur_seg_offset,
                size: cur_seg_size,
                duration_ticks: cur_seg_dur,
                sample_count: cur_seg_samples,
            });
        }

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
