//! Concatenator: merge multiple MUXL fMP4 or flat MP4 files into a single
//! event stream.
//!
//! Accepts concatenated MUXL files via `feed()`. Two input shapes are handled
//! interchangeably:
//!
//! - **fMP4**: `ftyp + uuid? + moov + moof+mdat moof+mdat ...`
//! - **flat MP4** (incl. signed muxl): `ftyp + uuid? + moov + mdat-envelope { moof+mdat moof+mdat ... }`
//!
//! Detection is implicit: an mdat encountered in streaming state with no
//! pending moof is the outer envelope of a flat MP4 — the concat consumes its
//! header only and lets the inner moof+mdat pairs flow through.
//!
//! The UUID atom in the init section is extracted and prepended to each output
//! segment.
//!
//! Performs keyframe-based segmentation (like [`crate::push::Segmenter`]) but
//! additionally handles multiple fMP4 files: when a new moov arrives, track state
//! resets and a new init event is emitted only if the catalog actually changed.
//!
//! # Example
//! ```ignore
//! let mut concat = Concatenator::new();
//! for chunk in fmp4_stream {
//!     for event in concat.feed(&chunk)? {
//!         match event {
//!             SegmenterEvent::InitSegment { catalog, data } => { /* init changed */ }
//!             SegmenterEvent::Segment(seg) => { /* uuid + moof+mdat data */ }
//!         }
//!     }
//! }
//! for event in concat.flush()? {
//!     // handle final segment
//! }
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;

use mp4_atom::{Header, Moof, Moov, ReadAtom, ReadFrom};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::fragment::{self, Frame};
use crate::init::{build_init_segment, catalog_from_moov};
use crate::push::SegmenterEvent;
use crate::segment::GopSegment;

/// Push-based concatenator for merging multiple MUXL fMP4 or flat MP4 files.
///
/// Feed concatenated bytes via `feed()`. The concatenator extracts UUID
/// atoms from each file's init section and prepends them to output
/// segments. Init events are emitted only when the catalog changes between
/// files. For flat MP4 input, a segment is flushed at the end of each
/// outer-mdat envelope — so livestream consumers don't lag by one file
/// waiting for the next ftyp boundary.
pub struct Concatenator {
    buffer: Vec<u8>,
    current_catalog: Option<Catalog>,
    /// UUID atom from the current fMP4's init section, prepended to each segment.
    current_uuid: Option<Vec<u8>>,
    /// UUID atom seen during init phase, before moov has arrived.
    pending_uuid: Option<Vec<u8>>,
    state: ConcatState,
    /// Per-track segment buffers, ordered by track_id.
    track_bufs: BTreeMap<u32, Vec<u8>>,
    /// Per-track accumulated duration in timescale ticks.
    track_durations: BTreeMap<u32, u64>,
    /// Per-track accumulated sample count.
    track_sample_counts: BTreeMap<u32, u32>,
    segment_number: u32,
    /// Bytes left until the end of the current flat-MP4 outer-mdat envelope.
    /// When this reaches 0 we flush the accumulated segment and clear the
    /// marker. `None` outside an envelope (i.e. fMP4 input or before the
    /// outer mdat is seen).
    envelope_remaining: Option<usize>,
}

enum ConcatState {
    /// Waiting for the moov box (skipping ftyp, capturing uuid).
    WaitingForInit,
    /// Processing moof+mdat fragment pairs.
    Streaming(StreamingState),
}

struct StreamingState {
    moov: Moov,
    video_track_ids: HashSet<u32>,
    track_state: HashMap<u32, (u64, u32)>,
    seen_first_keyframe: bool,
    pending_moof: Option<PendingMoof>,
}

struct PendingMoof {
    moof: Moof,
    box_size: usize,
}

impl Concatenator {
    /// Create a new concatenator ready to receive MUXL fMP4 data.
    pub fn new() -> Self {
        Concatenator {
            buffer: Vec::new(),
            current_catalog: None,
            current_uuid: None,
            pending_uuid: None,
            state: ConcatState::WaitingForInit,
            track_bufs: BTreeMap::new(),
            track_durations: BTreeMap::new(),
            track_sample_counts: BTreeMap::new(),
            segment_number: 0,
            envelope_remaining: None,
        }
    }

