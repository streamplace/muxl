//! Metafile: a payload-free, versioned, self-contained view of one canonical
//! segment's metadata, and on-demand synthesis of a flat-MP4 faststart header
//! from an ordered set of them — *without* the segment bytes.
//!
//! This is what makes "flat-MP4 VOD" content-addressable. A consumer archives
//! exactly one metafile per canonical segment (`.m4s`); to play any contiguous
//! range it feeds those metafiles to [`synthesize_flat_header`] and serves
//! `[synthesized header][canonical blob range]` — a byte-range-seekable MP4
//! whose `moov` is exact, with no random access over the blob and no re-mux.
//!
//! One metafile maps 1:1 to one canonical `.m4s` and is **self-contained**: it
//! carries that segment's single-track catalog (codec config the `moov` needs)
//! alongside the per-sample tables, byte size, and first decode time. There is
//! no separate init blob — the catalog is small and the canonical `.m4s`
//! already embeds it per segment, so mirroring that keeps the archive a flat
//! per-segment store with no coordination. The synthesizer aggregates the
//! per-segment catalogs into the multi-track `moov`.
//!
//! Wire form is DRISL / dag-cbor; field names match the live [`CborEvent`]
//! stream (a metafile is its payload-free subset — no `tracks` — plus the
//! catalog), so the same consumer-side decoder reads both. It is a plain struct
//! rather than a `#[serde(tag)]` enum so DRISL decodes the catalog's byte
//! fields directly (a tagged enum buffers through an intermediate that can't).
//!
//! Offsets are owned entirely by muxl: the synthesized `co64` already resolves
//! to `header_len + body_offset + per_sample_offset`, so the caller passes no
//! base offset — it serves the header then the segment bodies in input order.
//! See `spec/canonical-form.md § Metafile`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::cbor::{CborTrackSamples, METAFILE_VERSION};
use crate::error::{Error, Result};
use crate::flat::{SegmentMetadata, build_synth_flat_header};

/// One canonical segment's self-contained metafile, payload-free. DRISL key
/// `type` = `"segment"`. Single-track (one canonical `.m4s`); maps are keyed by
/// stringified track id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetafileSegment {
    /// Always `"segment"` — present for wire symmetry with the event stream.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Wire-format version ([`METAFILE_VERSION`]).
    #[serde(default)]
    pub version: u16,
    /// This segment's single-track catalog (codec/dimensions/timescale +
    /// decoder config bytes) — the codec config the synthesized `moov` needs.
    pub catalog: Catalog,
    /// Per-track per-sample tables (stsz/stts/ctts/stss + per-sample offsets).
    #[serde(default)]
    pub samples: BTreeMap<String, CborTrackSamples>,
    /// Per-track on-disk byte size of this segment (uuid prefix + moof+mdat
    /// run, incl. any c2pa signature) — for `co64` placement.
    #[serde(default)]
    pub track_byte_sizes: BTreeMap<String, u64>,
    /// Per-track decode time (`tfdt`) of this segment's first sample — the
    /// `elst` anchor when a range begins mid-stream.
    #[serde(default)]
    pub first_decode_times: BTreeMap<String, u64>,
    /// Per-track total duration in timescale ticks (HLS convenience).
    #[serde(default)]
    pub durations: BTreeMap<String, u64>,
    /// Per-track sample count (HLS convenience).
    #[serde(default)]
    pub sample_counts: BTreeMap<String, u32>,
    /// Total body bytes this segment contributes (sum of `track_byte_sizes`).
    #[serde(default)]
    pub body_size: u64,
    /// Playable duration in microseconds (HLS `EXTINF`); 0 when unknown.
    #[serde(default)]
    pub duration_us: u64,
}

/// Where one input segment's body lands in the synthesized flat MP4. Offsets
/// are relative to the start of the body (== the end of the synthesized
/// header), so a caller serving `[header][body]` maps a playback `Range:` to
/// `[header.len() + body_offset, header.len() + body_offset + body_size)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentLayout {
    /// Byte offset of this segment's contribution within the body.
    pub body_offset: u64,
    /// Byte size of this segment's contribution (its on-disk canonical bytes).
    pub body_size: u64,
}

/// A synthesized flat-MP4 faststart header plus the body layout the caller
/// reproduces when serving `[bytes][body]`.
#[derive(Debug, Clone)]
pub struct FlatHeader {
    /// `ftyp + moov + mdat-envelope-header`. The `moov`'s `co64` already points
    /// at `bytes.len() + body_offset + per_sample_offset`, and the mdat
    /// envelope's largesize declares `total_body`, so the caller streams these
    /// bytes then the segment bodies verbatim, in input order.
    pub bytes: Vec<u8>,
    /// Total body bytes the header expects to follow it.
    pub total_body: u64,
    /// Per input segment, in order: where its bytes land in the body.
    pub segments: Vec<SegmentLayout>,
}

