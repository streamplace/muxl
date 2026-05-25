//! `unwrap` — recover canonical MUXL segments from any storage wrapper.
//!
//! The inverse of the presentation/`wrap` layer. Given a bare m4s stream, a
//! MUXL fMP4 (`ftyp+moov+segments`), or a flat MP4 (`ftyp+moov+mdat`
//! envelope), [`unwrap`] fast-forwards over the container framing — *without*
//! parsing the `moov` or any sample table — and splits the canonical-segment
//! stream on `uuid` boundaries.
//!
//! The returned [`Segment::data`] slices are verbatim sub-slices of the
//! input: a hash or signature taken over a canonical segment survives the
//! round trip through any storage format unchanged (spec
//! `canonical-form.md § Round-Trip Property`). This is the goal-#3 consumer
//! primitive — `concat` and the signing layer are byte-passthrough on top of
//! it rather than re-minting fragments.

use crate::catalog::{self, Catalog};
use crate::error::{Error, Result};
use crate::segment::MUXL_UUID;

/// One canonical segment recovered from a storage wrapper.
pub struct Segment<'a> {
    /// Track this segment belongs to (from its catalog's CMAF container).
    pub track_id: u32,
    /// The single-track catalog carried in the segment's MUXL `uuid` box.
    pub catalog: Catalog,
    /// Verbatim segment bytes: `[uuid-c2pa?][uuid-muxl][moof][mdat]…`.
    pub data: &'a [u8],
}

/// Recover the canonical MUXL segments from `bytes`.
///
/// Accepts a bare m4s stream, a MUXL fMP4, or a flat MP4 interchangeably; the
/// container header is fast-forwarded over and the segment stream is split on
/// MUXL `uuid` boundaries. A leading C2PA/S2PA `uuid` box (signed segments)
/// stays attached to the segment it prefixes.
pub fn unwrap(bytes: &[u8]) -> Result<Vec<Segment<'_>>> {
    let stream = locate_segment_stream(bytes)?;
    scan_segments(stream)
}

/// Locate the contiguous canonical-segment byte range inside a storage
/// wrapper: skip a leading `ftyp`/`moov` (and other framing), and descend
/// into a flat MP4's outer `mdat` envelope whose payload *is* the segment
/// stream. A bare segment stream is returned unchanged.
fn locate_segment_stream(bytes: &[u8]) -> Result<&[u8]> {
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let (kind, body_start, box_end) = read_box_header(bytes, pos)?;
        match &kind {
            // Container framing — skip past it.
            b"ftyp" | b"moov" | b"free" | b"skip" | b"styp" | b"sidx" => pos = box_end,
            // Flat MP4 outer envelope: its payload is the verbatim segment stream.
            b"mdat" => return Ok(&bytes[body_start..box_end]),
            // Start of a bare / fMP4 segment stream.
            b"uuid" | b"moof" => return Ok(&bytes[pos..]),
            // Unknown leading box — skip conservatively.
            _ => pos = box_end,
        }
    }
    // No recognizable framing: treat the whole input as the segment stream.
    Ok(bytes)
}

/// Split a segment stream on MUXL `uuid` boundaries into verbatim segments.
fn scan_segments(stream: &[u8]) -> Result<Vec<Segment<'_>>> {
    let mut segs = Vec::new();
    let mut seg_start: Option<usize> = None;
    let mut seg_has_muxl = false;
    let mut pos = 0usize;

    while pos + 8 <= stream.len() {
        let (kind, body_start, box_end) = read_box_header(stream, pos)?;
        if &kind == b"uuid" {
            let is_muxl = body_start + 16 <= box_end && stream[body_start..body_start + 16] == MUXL_UUID;
            // A uuid box opens a new segment when either: it's a second muxl
            // uuid (the previous segment's fragments are complete), or it's a
            // non-muxl (c2pa) uuid, which always heads a fresh segment's
            // signing prefix.
            let boundary = seg_start.is_some() && (!is_muxl || seg_has_muxl);
            if boundary {
                push_segment(&mut segs, stream, seg_start.unwrap(), pos)?;
                seg_start = None;
                seg_has_muxl = false;
            }
            seg_start.get_or_insert(pos);
            seg_has_muxl |= is_muxl;
        } else if seg_start.is_none() {
            return Err(Error::InvalidMp4(
                "segment stream did not begin with a uuid box".into(),
            ));
        }
        pos = box_end;
    }
    if let Some(start) = seg_start {
        push_segment(&mut segs, stream, start, stream.len())?;
    }
    Ok(segs)
}