    /// Feed a chunk of concatenated MUXL fMP4 or flat MP4 data. Returns any
    /// events produced.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<SegmenterEvent>> {
        self.buffer.extend_from_slice(data);
        let mut events = Vec::new();

        loop {
            let (box_total_size, box_type) = match peek_atom_header(&self.buffer) {
                Some(v) => v,
                None => break,
            };

            // Outer-mdat envelope of a (signed) flat MP4: an mdat in
            // Streaming state with no pending moof is the wrapper around
            // inner moof+mdat fragment pairs. Consume only the box header
            // so the inner pairs surface as top-level boxes for the
            // existing handlers — and so we don't have to wait for the
            // full envelope (potentially gigabytes) to arrive. Record the
            // remaining body size so we can flush when we hit the end of
            // the envelope (one envelope = one logical segment).
            if &box_type == b"mdat"
                && matches!(
                    &self.state,
                    ConcatState::Streaming(ss) if ss.pending_moof.is_none()
                )
            {
                let header_size = if self.buffer[0..4] == [0u8, 0, 0, 1] { 16 } else { 8 };
                self.envelope_remaining = Some(box_total_size.saturating_sub(header_size));
                self.buffer.drain(..header_size);
                continue;
            }

            if self.buffer.len() < box_total_size {
                break;
            }

            let box_data: Vec<u8> = self.buffer.drain(..box_total_size).collect();

            match &box_type {
                b"ftyp" => {
                    // New fMP4 starting — flush any pending segment, reset to init phase
                    self.flush_segment_into(&mut events);
                    self.pending_uuid = None;
                    self.envelope_remaining = None;
                    self.state = ConcatState::WaitingForInit;
                }
                b"uuid" => {
                    match &self.state {
                        ConcatState::WaitingForInit => {
                            // UUID in init section — capture for later
                            self.pending_uuid = Some(box_data);
                        }
                        ConcatState::Streaming(_) => {
                            // UUID during streaming — treat as new fMP4's init
                            self.flush_segment_into(&mut events);
                            self.pending_uuid = Some(box_data);
                            self.envelope_remaining = None;
                            self.state = ConcatState::WaitingForInit;
                        }
                    }
                }
                b"moov" => {
                    // Flush any pending segment from previous fMP4
                    self.flush_segment_into(&mut events);

                    // Parse moov
                    let mut cursor = Cursor::new(&box_data);
                    let header = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                        .map_err(mp4_err)?
                        .ok_or_else(|| Error::InvalidMp4("empty moov header".into()))?;
                    let moov = Moov::read_atom(&header, &mut cursor).map_err(mp4_err)?;
                    let catalog = catalog_from_moov(&moov)?;

                    // Emit init only if catalog changed
                    let changed = self.current_catalog.as_ref() != Some(&catalog);
                    if changed {
                        let init_data = build_init_segment(&catalog)?;
                        events.push(SegmenterEvent::InitSegment {
                            catalog: catalog.clone(),
                            data: init_data,
                        });
                        self.current_catalog = Some(catalog);
                    }

                    // Promote pending UUID to current
                    self.current_uuid = self.pending_uuid.take();

                    let video_track_ids: HashSet<u32> = self
                        .current_catalog
                        .as_ref()
                        .unwrap()
                        .video_configs()
                        .map(|v| v.track_id())
                        .collect();

                    self.state = ConcatState::Streaming(StreamingState {
                        moov,
                        video_track_ids,
                        track_state: HashMap::new(),
                        seen_first_keyframe: false,
                        pending_moof: None,
                    });
                }
                b"moof" => {
                    if let ConcatState::Streaming(ss) = &mut self.state {
                        let mut cursor = Cursor::new(&box_data);
                        let header = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                            .map_err(mp4_err)?
                            .ok_or_else(|| Error::InvalidMp4("empty moof header".into()))?;
                        let moof =
                            Moof::read_atom(&header, &mut cursor).map_err(mp4_err)?;
                        ss.pending_moof = Some(PendingMoof {
                            moof,
                            box_size: box_data.len(),
                        });
                    }
                }
                b"mdat" => {
                    // Process moof+mdat into frames, then handle segmentation.
                    // We collect frames first to release the borrow on self.state
                    // before calling flush_segment_into.
                    let mut frames: Vec<(Frame, bool)> = Vec::new();

                    if let ConcatState::Streaming(ss) = &mut self.state {
                        if let Some(pending) = ss.pending_moof.take() {
                            let mdat_header_size =
                                if box_data.len() > u32::MAX as usize { 16 } else { 8 };
                            let mdat_payload = &box_data[mdat_header_size..];

                            let mut raw_frames: Vec<Frame> = Vec::new();
                            fragment::process_moof_mdat(
                                &ss.moov,
                                &pending.moof,
                                pending.box_size,
                                mdat_payload,
                                &mut ss.track_state,
                                &mut |frame| {
                                    raw_frames.push(frame);
                                    Ok(())
                                },
                            )?;

                            for frame in raw_frames {
                                let is_video_keyframe =
                                    ss.video_track_ids.contains(&frame.track_id)
                                        && frame.is_sync;
                                frames.push((frame, is_video_keyframe));
                            }
                        }
                    }

                    // Now process frames with full access to self
                    for (frame, is_video_keyframe) in frames {
                        let ss = match &mut self.state {
                            ConcatState::Streaming(ss) => ss,
                            _ => unreachable!(),
                        };

                        if is_video_keyframe && ss.seen_first_keyframe {
                            self.flush_segment_into(&mut events);
                        }

                        let ss = match &mut self.state {
                            ConcatState::Streaming(ss) => ss,
                            _ => unreachable!(),
                        };
                        if is_video_keyframe {
                            ss.seen_first_keyframe = true;
                        }

                        *self.track_durations.entry(frame.track_id).or_default() += frame.duration as u64;
                        *self.track_sample_counts.entry(frame.track_id).or_default() += 1;
                        self.track_bufs
                            .entry(frame.track_id)
                            .or_default()
                            .extend_from_slice(&frame.data);
                    }
                }
                _ => {
                    // Skip other boxes (free, styp, sidx, etc.)
                }
            }

            // If we're inside a flat-MP4 outer-mdat envelope, count this
            // box against the remaining body. ftyp/uuid/moov already
            // cleared envelope_remaining in their handlers, so this only
            // fires for the inner moof/mdat pairs that actually live
            // inside the envelope. When we hit zero, the envelope is done
            // → flush the accumulated segment.
            if let Some(rem) = self.envelope_remaining.as_mut() {
                *rem = rem.saturating_sub(box_total_size);
                if *rem == 0 {
                    self.envelope_remaining = None;
                    self.flush_segment_into(&mut events);
                }
            }
        }