/// Build the self-contained metafile for ONE canonical segment (`.m4s`).
///
/// Carries the segment's single-track catalog, per-sample tables, on-disk byte
/// size (for `co64` placement), and first sample's decode time (the `elst`
/// anchor) — with no payload. `byte_size`/offsets are taken from the bytes as
/// given, so feed the *final* stored bytes (post-signing) for an exact header.
pub fn segment_metafile(segment_bytes: &[u8]) -> Result<MetafileSegment> {
    let (tid, ts, first_dts) = crate::present::segment_index(segment_bytes)?;
    let catalog = crate::catalog::from_segment(segment_bytes)?;
    let key = tid.to_string();
    let dur_ticks: u64 = ts.durations.iter().map(|&d| d as u64).sum();
    let sample_count = ts.durations.len() as u32;
    let byte_size = segment_bytes.len() as u64;
    Ok(MetafileSegment {
        kind: "segment".into(),
        version: METAFILE_VERSION,
        catalog,
        samples: BTreeMap::from([(key.clone(), (&ts).into())]),
        track_byte_sizes: BTreeMap::from([(key.clone(), byte_size)]),
        first_decode_times: BTreeMap::from([(key.clone(), first_dts)]),
        durations: BTreeMap::from([(key.clone(), dur_ticks)]),
        sample_counts: BTreeMap::from([(key, sample_count)]),
        body_size: byte_size,
        // duration_us needs the timescale (not available from segment bytes
        // alone); it's an HLS convenience and unused by header synthesis.
        duration_us: 0,
    })
}

/// Stream one self-contained metafile per canonical segment of a MUXL wrapper
/// (bare m4s, fMP4, or flat MP4), front-to-back. Constant memory — never holds
/// more than one segment — so it runs over a multi-GB blob in ~one GoP of RAM.
pub fn metafiles_stream<R: Read, F>(reader: R, mut emit: F) -> Result<()>
where
    F: FnMut(MetafileSegment) -> Result<()>,
{
    crate::reader::scan_wrapper_stream(reader, |seg| emit(segment_metafile(seg)?))
}

