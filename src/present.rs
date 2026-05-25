//! `wrap` — synthesize a presentation MP4 over verbatim MUXL segments.
//!
//! MUXL canonical segments are the content-addressed truth; fMP4 and flat
//! MP4 are *presentation* wrappers that prepend a synthesized ISOBMFF header
//! derived from the catalog and leave the segment bytes untouched (spec
//! `canonical-form.md § Synthesized Storage Formats`). This module is the
//! `wrap` side; [`crate::reader`] (`unwrap`) is the inverse.
//!
//! - [`init`] / [`init_per_track`] — catalog → fMP4 init segment (`ftyp+moov`).
//! - [`write_fmp4`] — init + verbatim segments = an appendable MUXL fMP4.
//! - [`write_flat`] — finalize a flat MP4 from GoP segments that carry their
//!   per-sample metadata (e.g. straight from the segmenter).
//! - [`flat_header`] — catalog + per-segment metadata → flat MP4 header bytes
//!   (no sample bytes required; for byte-free assembly, e.g. S3 multipart).
//!
//! Deriving the per-sample metadata from raw m4s bytes alone (so a stream of
//! `unwrap`ped segments could be flattened without their segmenter metadata)
//! is still TODO; the Source-based [`crate::flat::write`] remains for
//! whole-file flattening in the meantime.

use std::collections::BTreeMap;
use std::io::Write;

use crate::catalog::Catalog;
use crate::error::Result;
use crate::segment::GopSegment;

pub use crate::flat::SegmentMetadata;

/// Build the canonical fMP4 init segment (`ftyp+moov`, empty sample tables,
/// `mvex` present) for `catalog`. Spec § Init Segment moov.
pub fn init(catalog: &Catalog) -> Result<Vec<u8>> {
    crate::init::build_init_segment(catalog)
}

/// Per-track init segments keyed by `track_id` — one single-track `ftyp+moov`
/// each, for HLS CMAF `#EXT-X-MAP` where every rendition needs its own init.
pub fn init_per_track(catalog: &Catalog) -> Result<BTreeMap<u32, Vec<u8>>> {
    crate::init::build_track_init_segments(catalog)
}

/// Synthesize a flat MP4 header (`ftyp + moov + mdat-envelope-header`) from
/// the catalog and per-segment metadata, with no sample bytes. The caller
/// appends each segment's verbatim body after the header (e.g. via S3
/// multipart copy from per-segment objects). Spec § Header Synthesis.
pub fn flat_header(catalog: &Catalog, segments: &[SegmentMetadata]) -> Result<Vec<u8>> {
    crate::flat::build_synth_flat_header(catalog, segments)
}

/// Write an appendable MUXL fMP4: the init segment followed by `segments`
/// verbatim, in the order given (interleave order — per GoP, tracks
/// ascending). Segment bytes are not parsed or rewritten, so any
/// hash/signature over them is preserved. This is the live/appendable
/// presentation format: further segments can be byte-appended after the
/// header with no rewrite, and the file is valid at every moment.
pub fn write_fmp4<'a, W: Write>(
    catalog: &Catalog,
    segments: impl IntoIterator<Item = &'a [u8]>,
    out: &mut W,
) -> Result<()> {
    out.write_all(&init(catalog)?)?;
    for seg in segments {
        out.write_all(seg)?;
    }
    Ok(())
}

