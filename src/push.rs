//! Push-based streaming segmenter.
//!
//! Accepts fMP4 data in arbitrary chunks via `feed()`, buffers internally,
//! and emits events (init segment, MUXL segments) as complete atoms arrive.
//! No blocking reads — suitable for WASM, async runtimes, or any push-based
//! data source.
//!
//! # Example
//! ```ignore
//! let mut segmenter = Segmenter::new();
//! for chunk in stream {
//!     for event in segmenter.feed(&chunk)? {
//!         match event {
//!             SegmenterEvent::InitSegment { catalog, data } => { /* send init */ }
//!             SegmenterEvent::Segment(seg) => { /* send segment */ }
//!         }
//!     }
//! }
//! for event in segmenter.flush()? {
//!     // handle final segment
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use mp4_atom::{Header, Moof, Moov, ReadAtom, ReadFrom};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::fragment::{self, Frame};
use crate::init::{build_init_segment, catalog_from_moov};
use crate::segment::Segment;

/// Events emitted by the push-based segmenter.
pub enum SegmenterEvent {
    /// The moov box has been parsed and a canonical init segment built.
    InitSegment {
        /// Track configuration metadata.
        catalog: Catalog,
        /// Canonical ftyp+moov bytes, ready to send to a decoder.
        data: Vec<u8>,
    },
    /// A complete MUXL segment (one GOP of interleaved per-frame moof+mdat).
    Segment(Segment),
}

/// Push-based fMP4 → MUXL segment streaming processor.
///
/// Feed arbitrary chunks of fMP4 data via `feed()`. The segmenter buffers
/// internally and emits [`SegmenterEvent`]s as complete MP4 atoms arrive.
/// Call `flush()` at end-of-stream to emit any remaining partial segment.
pub struct Segmenter {
    buffer: Vec<u8>,
    state: State,
}

enum State {
    /// Waiting for the moov box (skipping ftyp and other init boxes).
    WaitingForInit,
    /// Processing moof+mdat fragment pairs.
    Streaming(StreamingState),
}

struct StreamingState {
    moov: Moov,
    video_track_ids: HashSet<u32>,
    track_state: HashMap<u32, (u64, u32)>,
    segment_buf: Vec<u8>,
    segment_number: u32,
    seen_first_keyframe: bool,
    /// A parsed moof waiting for its following mdat.
    pending_moof: Option<PendingMoof>,
}

struct PendingMoof {
    moof: Moof,
    /// Total size of the moof box (header + body), needed for data_offset calculation.
    box_size: usize,
}

impl Segmenter {
    /// Create a new segmenter ready to receive fMP4 data.
    pub fn new() -> Self {
        Segmenter {
            buffer: Vec::new(),
            state: State::WaitingForInit,
        }
    }

    /// Feed a chunk of fMP4 data. Returns any events produced.
    ///
    /// Data can arrive in any chunk size — the segmenter buffers internally
    /// and only processes complete MP4 atoms.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<SegmenterEvent>> {
        self.buffer.extend_from_slice(data);
        let mut events = Vec::new();