/// Synthesize a flat-MP4 faststart header from an ordered set of self-contained
/// per-segment metafiles (canonical interleave order). The multi-track catalog
/// is aggregated from the segments' single-track catalogs.
///
/// Deterministic: identical input yields byte-identical `bytes`, so the result
/// can be content-addressed and cached. Segment metafiles are regrouped into
/// GoPs (a new GoP begins when a track id repeats — the same rule `unwrap`
/// orders by), then handed to [`build_synth_flat_header`].
pub fn synthesize_flat_header(segments: &[MetafileSegment]) -> Result<FlatHeader> {
    if segments.is_empty() {
        return Err(Error::InvalidMp4("no metafile segments to synthesize".into()));
    }

    // Aggregate the per-segment single-track catalogs into the multi-track
    // catalog the moov needs (same dedupe-by-rendition-name as unwrap).
    let mut catalog = Catalog::default();
    for seg in segments {
        crate::reader::merge_segment_catalog(&mut catalog, &seg.catalog);
    }

    // Regroup the per-track segment metafiles into per-GoP SegmentMetadata, and
    // record each metafile's body placement (cumulative byte size in order).
    let mut gops: Vec<SegmentMetadata> = Vec::new();
    let mut cur: Option<SegmentMetadata> = None;
    let mut cur_tids: BTreeSet<u32> = BTreeSet::new();
    let mut layouts: Vec<SegmentLayout> = Vec::new();
    let mut running_body: u64 = 0;

    for seg in segments {
        let seg_body: u64 = seg.track_byte_sizes.values().sum();
        layouts.push(SegmentLayout {
            body_offset: running_body,
            body_size: seg_body,
        });
        running_body += seg_body;

        // Fold this metafile's track(s) into the current GoP, flushing first
        // when a track id collides (tracks ascend within a GoP). Handles
        // per-track metafiles and pre-grouped multi-track ones alike.
        let mut tids: Vec<u32> = seg.samples.keys().filter_map(|k| k.parse().ok()).collect();
        tids.sort_unstable();
        for tid in tids {
            if cur_tids.contains(&tid) {
                if let Some(g) = cur.take() {
                    gops.push(g);
                }
                cur_tids.clear();
            }
            let g = cur.get_or_insert_with(SegmentMetadata::default);
            let key = tid.to_string();
            if let Some(s) = seg.samples.get(&key) {
                g.samples.insert(tid, s.into());
            }
            if let Some(&bs) = seg.track_byte_sizes.get(&key) {
                g.track_byte_sizes.insert(tid, bs);
            }
            if let Some(&dt) = seg.first_decode_times.get(&key) {
                g.first_decode_times.insert(tid, dt);
            }
            cur_tids.insert(tid);
        }
    }
    if let Some(g) = cur.take() {
        gops.push(g);
    }

    let bytes = build_synth_flat_header(&catalog, &gops)?;
    Ok(FlatHeader {
        bytes,
        total_body: running_body,
        segments: layouts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        let p = PathBuf::from(format!("samples/fixtures/{name}"));
        if p.exists() {
            p
        } else {
            PathBuf::from(format!("samples/{name}"))
        }
    }

    /// Convert a fixture (flat or fragmented MP4) to a MUXL flat MP4 — a valid
    /// wrapper we can unwrap into canonical segments. Doubles as the byte-exact
    /// oracle: its header comes from `build_synth_flat_header` and its body is
    /// the canonical segments verbatim, so the metafile path must reproduce it.
    fn flat_wrapper(fixture: &str) -> Vec<u8> {
        let input = crate::io::FileReadAt::open(&fixture_path(fixture)).unwrap();
        let mut out = Vec::new();
        crate::flat::to_flat(&input, &mut out).unwrap();
        out
    }

    /// The metafile path must reproduce a flat MP4 byte-for-byte: synthesize a
    /// header from per-segment metafiles, concatenate the verbatim segment
    /// bodies, and get exactly the direct flat writer's output.
    fn assert_metafile_synth_matches_direct_flat(fixture: &str) {
        let flat = flat_wrapper(fixture);
        let segs = reader::unwrap(&flat).unwrap();

        let metas: Vec<MetafileSegment> =
            segs.iter().map(|s| segment_metafile(s.data).unwrap()).collect();
        let header = synthesize_flat_header(&metas).unwrap();

        // [header][verbatim segment bodies, in order] == the flat MP4.
        let mut assembled = header.bytes.clone();
        for s in &segs {
            assembled.extend_from_slice(s.data);
        }
        assert_eq!(
            assembled, flat,
            "metafile-synthesized flat MP4 must be byte-identical to the direct flat writer ({fixture})"
        );

        // Layout sanity: offsets are contiguous and total matches the body.
        let body: u64 = segs.iter().map(|s| s.data.len() as u64).sum();
        assert_eq!(header.total_body, body);
        assert_eq!(header.segments.len(), segs.len());
        let mut expect = 0u64;
        for (lay, s) in header.segments.iter().zip(&segs) {
            assert_eq!(lay.body_offset, expect);
            assert_eq!(lay.body_size, s.data.len() as u64);
            expect += s.data.len() as u64;
        }
    }

    #[test]
    fn metafile_synth_matches_direct_flat_h264_aac() {
        assert_metafile_synth_matches_direct_flat("h264-aac.mp4");
    }

    #[test]
    fn metafile_synth_matches_direct_flat_h264_opus_frag() {
        assert_metafile_synth_matches_direct_flat("h264-opus-frag.mp4");
    }

    /// Full DRISL round-trip: encode the self-contained segment metafiles,
    /// decode them back (the synth CLI's path), and confirm the synth from the
    /// decoded values byte-matches the synth from the in-memory values. Proves
    /// the wire format carries everything (incl. the catalog) and decodes.
    #[test]
    fn metafile_drisl_roundtrip_matches() {
        let flat = flat_wrapper("h264-aac.mp4");
        let segs = reader::unwrap(&flat).unwrap();
        let metas: Vec<MetafileSegment> =
            segs.iter().map(|s| segment_metafile(s.data).unwrap()).collect();
        let direct = synthesize_flat_header(&metas).unwrap();

        // Encode the segment metafiles back to back.
        let mut wire = Vec::new();
        for m in &metas {
            dasl::drisl::to_writer(&mut wire, m).unwrap();
        }
        // Decode the uniform stream until EOF.
        let mut cur = Cursor::new(&wire[..]);
        let mut decoded: Vec<MetafileSegment> = Vec::new();
        while (cur.position() as usize) < wire.len() {
            decoded.push(dasl::drisl::de::from_reader_once(&mut cur).unwrap());
        }
        assert_eq!(decoded.len(), metas.len());
        assert!(decoded.iter().all(|m| m.kind == "segment" && m.version == METAFILE_VERSION));

        let via_wire = synthesize_flat_header(&decoded).unwrap();
        assert_eq!(via_wire.bytes, direct.bytes);
        assert_eq!(via_wire.total_body, direct.total_body);
        assert_eq!(via_wire.segments, direct.segments);
    }

    /// The streaming emitter yields the same self-contained metafiles as
    /// mapping `segment_metafile` over `unwrap`.
    #[test]
    fn metafiles_stream_matches_unwrap() {
        let flat = flat_wrapper("h264-opus-frag.mp4");
        let segs = reader::unwrap(&flat).unwrap();
        let expected: Vec<Vec<u8>> = segs
            .iter()
            .map(|s| dasl::drisl::to_vec(&segment_metafile(s.data).unwrap()).unwrap())
            .collect();

        let mut got: Vec<Vec<u8>> = Vec::new();
        metafiles_stream(Cursor::new(&flat), |m| {
            got.push(dasl::drisl::to_vec(&m).unwrap());
            Ok(())
        })
        .unwrap();
        assert_eq!(got, expected);
    }
}
