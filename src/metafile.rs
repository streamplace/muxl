//! Metafile: a payload-free, versioned view of canonical-segment metadata, and
//! on-demand synthesis of a flat-MP4 faststart header from an ordered set of
//! them — *without* the segment bytes.
//!
//! This is what makes "flat-MP4 VOD" content-addressable. A consumer archives
//! one small metafile per canonical segment (plus the init once), then
//! synthesizes a faststart header for any contiguous segment range on demand.
//! Serving `[header][canonical blob bytes for the range]` yields a
//! byte-range-seekable MP4 whose `moov` is exact for those bytes — no random
//! access over the blob, no re-muxing.
//!
//! The wire format is DRISL / dag-cbor: one [`MetafileInit`] (carrying the
//! catalog), then one [`MetafileSegment`] per canonical `.m4s` (per-track), in
//! canonical interleave order. The field names match the live [`CborEvent`]
//! stream exactly (a metafile is the payload-free subset — no `data`/`tracks`),
//! so the same consumer-side decoder reads both. They're plain structs rather
//! than a `#[serde(tag)]` enum so they decode cleanly under DRISL: a tagged
//! enum buffers each variant through an intermediate that mishandles the
//! catalog's byte fields, whereas a plain struct deserializes them directly
//! (the same path [`crate::catalog`] round-trips through).
//!
//! Offsets are owned entirely by muxl: the synthesized `moov`'s `co64` already
//! resolves to `header_len + body_offset + per_sample_offset`, so the caller
//! passes no base offset — it serves the header bytes immediately followed by
//! the segment bodies in input order. See `spec/canonical-form.md § Metafile`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::cbor::{CborTrackSamples, METAFILE_VERSION};
use crate::error::{Error, Result};
use crate::flat::{SegmentMetadata, build_synth_flat_header};

/// The one-time per-stream metafile: the catalog the synthesizer needs
/// (codecs, dimensions, timescales). DRISL key `type` = `"init"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetafileInit {
    /// Always `"init"` — present for wire symmetry with the [`CborEvent`]
    /// stream and consumer routing.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Wire-format version ([`METAFILE_VERSION`]).
    #[serde(default)]
    pub version: u16,
    /// Track configuration (codec/dimensions/timescale + decoder config bytes).
    pub catalog: Catalog,
}

/// One canonical segment's metadata, payload-free. DRISL key `type` =
/// `"segment"`. Maps are keyed by stringified track id. Typically single-track
/// (one canonical `.m4s`), but a pre-grouped multi-track GoP also works.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetafileSegment {
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Per-track per-sample tables (stsz/stts/ctts/stss + per-sample offsets).
    #[serde(default)]
    pub samples: BTreeMap<String, CborTrackSamples>,
    /// Per-track on-disk byte size of this segment's body contribution
    /// (uuid prefix + moof+mdat run, incl. any c2pa signature) — for `co64`.
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
    /// Per input segment event, in order: where its bytes land in the body.
    pub segments: Vec<SegmentLayout>,
}

/// Build the metafile event for ONE canonical segment (`.m4s`).
///
/// Carries the per-sample tables, the segment's on-disk byte size (for `co64`
/// placement), and the first sample's decode time (the `elst` anchor) — with
/// no payload. `byte_size`/offsets are taken from the bytes as given, so feed
/// the *final* stored bytes (post-signing) for an exact header.
pub fn segment_metafile(segment_bytes: &[u8]) -> Result<MetafileSegment> {
    let (tid, ts, first_dts) = crate::present::segment_index(segment_bytes)?;
    let key = tid.to_string();
    let dur_ticks: u64 = ts.durations.iter().map(|&d| d as u64).sum();
    let sample_count = ts.durations.len() as u32;
    let byte_size = segment_bytes.len() as u64;
    Ok(MetafileSegment {
        kind: "segment".into(),
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

/// Build the `init` metafile from a catalog.
pub fn init_metafile(catalog: &Catalog) -> MetafileInit {
    MetafileInit {
        kind: "init".into(),
        version: METAFILE_VERSION,
        catalog: catalog.clone(),
    }
}

/// Synthesize a flat-MP4 faststart header from a catalog + an ordered set of
/// per-canonical-segment metafiles (canonical interleave order).
///
/// Deterministic: identical input yields byte-identical `bytes`, so the result
/// can be content-addressed and cached. Segment metafiles are regrouped into
/// GoPs (a new GoP begins when a track id repeats) exactly as `unwrap` orders
/// them, then handed to [`build_synth_flat_header`].
pub fn synthesize_flat_header(
    init: &MetafileInit,
    segments: &[MetafileSegment],
) -> Result<FlatHeader> {
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

    if gops.is_empty() {
        return Err(Error::InvalidMp4("metafile stream has no segments".into()));
    }

    let bytes = build_synth_flat_header(&init.catalog, &gops)?;
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
    /// header from per-segment metafiles + the catalog, concatenate the
    /// verbatim segment bodies, and get exactly the direct flat writer's
    /// output. This is the content-addressing guarantee.
    fn assert_metafile_synth_matches_direct_flat(fixture: &str) {
        let flat = flat_wrapper(fixture);
        let segs = reader::unwrap(&flat).unwrap();
        let catalog = reader::aggregate_catalog(&segs);

        let init = init_metafile(&catalog);
        let metas: Vec<MetafileSegment> =
            segs.iter().map(|s| segment_metafile(s.data).unwrap()).collect();
        let header = synthesize_flat_header(&init, &metas).unwrap();

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

    /// Full DRISL round-trip: encode init + segment metafiles, decode them back
    /// positionally (the synth CLI's path), and confirm the synth from the
    /// decoded values byte-matches the synth from the in-memory values. Proves
    /// the wire format carries everything and decodes under DRISL.
    #[test]
    fn metafile_drisl_roundtrip_matches() {
        let flat = flat_wrapper("h264-aac.mp4");
        let segs = reader::unwrap(&flat).unwrap();
        let catalog = reader::aggregate_catalog(&segs);

        let init = init_metafile(&catalog);
        let metas: Vec<MetafileSegment> =
            segs.iter().map(|s| segment_metafile(s.data).unwrap()).collect();
        let direct = synthesize_flat_header(&init, &metas).unwrap();

        // Encode the whole stream: init first, then segments.
        let mut wire = Vec::new();
        dasl::drisl::to_writer(&mut wire, &init).unwrap();
        for m in &metas {
            dasl::drisl::to_writer(&mut wire, m).unwrap();
        }

        // Decode positionally: first value = init, rest = segments, until EOF.
        let mut cur = Cursor::new(&wire[..]);
        let decoded_init: MetafileInit = dasl::drisl::de::from_reader_once(&mut cur).unwrap();
        let mut decoded_segs: Vec<MetafileSegment> = Vec::new();
        while (cur.position() as usize) < wire.len() {
            decoded_segs.push(dasl::drisl::de::from_reader_once(&mut cur).unwrap());
        }
        assert_eq!(decoded_init.kind, "init");
        assert_eq!(decoded_init.version, METAFILE_VERSION);
        assert_eq!(decoded_segs.len(), metas.len());

        let via_wire = synthesize_flat_header(&decoded_init, &decoded_segs).unwrap();
        assert_eq!(via_wire.bytes, direct.bytes);
        assert_eq!(via_wire.total_body, direct.total_body);
        assert_eq!(via_wire.segments, direct.segments);
    }
}
