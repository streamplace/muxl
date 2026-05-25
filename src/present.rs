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
//! - [`write_flat_from_m4s`] — finalize a flat MP4 from raw segment bytes
//!   (e.g. `unwrap`ped from any wrapper), parsing the metadata back out — the
//!   fully fast-forward, m4s-native path.
//! - [`flat_header`] — catalog + per-segment metadata → flat MP4 header bytes
//!   (no sample bytes required; for byte-free assembly, e.g. S3 multipart).
//!
//! All three flat paths and the Source-based [`crate::flat::write`] produce
//! byte-identical output for the same content (enforced by tests).

use std::collections::BTreeMap;
use std::io::{Cursor, Write};

use mp4_atom::{Header, Moof, ReadAtom, ReadFrom};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::fragment::{TrackDefaults, resolve_sample};
use crate::reader::read_box_header;
use crate::segment::{GopSegment, TrackSamples};

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

/// Finalize a flat MP4 from raw canonical segment bytes — e.g. the verbatim
/// slices [`crate::reader::unwrap`] yields from any wrapper — by parsing each
/// segment's per-sample metadata rather than relying on segmenter-supplied
/// metadata. This is the fully fast-forward, m4s-native flat writer (goals #2
/// and #3): no `Source`/sample-plan, no moov parse.
///
/// `segments` must be in interleave order (per GoP, tracks ascending), which
/// is exactly what `unwrap` produces. The presentation anchor (`elst`) is
/// derived from the first GoP's segment `tfdt`s. Byte-identical to
/// [`crate::flat::write`] for the same content.
pub fn write_flat_from_m4s<W: Write>(
    catalog: &Catalog,
    segments: &[&[u8]],
    out: &mut W,
) -> Result<()> {
    // Parse + regroup into GoPs: a new GoP begins whenever a track id is <=
    // the previous one (tracks ascend within a GoP; a single-track stream
    // makes every segment its own GoP).
    let mut metas: Vec<SegmentMetadata> = Vec::new();
    let mut first_decode_times: BTreeMap<u32, u64> = BTreeMap::new();
    let mut last_tid: Option<u32> = None;
    for seg in segments {
        let (tid, ts, dts) = segment_index(seg)?;
        if last_tid.is_none_or(|prev| tid <= prev) {
            metas.push(SegmentMetadata::default());
        }
        let meta = metas.last_mut().unwrap();
        meta.track_byte_sizes.insert(tid, seg.len() as u64);
        meta.samples.insert(tid, ts);
        if metas.len() == 1 {
            first_decode_times.entry(tid).or_insert(dts);
        }
        last_tid = Some(tid);
    }
    if let Some(first) = metas.first_mut() {
        first.first_decode_times = first_decode_times;
    }

    out.write_all(&flat_header(catalog, &metas)?)?;
    for seg in segments {
        out.write_all(seg)?;
    }
    Ok(())
}

/// Parse one canonical segment's per-sample metadata by walking its inner
/// moofs (skipping the leading uuid prefix and any c2pa prefix). Returns
/// `(track_id, per-sample arrays, first decode time)`. Offsets are relative to
/// the segment start, matching `segment::TrackSamples::offsets_in_track`.
fn segment_index(data: &[u8]) -> Result<(u32, TrackSamples, u64)> {
    let zero = TrackDefaults {
        default_sample_duration: 0,
        default_sample_size: 0,
        default_sample_flags: 0,
    };
    let mut ts = TrackSamples::default();
    let mut track_id = 0u32;
    let mut first_dts = 0u64;
    let mut seen = false;
    let mut idx = 0u32;
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let (kind, _body, box_end) = read_box_header(data, pos)?;
        if &kind == b"moof" {
            let moof_size = box_end - pos;
            let mut cur = Cursor::new(&data[pos..box_end]);
            let h = <Option<Header> as ReadFrom>::read_from(&mut cur)
                .map_err(mp4e)?
                .ok_or_else(|| Error::InvalidMp4("empty moof header".into()))?;
            let moof = Moof::read_atom(&h, &mut cur).map_err(mp4e)?;
            let traf = moof
                .traf
                .first()
                .ok_or_else(|| Error::InvalidMp4("moof without traf".into()))?;
            track_id = traf.tfhd.track_id;
            if !seen {
                first_dts = traf.tfdt.as_ref().map_or(0, |t| t.base_media_decode_time);
                seen = true;
            }
            let entry = traf
                .trun
                .first()
                .and_then(|t| t.entries.first())
                .ok_or_else(|| Error::InvalidMp4("moof without trun entry".into()))?;
            let frame = resolve_sample(entry, &traf.tfhd, &zero, true, None);
            idx += 1;
            ts.durations.push(frame.duration);
            ts.sizes.push(frame.size);
            ts.cts_offsets.push(frame.cts_offset);
            if frame.is_sync {
                ts.sync_indices.push(idx);
            }
            ts.offsets_in_track.push((pos + moof_size + 8) as u64);
        }
        pos = box_end;
    }
    if !seen {
        return Err(Error::InvalidMp4("segment contains no moof".into()));
    }
    Ok((track_id, ts, first_dts))
}

fn mp4e(e: mp4_atom::Error) -> Error {
    Error::InvalidMp4(e.to_string())
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

    #[test]
    fn write_flat_from_m4s_matches_source_based_flat_writer() {
        // Fully fast-forward path: unwrap a reference flat MP4 to verbatim
        // segments, parse their metadata, and rebuild — must be byte-identical
        // to the original flat::write output (no Source, no moov parse).
        let data = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&data).unwrap();
        let mut flat_ref = Vec::new();
        crate::flat::write(&source, &data, &mut flat_ref).unwrap();

        let segs = reader::unwrap(&flat_ref).unwrap();
        let catalog = reader::aggregate_catalog(&segs);
        let slices: Vec<&[u8]> = segs.iter().map(|s| s.data).collect();

        let mut flat_mine = Vec::new();
        write_flat_from_m4s(&catalog, &slices, &mut flat_mine).unwrap();

        assert_eq!(
            flat_mine, flat_ref,
            "write_flat_from_m4s must match flat::write byte-for-byte"
        );
    }
}
