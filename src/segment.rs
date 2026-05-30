//! MUXL segmentation: fMP4 → per-track, GOP-aligned MUXL segments.
//!
//! Each MUXL segment contains one track's moof+mdat pairs for one video GOP.
//! Segments are emitted per-track at each keyframe boundary, ordered by
//! track_id ascending. This enables per-track byte-range addressing in
//! MUXL fMP4 files (for HLS playlists) and per-track content hashing.
//!
//! Spec: architecture.md § MUXL Segment

use std::collections::{BTreeMap, HashSet};
use std::io::Read;

use crate::catalog::{Catalog, to_drisl};
use crate::error::Result;
use crate::fragment::FMP4Reader;

/// The MUXL canonical-segment UUID identifier
/// (`e6404ea2-8f01-4305-98da-7bec3c2a9173`). Every canonical segment
/// begins with a `uuid` box bearing this identifier; the body of the
/// box is a DRISL-encoded single-track [`Catalog`]. Spec:
/// `canonical-form.md § MUXL Canonical Segment`.
pub const MUXL_UUID: [u8; 16] = [
    0xe6, 0x40, 0x4e, 0xa2,
    0x8f, 0x01, 0x43, 0x05,
    0x98, 0xda, 0x7b, 0xec,
    0x3c, 0x2a, 0x91, 0x73,
];

/// Build the leading `uuid` box for a canonical segment: 4-byte BE size,
/// `"uuid"` FourCC, 16-byte [`MUXL_UUID`], and the DRISL-encoded
/// single-track catalog (filtered from `catalog` to just `track_id`).
pub fn mint_canonical_segment_prefix(catalog: &Catalog, track_id: u32) -> Result<Vec<u8>> {
    let single = catalog.filter_to_track(track_id);
    let drisl = to_drisl(&single)?;
    let total = 24usize + drisl.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(b"uuid");
    out.extend_from_slice(&MUXL_UUID);
    out.extend_from_slice(&drisl);
    Ok(out)
}

/// A per-track, GOP-aligned MUXL segment.
pub struct Segment {
    /// GOP number (1-based). All tracks for the same GOP share the same number.
    pub number: u32,
    /// Track ID this segment belongs to.
    pub track_id: u32,
    /// Concatenated moof+mdat pairs for this track in this GOP.
    pub data: Vec<u8>,
}

/// A bundled multi-track GOP segment for streaming events.
#[derive(Debug, PartialEq)]
///
/// Contains all tracks' data for one GOP. Track data is keyed by track_id.
/// Used in [`crate::push::SegmenterEvent`] so consumers receive a complete
/// GOP in a single event.
pub struct GopSegment {
    /// GOP number (1-based).
    pub number: u32,
    /// Per-track moof+mdat data, keyed by track_id (ordered ascending).
    pub tracks: BTreeMap<u32, Vec<u8>>,
    /// Per-track total duration in timescale ticks.
    pub durations: BTreeMap<u32, u64>,
    /// Per-track sample (frame) count.
    pub sample_counts: BTreeMap<u32, u32>,
    /// Per-track per-sample metadata. Lets downstream consumers
    /// reconstruct a flat-MP4 moov (co64/stts/stsz/stss) or build an
    /// HLS playlist without re-parsing the segment bytes.
    pub samples: BTreeMap<u32, TrackSamples>,
    /// Total body bytes this segment contributes when concatenated into a
    /// flat MP4 (sum of `tracks` values' lengths). Used by downstream
    /// consumers to compute cumulative byte offsets in the synth flat MP4
    /// without summing track byte arrays themselves.
    pub body_size: u64,
    /// Segment's playable duration in microseconds. Computed from the
    /// video track if present, otherwise the longest track. Suitable for
    /// HLS `EXTINF` (just divide by 1e6).
    pub duration_us: u64,
}