        loop {
            let remaining = &self.buffer[..];

            // Peek at the next atom header
            let (box_total_size, box_type) = match peek_atom_header(remaining) {
                Some(v) => v,
                None => break,
            };

            // Wait until the complete box has arrived
            if remaining.len() < box_total_size {
                break;
            }

            let box_data = &remaining[..box_total_size];

            match &mut self.state {
                State::WaitingForInit => {
                    if box_type == *b"moov" {
                        let mut cursor = Cursor::new(box_data);
                        let header = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                            .map_err(mp4_err)?
                            .ok_or_else(|| Error::InvalidMp4("empty moov header".into()))?;
                        let moov = Moov::read_atom(&header, &mut cursor).map_err(mp4_err)?;
                        let catalog = catalog_from_moov(&moov)?;
                        let init_data = build_init_segment(&catalog)?;

                        let video_track_ids: HashSet<u32> =
                            catalog.video.values().map(|v| v.track_id).collect();

                        events.push(SegmenterEvent::InitSegment {
                            catalog,
                            data: init_data,
                        });

                        self.state = State::Streaming(StreamingState {
                            moov,
                            video_track_ids,
                            track_state: HashMap::new(),
                            segment_buf: Vec::new(),
                            segment_number: 0,
                            seen_first_keyframe: false,
                            pending_moof: None,
                        });
                    }
                    // Skip ftyp, free, etc.
                }
                State::Streaming(ss) => {
                    if box_type == *b"moof" {
                        let mut cursor = Cursor::new(box_data);
                        let header = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                            .map_err(mp4_err)?
                            .ok_or_else(|| Error::InvalidMp4("empty moof header".into()))?;
                        let moof = Moof::read_atom(&header, &mut cursor).map_err(mp4_err)?;
                        ss.pending_moof = Some(PendingMoof {
                            moof,
                            box_size: box_total_size,
                        });
                    } else if box_type == *b"mdat" {
                        if let Some(pending) = ss.pending_moof.take() {
                            // Determine mdat header size (8 for normal, 16 for extended)
                            let mdat_header_size = if box_total_size > u32::MAX as usize {
                                16
                            } else {
                                8
                            };
                            let mdat_payload = &box_data[mdat_header_size..];

                            // Process moof+mdat into per-frame fragments
                            let mut frames: Vec<Frame> = Vec::new();
                            fragment::process_moof_mdat(
                                &ss.moov,
                                &pending.moof,
                                pending.box_size,
                                mdat_payload,
                                &mut ss.track_state,
                                &mut |frame| {
                                    frames.push(frame);
                                    Ok(())
                                },
                            )?;

                            // Accumulate into segments, splitting at video keyframes
                            for frame in frames {
                                let is_video_keyframe =
                                    ss.video_track_ids.contains(&frame.track_id) && frame.is_sync;

                                if is_video_keyframe && ss.seen_first_keyframe {
                                    ss.segment_number += 1;
                                    events.push(SegmenterEvent::Segment(Segment {
                                        number: ss.segment_number,
                                        data: std::mem::take(&mut ss.segment_buf),
                                    }));
                                }

                                if is_video_keyframe {
                                    ss.seen_first_keyframe = true;
                                }

                                ss.segment_buf.extend_from_slice(&frame.data);
                            }
                        }
                        // Orphan mdat without moof — skip
                    }
                    // Skip styp, sidx, free, etc.
                }
            }

            // Consume the processed box
            self.buffer.drain(..box_total_size);
        }

        Ok(events)
    }

    /// Signal end of stream. Flushes any remaining partial segment.
    pub fn flush(&mut self) -> Result<Vec<SegmenterEvent>> {
        let mut events = Vec::new();
        if let State::Streaming(ss) = &mut self.state {
            if !ss.segment_buf.is_empty() {
                ss.segment_number += 1;
                events.push(SegmenterEvent::Segment(Segment {
                    number: ss.segment_number,
                    data: std::mem::take(&mut ss.segment_buf),
                }));
            }
        }
        Ok(events)
    }
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

/// Peek at an MP4 atom header without consuming it.
///
/// Returns `(total_box_size, four_cc)` or `None` if not enough data yet.
fn peek_atom_header(data: &[u8]) -> Option<(usize, [u8; 4])> {
    if data.len() < 8 {
        return None;
    }
    let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let box_type = [data[4], data[5], data[6], data[7]];

    if size == 1 {
        // Extended 64-bit size
        if data.len() < 16 {
            return None;
        }
        let ext_size = u64::from_be_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]) as usize;
        Some((ext_size, box_type))
    } else if size == 0 {
        // Box extends to EOF — can't handle in streaming mode
        None
    } else {
        Some((size, box_type))
    }
}