fn push_segment<'a>(
    out: &mut Vec<Segment<'a>>,
    stream: &'a [u8],
    start: usize,
    end: usize,
) -> Result<()> {
    let data = &stream[start..end];
    let catalog = catalog::from_segment(data)?;
    let track_id = catalog
        .video_configs()
        .map(|v| v.track_id())
        .chain(catalog.audio_configs().map(|a| a.track_id()))
        .next()
        .ok_or_else(|| Error::InvalidMp4("segment catalog describes no track".into()))?;
    out.push(Segment {
        track_id,
        catalog,
        data,
    });
    Ok(())
}

/// Read an ISOBMFF box header at `pos`, returning `(fourcc, body_start,
/// box_end)`. Handles the 32-bit, 64-bit-`largesize` (size==1), and
/// to-EOF (size==0) forms.
fn read_box_header(bytes: &[u8], pos: usize) -> Result<([u8; 4], usize, usize)> {
    if pos + 8 > bytes.len() {
        return Err(Error::InvalidMp4("truncated box header".into()));
    }
    let size32 = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&bytes[pos + 4..pos + 8]);
    let bad = || Error::InvalidMp4("box size out of range".into());
    let (body_start, box_end) = match size32 {
        1 => {
            if pos + 16 > bytes.len() {
                return Err(Error::InvalidMp4("truncated 64-bit box header".into()));
            }
            let large = u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize;
            let end = large
                .checked_add(pos)
                .filter(|&e| e >= pos + 16 && e <= bytes.len())
                .ok_or_else(bad)?;
            (pos + 16, end)
        }
        0 => (pos + 8, bytes.len()),
        n => {
            let end = (n as usize)
                .checked_add(pos)
                .filter(|&e| e >= pos + 8 && e <= bytes.len())
                .ok_or_else(bad)?;
            (pos + 8, end)
        }
    };
    Ok((kind, body_start, box_end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::build_init_segment;
    use std::io::Cursor;

    fn read_fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("samples/fixtures/{name}"))
            .unwrap_or_else(|_| panic!("samples/fixtures/{name} must exist for tests"))
    }

    /// Segment a fixture, then re-assemble the canonical segments in
    /// interleave order (per GoP, tracks ascending) — the same layout the
    /// fMP4/flat writers and `cmd_segment_fmp4` use.
    fn segments_in_order(data: &[u8]) -> (Catalog, Vec<(u32, Vec<u8>)>) {
        let mut gops = Vec::new();
        let catalog = crate::segment::segment_fmp4(&mut Cursor::new(data), |gop| {
            gops.push(gop);
            Ok(())
        })
        .unwrap();
        let mut ordered = Vec::new();
        for gop in &gops {
            for (&tid, bytes) in &gop.tracks {
                ordered.push((tid, bytes.clone()));
            }
        }
        (catalog, ordered)
    }

    #[test]
    fn unwrap_fmp4_recovers_segments_verbatim() {
        let data = read_fixture("h264-opus-frag.mp4");
        let (catalog, ordered) = segments_in_order(&data);

        // Build a MUXL fMP4: init + canonical segments in interleave order.
        let mut fmp4 = build_init_segment(&catalog).unwrap();
        for (_, seg) in &ordered {
            fmp4.extend_from_slice(seg);
        }

        let recovered = unwrap(&fmp4).unwrap();
        assert_eq!(recovered.len(), ordered.len(), "segment count mismatch");
        for (rec, (tid, seg)) in recovered.iter().zip(ordered.iter()) {
            assert_eq!(rec.track_id, *tid, "track id mismatch");
            assert_eq!(rec.data, seg.as_slice(), "segment bytes must be verbatim");
        }
    }

    #[test]
    fn unwrap_bare_m4s_stream_recovers_segments() {
        let data = read_fixture("h264-opus-frag.mp4");
        let (_catalog, ordered) = segments_in_order(&data);

        // No ftyp/moov framing at all — just concatenated canonical segments.
        let mut stream = Vec::new();
        for (_, seg) in &ordered {
            stream.extend_from_slice(seg);
        }

        let recovered = unwrap(&stream).unwrap();
        assert_eq!(recovered.len(), ordered.len());
        for (rec, (tid, seg)) in recovered.iter().zip(ordered.iter()) {
            assert_eq!(rec.track_id, *tid);
            assert_eq!(rec.data, seg.as_slice());
        }
    }

    #[test]
    fn unwrap_flat_mp4_descends_envelope() {
        // A flat MP4's outer mdat payload is the verbatim segment stream;
        // unwrapping it then concatenating must reproduce the payload exactly.
        let data = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&data).unwrap();
        let mut flat = Vec::new();
        let info = crate::flat::write(&source, &data, &mut flat).unwrap();

        let recovered = unwrap(&flat).unwrap();
        assert!(!recovered.is_empty(), "should recover segments from flat MP4");

        let envelope = &flat[info.mdat_payload_offset as usize..];
        let joined: Vec<u8> = recovered.iter().flat_map(|s| s.data.iter().copied()).collect();
        assert_eq!(joined.as_slice(), envelope, "segments must reconstruct the mdat envelope verbatim");

        // Each recovered segment is itself a valid canonical segment.
        for s in &recovered {
            assert_eq!(catalog::from_segment(s.data).unwrap(), s.catalog);
        }
    }

    #[test]
    fn unwrap_keeps_c2pa_prefix_with_segment() {
        // Simulate a signed stream: prepend a fake c2pa uuid box to each
        // canonical segment. unwrap must keep [c2pa][muxl][frags] together.
        let data = read_fixture("h264-opus-frag.mp4");
        let (_catalog, ordered) = segments_in_order(&data);

        let fake_c2pa = |body: &[u8]| {
            let total = 24 + body.len();
            let mut b = Vec::new();
            b.extend_from_slice(&(total as u32).to_be_bytes());
            b.extend_from_slice(b"uuid");
            b.extend_from_slice(&[0xd8u8; 16]); // non-MUXL usertype
            b.extend_from_slice(body);
            b
        };

        let mut stream = Vec::new();
        let mut signed = Vec::new();
        for (i, (_, seg)) in ordered.iter().enumerate() {
            let prefix = fake_c2pa(format!("manifest {i}").as_bytes());
            let mut s = prefix.clone();
            s.extend_from_slice(seg);
            stream.extend_from_slice(&s);
            signed.push(s);
        }

        let recovered = unwrap(&stream).unwrap();
        assert_eq!(recovered.len(), ordered.len());
        for (rec, expected) in recovered.iter().zip(signed.iter()) {
            assert_eq!(rec.data, expected.as_slice(), "c2pa prefix must stay with its segment");
            assert!(rec.data.starts_with(&[0, 0, 0]), "segment should start with the c2pa uuid box");
        }
    }

    #[test]
    fn unwrap_rejects_non_segment_stream() {
        // A moof with no preceding uuid is not a canonical segment stream.
        let bytes = b"\x00\x00\x00\x08moof".to_vec();
        assert!(unwrap(&bytes).is_err());
    }
}