/// Per-sample metadata as parallel arrays. Same length as the track's
/// sample count.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TrackSamples {
    /// Sample durations in the track's media timescale.
    pub durations: Vec<u32>,
    /// Encoded sample sizes in bytes (mdat payload, not counting the
    /// preceding moof or mdat header).
    pub sizes: Vec<u32>,
    /// Composition-time offsets, signed.
    pub cts_offsets: Vec<i32>,
    /// Sample indices that are sync (key) samples, 1-based — the same
    /// shape an `stss` table uses. Most tracks have a small number of
    /// sync samples so a sparse list is more compact than a per-sample
    /// boolean.
    pub sync_indices: Vec<u32>,
    /// Byte offset of each sample's payload (just the mdat data, not
    /// including the preceding moof or mdat header) within the track's
    /// segment buffer. Combined with the segment's position in a synth
    /// flat MP4's body, this is the value `co64` should carry.
    pub offsets_in_track: Vec<u64>,
}

/// Segment an fMP4 stream into per-track, GOP-aligned MUXL segments.
///
/// Only requires `Read` — no seeking. Splits at video keyframe boundaries.
/// Each GOP emits a [`GopSegment`] containing all tracks' data.
///
/// Calls `on_gop` for each completed GOP segment. Returns the catalog.
pub fn segment_fmp4<R: Read>(
    reader: &mut R,
    on_gop: impl FnMut(GopSegment) -> Result<()>,
) -> Result<Catalog> {
    segment_fmp4_with_remap(reader, BTreeMap::new(), on_gop)
}

/// Like [`segment_fmp4`], but relabels track ids per `remap` (input id →
/// output id): the returned catalog and every emitted segment carry the new
/// ids. An empty map is the identity.
pub fn segment_fmp4_with_remap<R: Read>(
    reader: &mut R,
    remap: BTreeMap<u32, u32>,
    mut on_gop: impl FnMut(GopSegment) -> Result<()>,
) -> Result<Catalog> {
    let mut fmp4 = FMP4Reader::with_remap(reader, remap)?;
    let catalog = fmp4.catalog().clone();

    // Determine which track IDs are video (for keyframe detection)
    let video_track_ids: HashSet<u32> = catalog.video_configs().map(|v| v.track_id()).collect();
    // Per-track timescales, used to compute segment duration_us.
    let track_timescales: BTreeMap<u32, u32> = catalog
        .video_configs()
        .map(|v| (v.track_id(), v.timescale()))
        .chain(
            catalog
                .audio_configs()
                .map(|a| (a.track_id(), a.timescale())),
        )
        .collect();

    // Per-track buffers, ordered by track_id
    let mut track_bufs: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut track_durations: BTreeMap<u32, u64> = BTreeMap::new();
    let mut track_sample_counts: BTreeMap<u32, u32> = BTreeMap::new();
    let mut track_samples: BTreeMap<u32, TrackSamples> = BTreeMap::new();
    let has_video = !video_track_ids.is_empty();
    let mut segment_number: u32 = 0;
    let mut seen_first_keyframe = false;

    while let Some(frame) = fmp4.next_frame()? {
        let is_video_keyframe = video_track_ids.contains(&frame.track_id) && frame.is_sync;

        // A new segment opens at each video keyframe (after the first), or —
        // for audio-only streams — at each 1-second wall-clock span.
        let boundary = if has_video {
            is_video_keyframe && seen_first_keyframe
        } else {
            audio_only_boundary(&track_durations, &track_timescales)
        };

        if boundary {
            segment_number += 1;
            if let Some(gop) = flush_track_bufs(
                &mut track_bufs,
                &mut track_durations,
                &mut track_sample_counts,
                &mut track_samples,
                &track_timescales,
                &video_track_ids,
                &catalog,
                segment_number,
            )? {
                on_gop(gop)?;
            }
        }

        if is_video_keyframe {
            seen_first_keyframe = true;
        }

        record_frame(
            &frame,
            &mut track_bufs,
            &mut track_durations,
            &mut track_sample_counts,
            &mut track_samples,
        );
    }

    // Flush remaining data
    segment_number += 1;
    if let Some(gop) = flush_track_bufs(
        &mut track_bufs,
        &mut track_durations,
        &mut track_sample_counts,
        &mut track_samples,
        &track_timescales,
        &video_track_ids,
        &catalog,
        segment_number,
    )? {
        on_gop(gop)?;
    }

    Ok(catalog)
}