        Ok(events)
    }

    /// Signal end of stream. Flushes any remaining partial segment.
    pub fn flush(&mut self) -> Result<Vec<SegmenterEvent>> {
        let mut events = Vec::new();
        self.flush_segment_into(&mut events);
        Ok(events)
    }

    fn flush_segment_into(&mut self, events: &mut Vec<SegmenterEvent>) {
        if !self.track_bufs.values().any(|b| !b.is_empty()) {
            return;
        }
        self.segment_number += 1;
        let uuid = self.current_uuid.clone();
        let mut tracks = BTreeMap::new();
        let mut durations = BTreeMap::new();
        let mut sample_counts = BTreeMap::new();
        for (&track_id, buf) in self.track_bufs.iter_mut() {
            if !buf.is_empty() {
                let mut data = Vec::new();
                if let Some(ref uuid) = uuid {
                    data.extend_from_slice(uuid);
                }
                data.append(buf);
                tracks.insert(track_id, data);
                durations.insert(track_id, self.track_durations.remove(&track_id).unwrap_or(0));
                sample_counts.insert(track_id, self.track_sample_counts.remove(&track_id).unwrap_or(0));
            }
        }
        self.track_durations.clear();
        self.track_sample_counts.clear();
        if !tracks.is_empty() {
            events.push(SegmenterEvent::Segment(GopSegment {
                number: self.segment_number,
                tracks,
                durations,
                sample_counts,
            }));
        }
    }
}

impl Default for Concatenator {
    fn default() -> Self {
        Self::new()
    }
}