fn mp4_err(e: mp4_atom::Error) -> Error {
    Error::InvalidMp4(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_fixture(name: &str) -> Vec<u8> {
        let path = format!("samples/fixtures/{}", name);
        std::fs::read(&path)
            .or_else(|_| std::fs::read(format!("samples/{}", name)))
            .unwrap_or_else(|_| panic!("{} must exist for tests", path))
    }

    #[test]
    fn test_push_segmenter_whole_file() {
        // Feed the entire fMP4 at once — should match pull-based segment_fmp4
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segmenter = Segmenter::new();
        let mut all_events = segmenter.feed(&data).unwrap();
        all_events.extend(segmenter.flush().unwrap());

        // Should have exactly one InitSegment event
        let init_count = all_events
            .iter()
            .filter(|e| matches!(e, SegmenterEvent::InitSegment { .. }))
            .count();
        assert_eq!(init_count, 1, "should emit exactly one init segment");

        // Collect segments
        let segments: Vec<&Segment> = all_events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();
        assert!(!segments.is_empty(), "should produce at least one segment");

        // Segments should be numbered sequentially
        for (i, seg) in segments.iter().enumerate() {
            assert_eq!(seg.number, (i + 1) as u32);
            assert!(!seg.data.is_empty());
        }

        // Compare with pull-based segmenter
        let mut pull_segments = Vec::new();
        crate::segment::segment_fmp4(&mut Cursor::new(&data), |seg| {
            pull_segments.push(seg);
            Ok(())
        })
        .unwrap();

        assert_eq!(
            segments.len(),
            pull_segments.len(),
            "push and pull should produce same number of segments"
        );

        for (push_seg, pull_seg) in segments.iter().zip(pull_segments.iter()) {
            assert_eq!(push_seg.number, pull_seg.number);
            assert_eq!(
                push_seg.data, pull_seg.data,
                "segment {} data should match between push and pull",
                push_seg.number
            );
        }
    }

    #[test]
    fn test_push_segmenter_byte_at_a_time() {
        // Feed one byte at a time — stress test for buffering
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segmenter = Segmenter::new();
        let mut all_events = Vec::new();

        for byte in &data {
            all_events.extend(segmenter.feed(std::slice::from_ref(byte)).unwrap());
        }
        all_events.extend(segmenter.flush().unwrap());

        let init_count = all_events
            .iter()
            .filter(|e| matches!(e, SegmenterEvent::InitSegment { .. }))
            .count();
        assert_eq!(init_count, 1);

        let segments: Vec<&Segment> = all_events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();
        assert!(!segments.is_empty());

        // Should match whole-file results
        let mut pull_segments = Vec::new();
        crate::segment::segment_fmp4(&mut Cursor::new(&data), |seg| {
            pull_segments.push(seg);
            Ok(())
        })
        .unwrap();

        assert_eq!(segments.len(), pull_segments.len());
        for (push_seg, pull_seg) in segments.iter().zip(pull_segments.iter()) {
            assert_eq!(
                push_seg.data, pull_seg.data,
                "segment {} mismatch in byte-at-a-time mode",
                push_seg.number
            );
        }
    }

    #[test]
    fn test_push_segmenter_random_chunks() {
        // Feed in random-sized chunks
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segmenter = Segmenter::new();
        let mut all_events = Vec::new();
        let mut offset = 0;
        let mut chunk_size = 1;

        while offset < data.len() {
            let end = (offset + chunk_size).min(data.len());
            all_events.extend(segmenter.feed(&data[offset..end]).unwrap());
            offset = end;
            // Vary chunk sizes: 1, 17, 289, 4913, ...
            chunk_size = (chunk_size * 17) % 5000 + 1;
        }
        all_events.extend(segmenter.flush().unwrap());

        let segments: Vec<&Segment> = all_events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();

        let mut pull_segments = Vec::new();
        crate::segment::segment_fmp4(&mut Cursor::new(&data), |seg| {
            pull_segments.push(seg);
            Ok(())
        })
        .unwrap();

        assert_eq!(segments.len(), pull_segments.len());
        for (push_seg, pull_seg) in segments.iter().zip(pull_segments.iter()) {
            assert_eq!(
                push_seg.data, pull_seg.data,
                "segment {} mismatch in random-chunk mode",
                push_seg.number
            );
        }
    }

    #[test]
    fn test_push_init_segment_is_canonical() {
        // The init segment from push should be identical to build_init_segment
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segmenter = Segmenter::new();
        let events = segmenter.feed(&data).unwrap();

        let (push_catalog, push_init) = events
            .into_iter()
            .find_map(|e| match e {
                SegmenterEvent::InitSegment { catalog, data } => Some((catalog, data)),
                _ => None,
            })
            .expect("should emit init segment");

        let canonical_init = build_init_segment(&push_catalog).unwrap();
        assert_eq!(
            push_init, canonical_init,
            "push init segment should be canonical"
        );
    }

    #[test]
    fn test_push_total_bytes_match_pull() {
        // Total segment bytes should be identical
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segmenter = Segmenter::new();
        let mut events = segmenter.feed(&data).unwrap();
        events.extend(segmenter.flush().unwrap());

        let push_total: usize = events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s.data.len()),
                _ => None,
            })
            .sum();

        let mut pull_total = 0;
        crate::segment::segment_fmp4(&mut Cursor::new(&data), |seg| {
            pull_total += seg.data.len();
            Ok(())
        })
        .unwrap();

        assert_eq!(push_total, pull_total, "total segment bytes should match");
    }
}