/// Append a frame's bytes + metadata to the per-track buffers, recording
/// the sample's offset within the buffer (post-append) for later use in
/// the synthesized flat-MP4 `co64`. The recorded offset points at the
/// sample payload — i.e. `prior_buffer_size + frame.moof_size + 8` (the
/// moof preceded by 8 bytes of mdat header).
pub(crate) fn record_frame(
    frame: &crate::fragment::Frame,
    track_bufs: &mut BTreeMap<u32, Vec<u8>>,
    track_durations: &mut BTreeMap<u32, u64>,
    track_sample_counts: &mut BTreeMap<u32, u32>,
    track_samples: &mut BTreeMap<u32, TrackSamples>,
) {
    let buf = track_bufs.entry(frame.track_id).or_default();
    let offset_in_track = buf.len() as u64 + frame.moof_size as u64 + 8;
    buf.extend_from_slice(&frame.data);

    *track_durations.entry(frame.track_id).or_default() += frame.duration as u64;
    let count = track_sample_counts.entry(frame.track_id).or_default();
    *count += 1;
    let sample_index = *count;

    let samples = track_samples.entry(frame.track_id).or_default();
    samples.durations.push(frame.duration);
    samples.sizes.push(frame.size);
    samples.cts_offsets.push(frame.cts_offset);
    samples.offsets_in_track.push(offset_in_track);
    if frame.is_sync {
        samples.sync_indices.push(sample_index);
    }
}

/// Flush all non-empty per-track buffers into a [`GopSegment`].
///
/// Returns `None` if all buffers are empty. Each non-empty track buffer
/// is wrapped into a canonical segment by prepending its single-track
/// [`mint_canonical_segment_prefix`] (uuid + DRISL catalog) — the
/// emitted `tracks[tid]` bytes are the full canonical segment.
/// Per-sample `offsets_in_track` shift by the uuid prefix length so
/// they keep pointing at sample bytes within the emitted buffer.
pub(crate) fn flush_track_bufs(
    track_bufs: &mut BTreeMap<u32, Vec<u8>>,
    track_durations: &mut BTreeMap<u32, u64>,
    track_sample_counts: &mut BTreeMap<u32, u32>,
    track_samples: &mut BTreeMap<u32, TrackSamples>,
    track_timescales: &BTreeMap<u32, u32>,
    video_track_ids: &HashSet<u32>,
    catalog: &Catalog,
    segment_number: u32,
) -> Result<Option<GopSegment>> {
    let mut tracks = BTreeMap::new();
    let mut durations = BTreeMap::new();
    let mut sample_counts = BTreeMap::new();
    let mut samples = BTreeMap::new();
    let mut body_size: u64 = 0;
    for (&track_id, buf) in track_bufs.iter_mut() {
        if !buf.is_empty() {
            let prefix = mint_canonical_segment_prefix(catalog, track_id)?;
            let prefix_len = prefix.len() as u64;
            let mut combined = Vec::with_capacity(prefix.len() + buf.len());
            combined.extend_from_slice(&prefix);
            combined.append(buf);
            body_size += combined.len() as u64;
            tracks.insert(track_id, combined);
            durations.insert(track_id, track_durations.remove(&track_id).unwrap_or(0));
            sample_counts.insert(track_id, track_sample_counts.remove(&track_id).unwrap_or(0));
            let mut ts = track_samples.remove(&track_id).unwrap_or_default();
            for off in &mut ts.offsets_in_track {
                *off += prefix_len;
            }
            samples.insert(track_id, ts);
        }
    }
    // Clear any remaining entries for tracks that had no data
    track_durations.clear();
    track_sample_counts.clear();
    track_samples.clear();
    if tracks.is_empty() {
        Ok(None)
    } else {
        let duration_us = compute_duration_us(&durations, track_timescales, video_track_ids);
        Ok(Some(GopSegment {
            number: segment_number,
            tracks,
            durations,
            sample_counts,
            samples,
            body_size,
            duration_us,
        }))
    }
}