/// Peek at an MP4 atom header without consuming it.
fn peek_atom_header(data: &[u8]) -> Option<(usize, [u8; 4])> {
    if data.len() < 8 {
        return None;
    }
    let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let box_type = [data[4], data[5], data[6], data[7]];

    if size == 1 {
        if data.len() < 16 {
            return None;
        }
        let ext_size = u64::from_be_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]) as usize;
        Some((ext_size, box_type))
    } else if size == 0 {
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

    fn read_fixture(name: &str) -> Vec<u8> {
        let path = format!("samples/fixtures/{}", name);
        std::fs::read(&path)
            .or_else(|_| std::fs::read(format!("samples/{}", name)))
            .unwrap_or_else(|_| panic!("{} must exist for tests", path))
    }

    /// Build a minimal UUID atom with the given 16-byte UUID and optional payload.
    fn make_uuid_atom(uuid: &[u8; 16], payload: &[u8]) -> Vec<u8> {
        let size = 8 + 16 + payload.len();
        let mut atom = Vec::with_capacity(size);
        atom.extend_from_slice(&(size as u32).to_be_bytes());
        atom.extend_from_slice(b"uuid");
        atom.extend_from_slice(uuid);
        atom.extend_from_slice(payload);
        atom
    }

    /// Build a test fMP4 in the Streamplace layout: ftyp + uuid + moov + moof+mdat...
    /// Uses the existing Segmenter to get canonical init + segment data from the fixture,
    /// then reassembles with the UUID atom inserted between ftyp and moov.
    /// Returns (fMP4 bytes, original GopSegments).
    fn build_streamplace_fmp4(
        fixture: &[u8],
        uuid_atom: &[u8],
    ) -> (Vec<u8>, Vec<GopSegment>) {
        let mut segmenter = crate::push::Segmenter::new();
        let mut seg_events = segmenter.feed(fixture).unwrap();
        seg_events.extend(segmenter.flush().unwrap());

        let mut fmp4 = Vec::new();
        let mut original_gops = Vec::new();

        for event in seg_events {
            match event {
                SegmenterEvent::InitSegment { data, .. } => {
                    let (ftyp_size, _) = peek_atom_header(&data).unwrap();
                    let ftyp = &data[..ftyp_size];
                    let moov = &data[ftyp_size..];
                    fmp4.extend_from_slice(ftyp);
                    fmp4.extend_from_slice(uuid_atom);
                    fmp4.extend_from_slice(moov);
                }
                SegmenterEvent::Segment(gop) => {
                    // Write interleaved for the fMP4 input
                    for data in gop.tracks.values() {
                        fmp4.extend_from_slice(data);
                    }
                    original_gops.push(gop);
                }
            }
        }

        (fmp4, original_gops)
    }

    #[test]
    fn test_concat_single_fmp4_no_uuid() {
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segmenter = crate::push::Segmenter::new();
        let mut seg_events = segmenter.feed(&data).unwrap();
        seg_events.extend(segmenter.flush().unwrap());

        let mut fmp4 = Vec::new();
        let mut original_gops = Vec::new();
        for event in seg_events {
            match event {
                SegmenterEvent::InitSegment { data, .. } => fmp4.extend_from_slice(&data),
                SegmenterEvent::Segment(gop) => {
                    for data in gop.tracks.values() {
                        fmp4.extend_from_slice(data);
                    }
                    original_gops.push(gop);
                }
            }
        }

        let mut concat = Concatenator::new();
        let mut events = concat.feed(&fmp4).unwrap();
        events.extend(concat.flush().unwrap());

        let init_count = events
            .iter()
            .filter(|e| matches!(e, SegmenterEvent::InitSegment { .. }))
            .count();
        assert_eq!(init_count, 1);

        let gops: Vec<&GopSegment> = events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();

        assert_eq!(gops.len(), original_gops.len());
        for (concat_gop, orig_gop) in gops.iter().zip(original_gops.iter()) {
            assert_eq!(concat_gop.tracks, orig_gop.tracks, "GOP {} tracks should match", concat_gop.number);
        }
    }

    #[test]
    fn test_concat_uuid_prepended_to_segments() {
        let data = read_fixture("h264-opus-frag.mp4");
        let test_uuid: [u8; 16] = [0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let uuid_atom = make_uuid_atom(&test_uuid, b"s2pa-payload");

        let (fmp4, original_gops) = build_streamplace_fmp4(&data, &uuid_atom);

        let mut concat = Concatenator::new();
        let mut events = concat.feed(&fmp4).unwrap();
        events.extend(concat.flush().unwrap());

        let gops: Vec<&GopSegment> = events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();

        assert_eq!(gops.len(), original_gops.len());
        for (concat_gop, orig_gop) in gops.iter().zip(original_gops.iter()) {
            // Each track's data should be uuid_atom + original data
            for (tid, concat_data) in &concat_gop.tracks {
                let orig_data = orig_gop.tracks.get(tid).expect("missing track");
                assert!(
                    concat_data.starts_with(&uuid_atom),
                    "GOP {} track {} should start with UUID atom",
                    concat_gop.number, tid
                );
                assert_eq!(
                    &concat_data[uuid_atom.len()..],
                    orig_data.as_slice(),
                    "GOP {} track {} data should match after UUID",
                    concat_gop.number, tid
                );
            }
        }
    }

    #[test]
    fn test_concat_duplicate_fmp4_single_init() {
        let data = read_fixture("h264-opus-frag.mp4");
        let test_uuid: [u8; 16] = [0x01; 16];
        let uuid_atom = make_uuid_atom(&test_uuid, &[]);

        let (fmp4, _) = build_streamplace_fmp4(&data, &uuid_atom);

        let mut doubled = fmp4.clone();
        doubled.extend_from_slice(&fmp4);

        let mut concat = Concatenator::new();
        let mut events = concat.feed(&doubled).unwrap();
        events.extend(concat.flush().unwrap());

        let init_count = events
            .iter()
            .filter(|e| matches!(e, SegmenterEvent::InitSegment { .. }))
            .count();
        assert_eq!(init_count, 1, "identical fMP4s should emit init only once");

        // All tracks in all GOPs should have the UUID prefix
        let gops: Vec<&GopSegment> = events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();
        for gop in &gops {
            for (tid, data) in &gop.tracks {
                assert!(
                    data.starts_with(&uuid_atom),
                    "GOP {} track {} should have UUID prefix",
                    gop.number, tid
                );
            }
        }
    }

    #[test]
    fn test_concat_byte_at_a_time() {
        let data = read_fixture("h264-opus-frag.mp4");
        let test_uuid: [u8; 16] = [0xAB; 16];
        let uuid_atom = make_uuid_atom(&test_uuid, b"test");

        let (fmp4, _) = build_streamplace_fmp4(&data, &uuid_atom);

        let mut concat_whole = Concatenator::new();
        let mut whole_events = concat_whole.feed(&fmp4).unwrap();
        whole_events.extend(concat_whole.flush().unwrap());

        let mut concat_byte = Concatenator::new();
        let mut byte_events = Vec::new();
        for b in &fmp4 {
            byte_events.extend(concat_byte.feed(std::slice::from_ref(b)).unwrap());
        }
        byte_events.extend(concat_byte.flush().unwrap());

        let whole_gops: Vec<&GopSegment> = whole_events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();
        let byte_gops: Vec<&GopSegment> = byte_events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();

        assert_eq!(whole_gops.len(), byte_gops.len());
        for (w, b) in whole_gops.iter().zip(byte_gops.iter()) {
            assert_eq!(w.tracks, b.tracks, "GOP {} mismatch", w.number);
        }
    }

    #[test]
    fn test_concat_segments_match_segmenter() {
        let data = read_fixture("h264-opus-frag.mp4");
        let test_uuid: [u8; 16] = [0x42; 16];
        let uuid_atom = make_uuid_atom(&test_uuid, &[]);

        let (fmp4, original_gops) = build_streamplace_fmp4(&data, &uuid_atom);

        let mut concat = Concatenator::new();
        let mut events = concat.feed(&fmp4).unwrap();
        events.extend(concat.flush().unwrap());

        let concat_gops: Vec<&GopSegment> = events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();

        assert_eq!(concat_gops.len(), original_gops.len());
        for (concat_gop, orig_gop) in concat_gops.iter().zip(original_gops.iter()) {
            for (tid, concat_data) in &concat_gop.tracks {
                let orig_data = orig_gop.tracks.get(tid).expect("missing track");
                let after_uuid = &concat_data[uuid_atom.len()..];
                assert_eq!(
                    after_uuid,
                    orig_data.as_slice(),
                    "GOP {} track {} content mismatch",
                    concat_gop.number, tid
                );
            }
        }
    }

    /// Concat must descend into a flat MP4's outer mdat envelope to surface
    /// the inner moof+mdat fragments. Streamplace's S3 livestream-to-VOD
    /// pipeline feeds signed muxl flat MP4s (one per signed segment) — if
    /// concat treats the outer mdat opaquely, InitCh fires but SegCh never
    /// does. Regression test for that bug.
    #[test]
    fn test_concat_flat_mp4_input() {
        let data = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&data[..]).unwrap();
        let mut flat_mp4 = Vec::new();
        crate::flat::write(&source, &data[..], &mut flat_mp4).unwrap();

        let mut concat = Concatenator::new();
        let mut events = concat.feed(&flat_mp4).unwrap();
        events.extend(concat.flush().unwrap());

        let init_count = events
            .iter()
            .filter(|e| matches!(e, SegmenterEvent::InitSegment { .. }))
            .count();
        assert_eq!(init_count, 1, "expected one init segment");

        let gops: Vec<&GopSegment> = events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();
        assert!(!gops.is_empty(), "concat must emit segments from flat MP4 input");

        // Across all GoPs the union of tracks must include every catalog
        // track — i.e. fragments from both the video and audio sub-runs of
        // the outer mdat envelope made it out. (We don't assert per-GoP
        // track presence: flat MP4 groups fragments by track rather than
        // interleaving by time, so concat's keyframe-based segmentation
        // can produce per-track-only intermediate segments. Streamplace's
        // real flow feeds one-GoP flat MP4s where the ftyp boundary
        // delimits segments and this concern goes away.)
        let mut seen_tracks = std::collections::BTreeSet::new();
        for gop in &gops {
            for tid in gop.tracks.keys() {
                seen_tracks.insert(*tid);
            }
        }
        let expected: std::collections::BTreeSet<u32> = source
            .catalog
            .video_configs()
            .map(|v| v.track_id())
            .chain(source.catalog.audio_configs().map(|a| a.track_id()))
            .collect();
        assert_eq!(seen_tracks, expected, "expected all catalog tracks across emitted segments");
    }

    /// Streaming livestream invariant: each input file's segment must be
    /// emitted as soon as that file's outer-mdat envelope completes — *not*
    /// at the next file's ftyp boundary or at flush(). Without this, a
    /// livestream consumer (Streamplace's S3 upload) lags one file behind
    /// the producer indefinitely, since the final flush only ever fires at
    /// stream close.
    #[test]
    fn test_concat_flushes_at_envelope_end_without_flush_call() {
        let data = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&data[..]).unwrap();
        let mut flat_mp4 = Vec::new();
        crate::flat::write(&source, &data[..], &mut flat_mp4).unwrap();

        // Feed three concatenated flat MP4s; do NOT call flush().
        let mut concat = Concatenator::new();
        let mut events = Vec::new();
        for _ in 0..3 {
            events.extend(concat.feed(&flat_mp4).unwrap());
        }

        let segs: Vec<&GopSegment> = events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();
        // At minimum, we expect three envelope-end flushes — one per file.
        // (Per-file may also include keyframe-mid flushes if the fixture
        // has multiple GoPs, hence >= rather than ==.)
        assert!(
            segs.len() >= 3,
            "expected at least 3 segments without flush() (one per envelope), got {}",
            segs.len(),
        );
    }

    /// Same as above but byte-by-byte feeding, to confirm the streaming
    /// state machine handles the outer mdat header arriving in pieces and
    /// doesn't block waiting for the (potentially gigabytes-long) envelope
    /// body.
    #[test]
    fn test_concat_flat_mp4_byte_at_a_time() {
        let data = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&data[..]).unwrap();
        let mut flat_mp4 = Vec::new();
        crate::flat::write(&source, &data[..], &mut flat_mp4).unwrap();

        let mut concat = Concatenator::new();
        let mut events = Vec::new();
        for b in &flat_mp4 {
            events.extend(concat.feed(std::slice::from_ref(b)).unwrap());
        }
        events.extend(concat.flush().unwrap());

        let gops: Vec<&GopSegment> = events
            .iter()
            .filter_map(|e| match e {
                SegmenterEvent::Segment(s) => Some(s),
                _ => None,
            })
            .collect();
        assert!(!gops.is_empty(), "byte-at-a-time flat MP4 input must produce segments");
    }
}