/// Finalize a flat MP4 from GoP segments. Each [`GopSegment`] carries its
/// per-track bytes and the per-sample metadata the segmenter already
/// computed, so the synthesized `moov` (populated `stbl` + `co64`) is built
/// without re-parsing the segment bytes. The body is the GoP segments written
/// verbatim in interleave order (per GoP, tracks ascending) inside the outer
/// `mdat` envelope.
///
/// `first_decode_times` is the presentation anchor (per track media ticks) for
/// the first GoP — used to synthesize an `elst` when a VOD starts mid-stream;
/// pass an empty map for a stream that starts at time zero.
///
/// This is the segment-native equivalent of [`crate::flat::write`] and
/// produces byte-identical output for the same content.
pub fn write_flat<W: Write>(
    catalog: &Catalog,
    gops: &[GopSegment],
    first_decode_times: &BTreeMap<u32, u64>,
    out: &mut W,
) -> Result<()> {
    let mut metas: Vec<SegmentMetadata> = Vec::with_capacity(gops.len());
    for (gi, gop) in gops.iter().enumerate() {
        let mut meta = SegmentMetadata::default();
        for (&tid, bytes) in &gop.tracks {
            meta.track_byte_sizes.insert(tid, bytes.len() as u64);
            if let Some(ts) = gop.samples.get(&tid) {
                meta.samples.insert(tid, ts.clone());
            }
        }
        if gi == 0 {
            meta.first_decode_times = first_decode_times.clone();
        }
        metas.push(meta);
    }

    out.write_all(&flat_header(catalog, &metas)?)?;
    // Body: GoP segments verbatim, tracks ascending within each GoP (BTreeMap
    // iteration order), matching the co64 offsets the header was built from.
    for gop in gops {
        for bytes in gop.tracks.values() {
            out.write_all(bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader;
    use std::io::Cursor;

    fn read_fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("samples/fixtures/{name}"))
            .unwrap_or_else(|_| panic!("samples/fixtures/{name} must exist"))
    }

    #[test]
    fn write_fmp4_then_unwrap_round_trips() {
        // Segment a fixture, wrap the canonical segments into an fMP4 via
        // present::write_fmp4, then unwrap it: the recovered segment bytes
        // must be byte-identical (wrap/unwrap are exact inverses).
        let data = read_fixture("h264-opus-frag.mp4");
        let mut gops = Vec::new();
        let catalog = crate::segment::segment_fmp4(&mut Cursor::new(&data), |gop| {
            gops.push(gop);
            Ok(())
        })
        .unwrap();

        let ordered: Vec<Vec<u8>> = gops
            .iter()
            .flat_map(|g| g.tracks.values().cloned())
            .collect();

        let mut fmp4 = Vec::new();
        write_fmp4(&catalog, ordered.iter().map(|v| v.as_slice()), &mut fmp4).unwrap();

        // The init must be exactly the canonical init segment.
        assert!(fmp4.starts_with(&init(&catalog).unwrap()));

        let recovered = reader::unwrap(&fmp4).unwrap();
        assert_eq!(recovered.len(), ordered.len());
        for (rec, seg) in recovered.iter().zip(ordered.iter()) {
            assert_eq!(rec.data, seg.as_slice(), "segment bytes must survive wrap→unwrap");
        }
    }

    #[test]
    fn write_flat_matches_source_based_flat_writer() {
        // Oracle: present::write_flat (segment-native) must produce a flat MP4
        // byte-identical to the Source-based flat::write for the same content.
        let data = read_fixture("h264-opus-frag.mp4");

        let source = crate::read(&data).unwrap();
        let mut flat_ref = Vec::new();
        crate::flat::write(&source, &data, &mut flat_ref).unwrap();

        // Presentation anchor per track (start_offset_ticks), so the
        // synthesized elst matches the reference writer's.
        let fdt: std::collections::BTreeMap<u32, u64> = source
            .plan
            .tracks
            .iter()
            .map(|t| (t.track_id, t.start_offset_ticks))
            .collect();

        let mut gops = Vec::new();
        let catalog = crate::segment::segment_fmp4(&mut Cursor::new(&data), |g| {
            gops.push(g);
            Ok(())
        })
        .unwrap();

        let mut flat_mine = Vec::new();
        write_flat(&catalog, &gops, &fdt, &mut flat_mine).unwrap();

        assert_eq!(
            flat_mine, flat_ref,
            "present::write_flat must match flat::write byte-for-byte"
        );
    }
}