/// Compute a segment's playable duration in microseconds. Prefers the
/// first video track; falls back to the track with the longest duration
/// in microseconds when there is no video. Returns 0 for an empty
/// segment.
pub(crate) fn compute_duration_us(
    durations: &BTreeMap<u32, u64>,
    track_timescales: &BTreeMap<u32, u32>,
    video_track_ids: &HashSet<u32>,
) -> u64 {
    let to_us = |tid: u32, ticks: u64| -> u64 {
        let ts = track_timescales.get(&tid).copied().unwrap_or(0);
        if ts == 0 {
            return 0;
        }
        ticks.saturating_mul(1_000_000) / ts as u64
    };
    if let Some((&tid, &ticks)) = durations
        .iter()
        .find(|(tid, _)| video_track_ids.contains(tid))
    {
        return to_us(tid, ticks);
    }
    durations
        .iter()
        .map(|(&tid, &ticks)| to_us(tid, ticks))
        .max()
        .unwrap_or(0)
}

/// Segment-boundary test for audio-only streams (no video reference track).
///
/// Spec (`canonical-form.md § Segmentation Rule`): audio-only streams cut at
/// 1-second wall-clock spans. `track_durations` holds each track's media
/// ticks accumulated since the last flush; the reference clock is the
/// smallest-id track present. Returns `true` once the reference track has
/// accumulated at least one second, meaning the *next* frame should open a
/// new segment (mirroring how a video keyframe opens the next GoP). Returns
/// `false` right after a flush (the map is empty) so a fresh segment never
/// cuts on its very first frame.
pub(crate) fn audio_only_boundary(
    track_durations: &BTreeMap<u32, u64>,
    track_timescales: &BTreeMap<u32, u32>,
) -> bool {
    match track_durations.iter().next() {
        Some((tid, &ticks)) => {
            let ts = track_timescales.get(tid).copied().unwrap_or(0);
            ts > 0 && ticks >= ts as u64
        }
        None => false,
    }
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
    fn test_segment_fmp4_produces_bundled_gop_segments() {
        let data = read_fixture("h264-opus-frag.mp4");
        let mut gops = Vec::new();
        let catalog = segment_fmp4(&mut Cursor::new(&data), |gop| {
            gops.push(gop);
            Ok(())
        })
        .unwrap();

        let num_tracks = catalog.video_configs().count() + catalog.audio_configs().count();
        assert!(num_tracks > 1, "fixture should have multiple tracks");
        assert!(!gops.is_empty(), "should produce GOP segments");

        // Each GOP should bundle all tracks
        for (i, gop) in gops.iter().enumerate() {
            assert_eq!(gop.number, (i + 1) as u32);
            assert_eq!(
                gop.tracks.len(),
                num_tracks,
                "GOP {} should have all tracks",
                gop.number
            );
            // Track IDs should be sorted ascending (BTreeMap guarantees this)
            let track_ids: Vec<u32> = gop.tracks.keys().copied().collect();
            let mut sorted = track_ids.clone();
            sorted.sort();
            assert_eq!(track_ids, sorted);
        }
    }

    #[test]
    fn test_segment_data_is_all_frame_data() {
        // Total bytes across all per-track segments should equal total frame
        // bytes + the leading muxl uuid box on every (track, GoP) chunk.
        let data = read_fixture("h264-opus-frag.mp4");

        let mut segment_total = 0usize;
        let mut uuid_overhead = 0usize;
        let catalog = segment_fmp4(&mut Cursor::new(&data), |gop| {
            segment_total += gop.tracks.values().map(|d| d.len()).sum::<usize>();
            Ok(())
        })
        .unwrap();
        // Reconstruct expected uuid overhead: one prefix per non-empty
        // (track, GoP) pair, prefix length per-track from filter_to_track.
        for tid in catalog
            .video_configs()
            .map(|v| v.track_id())
            .chain(catalog.audio_configs().map(|a| a.track_id()))
        {
            let prefix = mint_canonical_segment_prefix(&catalog, tid).unwrap();
            let mut gop_count = 0usize;
            segment_fmp4(&mut Cursor::new(&data), |gop| {
                if gop.tracks.contains_key(&tid) {
                    gop_count += 1;
                }
                Ok(())
            })
            .unwrap();
            uuid_overhead += prefix.len() * gop_count;
        }

        let mut frame_total = 0usize;
        crate::fragment::fragment_fmp4(&mut Cursor::new(&data), |frame| {
            frame_total += frame.data.len();
            Ok(())
        })
        .unwrap();

        assert_eq!(
            segment_total,
            frame_total + uuid_overhead,
            "segment bytes should equal frame bytes plus per-(track, GoP) muxl uuid prefixes"
        );
    }

    #[test]
    fn test_per_track_fmp4_is_parseable() {
        let data = read_fixture("h264-opus-frag.mp4");

        let mut gops = Vec::new();
        let catalog = segment_fmp4(&mut Cursor::new(&data), |gop| {
            gops.push(gop);
            Ok(())
        })
        .unwrap();

        let init = build_init_segment(&catalog).unwrap();

        // Build per-track fMP4
        let mut track_ids: Vec<u32> = gops
            .iter()
            .flat_map(|g| g.tracks.keys().copied())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        track_ids.sort();

        let mut fmp4 = init;
        for &tid in &track_ids {
            for gop in &gops {
                if let Some(data) = gop.tracks.get(&tid) {
                    fmp4.extend_from_slice(data);
                }
            }
        }

        let fmp4_catalog = crate::init::catalog_from_mp4(Cursor::new(&fmp4)).unwrap();
        assert_eq!(catalog.video_configs().count(), fmp4_catalog.video_configs().count());
        assert_eq!(catalog.audio_configs().count(), fmp4_catalog.audio_configs().count());
    }

    #[test]
    fn remap_track_ids_roundtrips_through_canonicalize() {
        // Read a multi-track fixture, remap the audio track to a fresh free
        // id, canonicalize, and confirm the re-read catalog reflects the new
        // id with no collision — proving the rewrite reaches both the moov
        // and the minted moofs. This is the primitive Streamplace uses to
        // give a transcoded rendition a free id before transcode-signing.
        let flat = read_fixture("h264-aac.mp4");
        let mut source = crate::read(&flat).unwrap();

        let audio_tid = source.catalog.audio_configs().next().unwrap().track_id();
        let new_tid = 99u32;
        let map: BTreeMap<u32, u32> = [(audio_tid, new_tid)].into_iter().collect();
        source.remap_track_ids(&map);

        let mut fmp4 = Vec::new();
        crate::fmp4::write(&source, &flat, &mut fmp4).unwrap();

        let cat = crate::init::catalog_from_mp4(Cursor::new(&fmp4)).unwrap();
        let audio_ids: Vec<u32> = cat.audio_configs().map(|a| a.track_id()).collect();
        assert!(audio_ids.contains(&new_tid), "audio remapped to {new_tid}, got {audio_ids:?}");
        assert!(!audio_ids.contains(&audio_tid), "old audio id {audio_tid} must be gone");
        for v in cat.video_configs() {
            assert_ne!(v.track_id(), new_tid, "video must not collide with remapped audio");
        }

        // The minted moofs must carry the new track id too (not just the moov).
        use mp4_atom::{Atom, Header, Moof, ReadAtom, ReadFrom};
        let mut cursor = Cursor::new(&fmp4);
        let mut saw_remapped_moof = false;
        while cursor.position() < fmp4.len() as u64 {
            let Ok(Some(h)) = <Option<Header> as ReadFrom>::read_from(&mut cursor) else { break };
            if h.kind == Moof::KIND {
                let moof = Moof::read_atom(&h, &mut cursor).unwrap();
                for traf in &moof.traf {
                    assert_ne!(traf.tfhd.track_id, audio_tid, "no moof should keep the old audio id");
                    if traf.tfhd.track_id == new_tid {
                        saw_remapped_moof = true;
                    }
                }
            } else {
                cursor.set_position(cursor.position() + h.size.unwrap_or(0) as u64);
            }
        }
        assert!(saw_remapped_moof, "expected at least one moof at the remapped audio id {new_tid}");
    }

    #[test]
    fn segment_with_remap_relabels_catalog_and_moofs() {
        // The streaming-segmenter equivalent of the fmp4 remap: segment a
        // canonical fMP4 with the audio track relabeled to a free id, and
        // confirm the emitted catalog, the per-segment keying, and the minted
        // moofs all carry the new id. This is what Streamplace's transcode
        // Pass 1 uses to land the transcoded audio at a collision-free id.
        let flat = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&flat).unwrap();
        let mut fmp4 = Vec::new();
        crate::fmp4::write(&source, &flat, &mut fmp4).unwrap();

        let audio_tid = source.catalog.audio_configs().next().unwrap().track_id();
        let new_tid = 7u32;
        let map: BTreeMap<u32, u32> = [(audio_tid, new_tid)].into_iter().collect();

        let mut new_tid_buf: Vec<u8> = Vec::new();
        let mut seg_track_ids: std::collections::BTreeSet<u32> = Default::default();
        let cat = crate::segment_fmp4_with_remap(&mut Cursor::new(&fmp4), map, |gop| {
            for (tid, data) in &gop.tracks {
                seg_track_ids.insert(*tid);
                if *tid == new_tid && new_tid_buf.is_empty() {
                    new_tid_buf = data.clone();
                }
            }
            Ok(())
        })
        .unwrap();

        let audio_ids: Vec<u32> = cat.audio_configs().map(|a| a.track_id()).collect();
        assert!(audio_ids.contains(&new_tid), "audio remapped to {new_tid}, got {audio_ids:?}");
        assert!(!audio_ids.contains(&audio_tid), "old audio id {audio_tid} must be gone");
        assert!(seg_track_ids.contains(&new_tid), "segment tracks: {seg_track_ids:?}");
        assert!(!seg_track_ids.contains(&audio_tid), "old id leaked into segment keys: {seg_track_ids:?}");

        // The minted moofs in the remapped track carry the new id.
        use mp4_atom::{Atom, Header, Moof, ReadAtom, ReadFrom};
        assert!(!new_tid_buf.is_empty(), "expected a segment for the remapped track");
        let mut cursor = Cursor::new(&new_tid_buf);
        let mut saw_moof = false;
        while cursor.position() < new_tid_buf.len() as u64 {
            let Ok(Some(h)) = <Option<Header> as ReadFrom>::read_from(&mut cursor) else { break };
            if h.kind == Moof::KIND {
                let moof = Moof::read_atom(&h, &mut cursor).unwrap();
                for traf in &moof.traf {
                    assert_eq!(traf.tfhd.track_id, new_tid, "moof must carry the remapped id");
                }
                saw_moof = true;
            } else {
                cursor.set_position(cursor.position() + h.size.unwrap_or(0) as u64);
            }
        }
        assert!(saw_moof, "expected at least one moof in the remapped track's segment");
    }

    #[test]
    fn test_video_segments_start_with_keyframe() {
        let data = read_fixture("h264-opus-frag.mp4");

        let mut gops = Vec::new();
        let catalog = segment_fmp4(&mut Cursor::new(&data), |gop| {
            gops.push(gop);
            Ok(())
        })
        .unwrap();

        let video_track_ids: HashSet<u32> = catalog.video_configs().map(|v| v.track_id()).collect();

        for gop in &gops {
            for (&track_id, track_data) in &gop.tracks {
                if !video_track_ids.contains(&track_id) {
                    continue;
                }
                use mp4_atom::{Atom, Header, Moof, ReadAtom, ReadFrom};

                let mut cursor = Cursor::new(track_data);
                // Skip the leading muxl uuid box.
                let uuid_h = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                    .unwrap()
                    .unwrap();
                assert_eq!(uuid_h.kind, mp4_atom::FourCC::new(b"uuid"));
                let uuid_size = uuid_h.size.unwrap_or(0);
                cursor.set_position(cursor.position() + uuid_size as u64);

                let h = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                    .unwrap()
                    .unwrap();
                assert_eq!(h.kind, Moof::KIND);
                let moof = Moof::read_atom(&h, &mut cursor).unwrap();
                let entry = &moof.traf[0].trun[0].entries[0];
                let flags = entry.flags.unwrap_or(0);
                let is_sync = (flags & 0x00010000) == 0;
                assert!(
                    is_sync,
                    "video segment (GOP {}, track {}) first frame should be sync",
                    gop.number, track_id
                );
            }
        }
    }

    #[test]
    fn test_each_track_data_has_single_track() {
        let data = read_fixture("h264-opus-frag.mp4");

        let mut gops = Vec::new();
        segment_fmp4(&mut Cursor::new(&data), |gop| {
            gops.push(gop);
            Ok(())
        })
        .unwrap();

        for gop in &gops {
            for (&track_id, track_data) in &gop.tracks {
                use mp4_atom::{Atom, Header, Moof, ReadAtom, ReadFrom};

                let mut cursor = Cursor::new(track_data);
                while cursor.position() < track_data.len() as u64 {
                    let h = match <Option<Header> as ReadFrom>::read_from(&mut cursor) {
                        Ok(Some(h)) => h,
                        _ => break,
                    };
                    if h.kind == Moof::KIND {
                        let moof = Moof::read_atom(&h, &mut cursor).unwrap();
                        for traf in &moof.traf {
                            assert_eq!(
                                traf.tfhd.track_id, track_id,
                                "GOP {} track {} data contains data for track {}",
                                gop.number, track_id, traf.tfhd.track_id
                            );
                        }
                    } else {
                        let size = h.size.unwrap_or(0);
                        cursor.set_position(cursor.position() + size as u64);
                    }
                }
            }
        }
    }

    #[test]
    fn audio_only_boundary_fires_at_one_second() {
        let ts: BTreeMap<u32, u32> = [(1u32, 48_000u32)].into_iter().collect();
        assert!(!audio_only_boundary(&BTreeMap::new(), &ts), "empty (post-flush) → no cut");
        let below: BTreeMap<u32, u64> = [(1u32, 47_040u64)].into_iter().collect();
        assert!(!audio_only_boundary(&below, &ts), "<1s accumulated → no cut");
        let at: BTreeMap<u32, u64> = [(1u32, 48_000u64)].into_iter().collect();
        assert!(audio_only_boundary(&at, &ts), ">=1s accumulated → cut");
    }

    #[test]
    fn test_audio_only_stream_segments_at_one_second() {
        // opus-audio-only.mp4 is a flat ~2s single Opus track. Convert it to
        // fMP4 via muxl, then segment: the audio-only 1-second rule must split
        // it into multiple ~1s segments. Before this fix the streaming
        // segmenter only cut on video keyframes, so an audio-only stream never
        // segmented until EOF (one giant segment).
        let flat = read_fixture("opus-audio-only.mp4");
        let source = crate::read(&flat).unwrap();
        let mut fmp4 = Vec::new();
        crate::fmp4::write(&source, &flat, &mut fmp4).unwrap();

        let mut segs = Vec::new();
        let catalog = segment_fmp4(&mut Cursor::new(&fmp4), |gop| {
            segs.push(gop);
            Ok(())
        })
        .unwrap();

        assert!(
            catalog.video_configs().next().is_none(),
            "fixture must be audio-only for this test"
        );
        assert!(
            segs.len() >= 2,
            "audio-only ~2s stream should split into multiple 1s segments, got {}",
            segs.len()
        );
        // Each non-final segment should span ~1 second.
        for gop in &segs[..segs.len() - 1] {
            assert!(
                gop.duration_us >= 900_000,
                "non-final audio segment should be ~1s, got {} us",
                gop.duration_us
            );
        }
        // Segmentation must not drop or duplicate frames.
        let total: u32 = segs.iter().flat_map(|g| g.sample_counts.values().copied()).sum();
        assert_eq!(total, 101, "all 101 opus frames must survive segmentation");
    }
}
