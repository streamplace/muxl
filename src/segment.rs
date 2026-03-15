//! MUXL segmentation: fMP4 → GOP-aligned MUXL segments.
//!
//! A MUXL segment is a concatenation of per-frame moof+mdat pairs covering
//! one video GOP (keyframe to keyframe). Audio frames for the same time range
//! are included in the same segment.
//!
//! Spec: architecture.md § MUXL Segment

use std::collections::HashSet;
use std::io::Read;

use crate::catalog::Catalog;
use crate::error::Result;
use crate::fragment::FMP4Reader;

/// A GOP-aligned MUXL segment.
pub struct Segment {
    /// Segment number (1-based).
    pub number: u32,
    /// Concatenated moof+mdat pairs for all frames in this GOP.
    pub data: Vec<u8>,
}

/// Segment an fMP4 stream into GOP-aligned MUXL segments.
///
/// Only requires `Read` — no seeking. Splits at video keyframe boundaries:
/// a new segment starts when a sync (key) frame arrives on any video track,
/// after the first one.
///
/// Calls `on_segment` for each completed segment. Returns the catalog
/// extracted from the init segment. Use `build_init_segment(&catalog)` to
/// produce the canonical init segment bytes.
pub fn segment_fmp4<R: Read>(
    reader: &mut R,
    mut on_segment: impl FnMut(Segment) -> Result<()>,
) -> Result<Catalog> {
    let mut fmp4 = FMP4Reader::new(reader)?;
    let catalog = fmp4.catalog().clone();

    // Determine which track IDs are video (for keyframe detection)
    let video_track_ids: HashSet<u32> = catalog.video.values().map(|v| v.track_id).collect();

    let mut segment_buf: Vec<u8> = Vec::new();
    let mut segment_number: u32 = 0;
    let mut seen_first_keyframe = false;

    while let Some(frame) = fmp4.next_frame()? {
        let is_video_keyframe = video_track_ids.contains(&frame.track_id) && frame.is_sync;

        if is_video_keyframe && seen_first_keyframe {
            // Flush the current segment
            segment_number += 1;
            on_segment(Segment {
                number: segment_number,
                data: std::mem::take(&mut segment_buf),
            })?;
        }

        if is_video_keyframe {
            seen_first_keyframe = true;
        }

        segment_buf.extend_from_slice(&frame.data);
    }

    // Flush the final segment (if any data remains)
    if !segment_buf.is_empty() {
        segment_number += 1;
        on_segment(Segment {
            number: segment_number,
            data: segment_buf,
        })?;
    }

    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::build_init_segment;
    use std::io::Cursor;

    fn read_fixture(name: &str) -> Vec<u8> {
        let path = format!("samples/fixtures/{}", name);
        std::fs::read(&path)
            .or_else(|_| std::fs::read(format!("samples/{}", name)))
            .unwrap_or_else(|_| panic!("{} must exist for tests", path))
    }

    #[test]
    fn test_segment_fmp4_produces_segments() {
        let data = read_fixture("h264-opus-frag.mp4");
        let mut segments = Vec::new();
        let catalog = segment_fmp4(&mut Cursor::new(&data), |seg| {
            segments.push(seg);
            Ok(())
        })
        .unwrap();

        assert!(!catalog.video.is_empty(), "should have video tracks");
        assert!(!segments.is_empty(), "should produce at least one segment");

        // Segments should be numbered sequentially
        for (i, seg) in segments.iter().enumerate() {
            assert_eq!(seg.number, (i + 1) as u32);
            assert!(
                !seg.data.is_empty(),
                "segment {} should have data",
                seg.number
            );
        }
    }

    #[test]
    fn test_segment_data_is_all_frame_data() {
        // Total bytes across all segments should equal total bytes across all frames
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segment_total = 0usize;
        segment_fmp4(&mut Cursor::new(&data), |seg| {
            segment_total += seg.data.len();
            Ok(())
        })
        .unwrap();

        let mut frame_total = 0usize;
        crate::fragment::fragment_fmp4(&mut Cursor::new(&data), |frame| {
            frame_total += frame.data.len();
            Ok(())
        })
        .unwrap();

        assert_eq!(
            segment_total, frame_total,
            "segment bytes should equal total frame bytes"
        );
    }

    #[test]
    fn test_archive_equals_init_plus_segments() {
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segments = Vec::new();
        let catalog = segment_fmp4(&mut Cursor::new(&data), |seg| {
            segments.push(seg);
            Ok(())
        })
        .unwrap();

        let init = build_init_segment(&catalog).unwrap();

        // Build archive: init + all segments concatenated
        let mut archive = init.clone();
        for seg in &segments {
            archive.extend_from_slice(&seg.data);
        }

        // The archive should be parseable as fMP4
        let archive_catalog = crate::catalog_from_mp4(Cursor::new(&archive)).unwrap();
        assert_eq!(catalog.video.len(), archive_catalog.video.len());
        assert_eq!(catalog.audio.len(), archive_catalog.audio.len());

        // And re-segmenting the archive should produce the same number of segments
        let mut re_segments = Vec::new();
        segment_fmp4(&mut Cursor::new(&archive), |seg| {
            re_segments.push(seg);
            Ok(())
        })
        .unwrap();

        assert_eq!(
            segments.len(),
            re_segments.len(),
            "re-segmenting archive should produce same segment count"
        );
    }

    #[test]
    fn test_segments_start_with_keyframe() {
        // Each segment's first video frame should be a sync frame.
        // We can verify by re-fragmenting each segment and checking.
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segments = Vec::new();
        let catalog = segment_fmp4(&mut Cursor::new(&data), |seg| {
            segments.push(seg);
            Ok(())
        })
        .unwrap();

        let video_track_ids: HashSet<u32> = catalog.video.values().map(|v| v.track_id).collect();

        for seg in &segments {
            // Parse the segment's moof+mdat pairs to find the first video frame
            use mp4_atom::{Atom, Header, Moof, ReadAtom, ReadFrom};

            let mut cursor = Cursor::new(&seg.data);
            let mut found_video = false;

            while cursor.position() < seg.data.len() as u64 {
                let h = match <Option<Header> as ReadFrom>::read_from(&mut cursor) {
                    Ok(Some(h)) => h,
                    _ => break,
                };

                if h.kind == Moof::KIND {
                    let moof = Moof::read_atom(&h, &mut cursor).unwrap();
                    for traf in &moof.traf {
                        if video_track_ids.contains(&traf.tfhd.track_id) && !found_video {
                            // First video frame in this segment — check sync flag
                            let entry = &traf.trun[0].entries[0];
                            let flags = entry.flags.unwrap_or(0);
                            let is_sync = (flags & 0x00010000) == 0;
                            assert!(
                                is_sync,
                                "segment {} first video frame should be sync",
                                seg.number
                            );
                            found_video = true;
                        }
                    }
                } else {
                    // Skip mdat
                    let size = h.size.unwrap_or(0);
                    cursor.set_position(cursor.position() + size as u64);
                }
            }
        }
    }
}
