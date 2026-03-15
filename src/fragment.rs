//! Fragmentation: MP4 → per-frame fMP4 fragments (Hang CMAF style).
//!
//! Two code paths:
//! - **Flat MP4** (`fragment_track`, `fragment_to_directory`): parses moov sample
//!   tables, seeks to sample offsets. Requires `Read + Seek`.
//! - **Streaming fMP4** (`fragment_fmp4`): processes moof+mdat pairs as they
//!   arrive. Only requires `Read`. Suitable for livestream ingest.
//!
//! Both emit single-sample moof+mdat pairs per frame.
//!
//! Spec: architecture.md § Hang CMAF

use std::io::{Read, Seek, SeekFrom, Write};

use mp4_atom::{
    Atom, CttsEntry, Encode, Header, Mfhd, Moof, Moov, ReadAtom, ReadFrom, StscEntry, StszSamples,
    SttsEntry, Tfdt, Tfhd, Traf, Trun, TrunEntry,
};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::init::{catalog_from_moov, read_moov};

// ---------------------------------------------------------------------------
// Shared: write a single-sample moof+mdat fragment
// ---------------------------------------------------------------------------

/// Per-sample metadata used by both flat and streaming paths.
struct FrameInfo {
    duration: u32,
    size: u32,
    is_sync: bool,
    cts_offset: i32,
}

/// Write a single-sample moof+mdat fragment.
///
/// Returns the total bytes written (moof + mdat).
fn write_frame_fragment<W: Write>(
    writer: &mut W,
    sequence_number: u32,
    track_id: u32,
    base_decode_time: u64,
    frame: &FrameInfo,
    sample_data: &[u8],
) -> Result<u64> {
    // Sample flags per ISOBMFF:
    // sync: 0x02000000 (sample_depends_on=2: does not depend on others)
    // non-sync: 0x01010000 (sample_depends_on=1 + sample_is_non_sync=1)
    let sample_flags: u32 = if frame.is_sync {
        0x02000000
    } else {
        0x01010000
    };

    let has_cts = frame.cts_offset != 0;

    let entry = TrunEntry {
        duration: Some(frame.duration),
        size: Some(frame.size),
        flags: Some(sample_flags),
        cts: if has_cts {
            Some(frame.cts_offset)
        } else {
            None
        },
    };

    let moof = Moof {
        mfhd: Mfhd { sequence_number },
        traf: vec![Traf {
            tfhd: Tfhd {
                track_id,
                base_data_offset: None,
                sample_description_index: None,
                default_sample_duration: None,
                default_sample_size: None,
                default_sample_flags: None,
            },
            tfdt: Some(Tfdt {
                base_media_decode_time: base_decode_time,
            }),
            trun: vec![Trun {
                data_offset: Some(0), // placeholder
                entries: vec![entry],
            }],
            ..Default::default()
        }],
    };

    // Encode moof to measure its size
    let mut moof_buf = Vec::new();
    moof.encode(&mut moof_buf).map_err(mp4_err)?;
    let moof_size = moof_buf.len();

    // data_offset = offset from start of moof to start of mdat payload
    // = moof_size + 8 (mdat header)
    let data_offset = (moof_size + 8) as i32;

    // Re-encode with correct data_offset
    let mut moof_patched = moof;
    moof_patched.traf[0].trun[0].data_offset = Some(data_offset);
    let mut moof_buf = Vec::new();
    moof_patched.encode(&mut moof_buf).map_err(mp4_err)?;

    // Write moof
    writer.write_all(&moof_buf)?;

    // Write mdat: 4-byte size (big-endian) + 4-byte type + payload
    let mdat_total_size = 8u32 + sample_data.len() as u32;
    writer.write_all(&mdat_total_size.to_be_bytes())?;
    writer.write_all(b"mdat")?;
    writer.write_all(sample_data)?;

    Ok(moof_buf.len() as u64 + mdat_total_size as u64)
}

fn mp4_err(e: mp4_atom::Error) -> Error {
    Error::InvalidMp4(e.to_string())
}

// ---------------------------------------------------------------------------
// Streaming fMP4 path (no Seek required)
// ---------------------------------------------------------------------------

/// Per-track defaults resolved from trex (moov/mvex) and tfhd (per-fragment).
struct TrackDefaults {
    default_sample_duration: u32,
    default_sample_size: u32,
    default_sample_flags: u32,
}

/// Look up trex defaults for a given track_id.
fn trex_defaults(moov: &Moov, track_id: u32) -> TrackDefaults {
    if let Some(ref mvex) = moov.mvex {
        for trex in &mvex.trex {
            if trex.track_id == track_id {
                return TrackDefaults {
                    default_sample_duration: trex.default_sample_duration,
                    default_sample_size: trex.default_sample_size,
                    default_sample_flags: trex.default_sample_flags,
                };
            }
        }
    }
    TrackDefaults {
        default_sample_duration: 0,
        default_sample_size: 0,
        default_sample_flags: 0,
    }
}

/// Resolve a TrunEntry's effective values using the ISOBMFF default cascade:
/// trun entry > tfhd defaults > trex defaults.
fn resolve_sample(
    entry: &TrunEntry,
    tfhd: &Tfhd,
    trex: &TrackDefaults,
    first_sample: bool,
    first_sample_flags: Option<u32>,
) -> FrameInfo {
    let duration = entry
        .duration
        .or(tfhd.default_sample_duration)
        .unwrap_or(trex.default_sample_duration);
    let size = entry
        .size
        .or(tfhd.default_sample_size)
        .unwrap_or(trex.default_sample_size);

    // For the first sample in a trun, first_sample_flags (from trun header)
    // overrides per-sample flags. Otherwise use entry > tfhd > trex cascade.
    let flags = if first_sample {
        first_sample_flags
            .or(entry.flags)
            .or(tfhd.default_sample_flags)
            .unwrap_or(trex.default_sample_flags)
    } else {
        entry
            .flags
            .or(tfhd.default_sample_flags)
            .unwrap_or(trex.default_sample_flags)
    };

    // Bit 16 of sample_flags: sample_is_non_sync_sample
    let is_sync = (flags & 0x00010000) == 0;

    FrameInfo {
        duration,
        size,
        is_sync,
        cts_offset: entry.cts.unwrap_or(0),
    }
}

/// A per-frame fragment emitted by `fragment_fmp4`.
pub struct Frame {
    /// Track ID this frame belongs to.
    pub track_id: u32,
    /// Whether this is a sync (key) frame.
    pub is_sync: bool,
    /// Encoded moof+mdat bytes for this single frame.
    pub data: Vec<u8>,
}

/// Streaming fMP4 reader that parses the init segment upfront, then
/// yields per-frame fragments on demand.
///
/// Only requires `Read` — no seeking. Suitable for processing a live fMP4
/// stream (e.g. from GStreamer's cmafmux/splitmuxsink).
pub struct FMP4Reader<R> {
    reader: R,
    moov: Moov,
    catalog: Catalog,
    track_state: std::collections::HashMap<u32, (u64, u32)>,
    /// Buffered frames from the current moof+mdat pair (may contain
    /// multiple frames from multiple tracks).
    pending: Vec<Frame>,
}

impl<R: Read> FMP4Reader<R> {
    /// Create a new FMP4Reader, reading the init segment (ftyp+moov).
    pub fn new(mut reader: R) -> Result<Self> {
        let moov = read_moov_streaming(&mut reader)?;
        let catalog = catalog_from_moov(&moov)?;
        Ok(FMP4Reader {
            reader,
            moov,
            catalog,
            track_state: std::collections::HashMap::new(),
            pending: Vec::new(),
        })
    }

    /// The catalog extracted from the init segment.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Read the next per-frame fragment, or None at EOF.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        // Return buffered frames first
        if !self.pending.is_empty() {
            return Ok(Some(self.pending.remove(0)));
        }

        // Read the next moof+mdat pair and buffer its frames
        loop {
            let header =
                match <Option<Header> as ReadFrom>::read_from(&mut self.reader).map_err(mp4_err)? {
                    Some(h) => h,
                    None => return Ok(None), // EOF
                };

            if header.kind == Moof::KIND {
                let moof_box_size = header.size.unwrap_or(0) + 8;
                let moof = Moof::read_atom(&header, &mut self.reader).map_err(mp4_err)?;

                // Next box must be mdat
                let mdat_header = <Option<Header> as ReadFrom>::read_from(&mut self.reader)
                    .map_err(mp4_err)?
                    .ok_or_else(|| Error::InvalidMp4("expected mdat after moof".into()))?;

                if mdat_header.kind != mp4_atom::FourCC::new(b"mdat") {
                    return Err(Error::InvalidMp4(format!(
                        "expected mdat after moof, got {:?}",
                        mdat_header.kind
                    )));
                }

                let mdat_size = mdat_header
                    .size
                    .ok_or_else(|| Error::InvalidMp4("mdat with unknown size".into()))?;
                let mut mdat_data = vec![0u8; mdat_size];
                self.reader.read_exact(&mut mdat_data)?;

                process_moof_mdat(
                    &self.moov,
                    &moof,
                    moof_box_size,
                    &mdat_data,
                    &mut self.track_state,
                    &mut |frame| {
                        self.pending.push(frame);
                        Ok(())
                    },
                )?;

                if !self.pending.is_empty() {
                    return Ok(Some(self.pending.remove(0)));
                }
            } else {
                // Skip non-moof boxes (styp, sidx, free, etc.)
                let size = header
                    .size
                    .ok_or_else(|| Error::InvalidMp4("box with unknown size in stream".into()))?;
                let mut skip = vec![0u8; size];
                self.reader.read_exact(&mut skip)?;
            }
        }
    }
}

/// Process an fMP4 stream, splitting multi-sample moof+mdat pairs into
/// per-frame single-sample fragments.
///
/// Only requires `Read` — no seeking. Suitable for processing a live fMP4
/// stream (e.g. from GStreamer's cmafmux/splitmuxsink).
///
/// Calls `on_frame` for each per-frame fragment with the track_id and the
/// encoded moof+mdat bytes. Returns the catalog extracted from the init
/// segment.
pub fn fragment_fmp4<R: Read>(
    reader: &mut R,
    mut on_frame: impl FnMut(Frame) -> Result<()>,
) -> Result<Catalog> {
    let mut fmp4 = FMP4Reader::new(reader)?;
    let catalog = fmp4.catalog().clone();
    while let Some(frame) = fmp4.next_frame()? {
        on_frame(frame)?;
    }
    Ok(catalog)
}

/// Read boxes from a stream until we find and parse the moov box.
/// Only requires Read, not Seek.
fn read_moov_streaming<R: Read>(reader: &mut R) -> Result<Moov> {
    loop {
        let header = match <Option<Header> as ReadFrom>::read_from(reader).map_err(mp4_err)? {
            Some(h) => h,
            None => return Err(Error::InvalidMp4("moov box not found".into())),
        };

        if header.kind == Moov::KIND {
            return Moov::read_atom(&header, reader).map_err(mp4_err);
        }

        // Skip this box
        let size = header
            .size
            .ok_or_else(|| Error::InvalidMp4("box with unknown size before moov".into()))?;
        let mut skip = vec![0u8; size];
        reader.read_exact(&mut skip)?;
    }
}

/// Process one moof+mdat pair, emitting per-frame fragments.
///
/// `moof_box_size` is the total size of the original moof box (header + body),
/// needed because trun data_offset values are relative to the moof start.
fn process_moof_mdat(
    moov: &Moov,
    moof: &Moof,
    moof_box_size: usize,
    mdat_data: &[u8],
    track_state: &mut std::collections::HashMap<u32, (u64, u32)>,
    on_frame: &mut impl FnMut(Frame) -> Result<()>,
) -> Result<()> {
    // data_offset in trun is relative to the start of the moof box (when
    // default-base-is-moof is set). The mdat payload starts at
    // moof_box_size + 8 (mdat header) from the moof start.
    let mdat_base = moof_box_size + 8;

    for traf in &moof.traf {
        let track_id = traf.tfhd.track_id;
        let trex = trex_defaults(moov, track_id);

        let (decode_time, seq) = track_state.entry(track_id).or_insert((0u64, 0u32));

        // Base decode time from tfdt (if present), otherwise continue from where we left off
        if let Some(ref tfdt) = traf.tfdt {
            *decode_time = tfdt.base_media_decode_time;
        }

        for trun in &traf.trun {
            // Calculate where this trun's sample data starts in mdat_data
            let data_start = match trun.data_offset {
                Some(offset) => {
                    let abs_offset = offset as usize;
                    if abs_offset < mdat_base {
                        return Err(Error::InvalidMp4(
                            "trun data_offset points before mdat".into(),
                        ));
                    }
                    abs_offset - mdat_base
                }
                None => {
                    // No data_offset: data follows immediately after previous trun's data
                    // This is unusual for fMP4 but handle it
                    0
                }
            };

            let mut offset = data_start;
            for (i, entry) in trun.entries.iter().enumerate() {
                let frame = resolve_sample(entry, &traf.tfhd, &trex, i == 0, None);

                let sample_end = offset + frame.size as usize;
                if sample_end > mdat_data.len() {
                    return Err(Error::InvalidMp4(format!(
                        "sample data overflows mdat: offset {offset}+{} > {}",
                        frame.size,
                        mdat_data.len()
                    )));
                }
                let sample_data = &mdat_data[offset..sample_end];

                *seq += 1;
                let mut frag_buf = Vec::new();
                write_frame_fragment(
                    &mut frag_buf,
                    *seq,
                    track_id,
                    *decode_time,
                    &frame,
                    sample_data,
                )?;

                on_frame(Frame {
                    track_id,
                    is_sync: frame.is_sync,
                    data: frag_buf,
                })?;

                *decode_time += frame.duration as u64;
                offset = sample_end;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Flat MP4 path (requires Seek)
// ---------------------------------------------------------------------------

/// Per-sample metadata with file offset, for flat MP4 fragmentation.
struct FlatSampleInfo {
    frame: FrameInfo,
    /// File offset where this sample's data starts.
    file_offset: u64,
}

/// Extract per-sample metadata from a flat MP4 track's sample tables.
fn extract_flat_track_info(trak: &mp4_atom::Trak) -> Result<Vec<FlatSampleInfo>> {
    let stbl = &trak.mdia.minf.stbl;

    // Sample sizes
    let sizes: Vec<u32> = match &stbl.stsz.samples {
        StszSamples::Different { sizes } => sizes.clone(),
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
    };
    let sample_count = sizes.len();

    // Sample durations from stts (run-length encoded)
    let durations = expand_stts(&stbl.stts.entries, sample_count);

    // Composition time offsets from ctts
    let cts_offsets = match &stbl.ctts {
        Some(ctts) => expand_ctts(&ctts.entries, sample_count),
        None => vec![0i32; sample_count],
    };

    // Sync samples from stss (None = all sync)
    let sync_set: Option<std::collections::HashSet<u32>> = stbl
        .stss
        .as_ref()
        .map(|stss| stss.entries.iter().copied().collect());

    // Chunk offsets from stco or co64
    let chunk_offsets: Vec<u64> = if let Some(ref stco) = stbl.stco {
        stco.entries.iter().map(|&o| o as u64).collect()
    } else if let Some(ref co64) = stbl.co64 {
        co64.entries.clone()
    } else {
        return Err(Error::InvalidMp4("no stco or co64 box".into()));
    };

    let file_offsets = resolve_sample_offsets(&stbl.stsc.entries, &chunk_offsets, &sizes)?;

    let mut samples = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let is_sync = match &sync_set {
            Some(set) => set.contains(&(i as u32 + 1)),
            None => true,
        };
        samples.push(FlatSampleInfo {
            frame: FrameInfo {
                duration: durations[i],
                size: sizes[i],
                is_sync,
                cts_offset: cts_offsets[i],
            },
            file_offset: file_offsets[i],
        });
    }

    Ok(samples)
}

/// Resolve stsc entries + chunk offsets + sample sizes into per-sample file offsets.
fn resolve_sample_offsets(
    stsc_entries: &[StscEntry],
    chunk_offsets: &[u64],
    sample_sizes: &[u32],
) -> Result<Vec<u64>> {
    let sample_count = sample_sizes.len();
    if sample_count == 0 {
        return Ok(vec![]);
    }
    if stsc_entries.is_empty() {
        return Err(Error::InvalidMp4("empty stsc table".into()));
    }

    let mut offsets = Vec::with_capacity(sample_count);
    let mut sample_idx = 0usize;

    for (i, entry) in stsc_entries.iter().enumerate() {
        let first_chunk = entry.first_chunk as usize;
        let next_first_chunk = if i + 1 < stsc_entries.len() {
            stsc_entries[i + 1].first_chunk as usize
        } else {
            chunk_offsets.len() + 1
        };

        for chunk_idx in first_chunk..next_first_chunk {
            if chunk_idx < 1 || chunk_idx > chunk_offsets.len() {
                break;
            }
            let mut offset = chunk_offsets[chunk_idx - 1];
            for _ in 0..entry.samples_per_chunk {
                if sample_idx >= sample_count {
                    return Ok(offsets);
                }
                offsets.push(offset);
                offset += sample_sizes[sample_idx] as u64;
                sample_idx += 1;
            }
        }
    }

    Ok(offsets)
}

/// Expand run-length encoded stts entries into per-sample durations.
fn expand_stts(entries: &[SttsEntry], sample_count: usize) -> Vec<u32> {
    let mut durations = Vec::with_capacity(sample_count);
    for entry in entries {
        for _ in 0..entry.sample_count {
            durations.push(entry.sample_delta);
        }
    }
    durations.truncate(sample_count);
    durations
}

/// Expand ctts entries into per-sample composition time offsets.
fn expand_ctts(entries: &[CttsEntry], sample_count: usize) -> Vec<i32> {
    let mut offsets = Vec::with_capacity(sample_count);
    for entry in entries {
        for _ in 0..entry.sample_count {
            offsets.push(entry.sample_offset);
        }
    }
    offsets.truncate(sample_count);
    offsets
}

/// Fragment a single track from a flat MP4 into per-frame moof+mdat pairs.
///
/// Writes concatenated fragments to the writer. Returns the number of samples
/// written.
pub fn fragment_track<RS: Read + Seek, W: Write>(
    mut input: RS,
    track_id: u32,
    writer: &mut W,
) -> Result<u32> {
    let moov = read_moov(&mut input)?;

    let trak = moov
        .trak
        .iter()
        .find(|t| t.tkhd.track_id == track_id)
        .ok_or_else(|| Error::InvalidMp4(format!("track {track_id} not found")))?;

    let samples = extract_flat_track_info(trak)?;
    let sample_count = samples.len() as u32;
    let mut decode_time: u64 = 0;

    for (i, sample) in samples.iter().enumerate() {
        input.seek(SeekFrom::Start(sample.file_offset))?;
        let mut data = vec![0u8; sample.frame.size as usize];
        input.read_exact(&mut data)?;

        write_frame_fragment(
            writer,
            (i as u32) + 1,
            track_id,
            decode_time,
            &sample.frame,
            &data,
        )?;
        decode_time += sample.frame.duration as u64;
    }

    Ok(sample_count)
}

/// Fragment all tracks from a flat MP4 into per-frame moof+mdat pairs,
/// writing each track's fragments to a separate directory.
pub fn fragment_to_directory<RS: Read + Seek>(
    mut input: RS,
    output_dir: &std::path::Path,
) -> Result<FragmentStats> {
    let moov = read_moov(&mut input)?;

    let mut track_ids: Vec<u32> = moov.trak.iter().map(|t| t.tkhd.track_id).collect();
    track_ids.sort();

    let mut stats = FragmentStats { tracks: Vec::new() };

    for &track_id in &track_ids {
        let trak = moov
            .trak
            .iter()
            .find(|t| t.tkhd.track_id == track_id)
            .unwrap();

        let samples = extract_flat_track_info(trak)?;
        let track_dir = output_dir.join(format!("track{}", track_id));
        std::fs::create_dir_all(&track_dir)?;

        let sample_count = samples.len() as u32;
        let mut decode_time: u64 = 0;
        let mut total_bytes: u64 = 0;

        for (i, sample) in samples.iter().enumerate() {
            let sample_id = (i as u32) + 1;

            input.seek(SeekFrom::Start(sample.file_offset))?;
            let mut data = vec![0u8; sample.frame.size as usize];
            input.read_exact(&mut data)?;

            let mut buf = Vec::new();
            write_frame_fragment(
                &mut buf,
                sample_id,
                track_id,
                decode_time,
                &sample.frame,
                &data,
            )?;

            let filename = track_dir.join(format!("{:06}.cmaf", sample_id));
            std::fs::write(&filename, &buf)?;
            total_bytes += buf.len() as u64;
            decode_time += sample.frame.duration as u64;
        }

        let handler = trak.mdia.hdlr.handler;
        stats.tracks.push(TrackStats {
            track_id,
            handler_type: String::from_utf8_lossy(handler.as_ref()).to_string(),
            timescale: trak.mdia.mdhd.timescale,
            sample_count,
            total_bytes,
        });
    }

    Ok(stats)
}

/// Statistics about a fragmentation operation.
pub struct FragmentStats {
    pub tracks: Vec<TrackStats>,
}

pub struct TrackStats {
    pub track_id: u32,
    pub handler_type: String,
    pub timescale: u32,
    pub sample_count: u32,
    pub total_bytes: u64,
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

    // --- Flat MP4 tests ---

    #[test]
    fn test_fragment_track_h264_aac() {
        let data = read_fixture("h264-aac.mp4");
        let moov = read_moov(&mut Cursor::new(&data)).unwrap();

        for trak in &moov.trak {
            let track_id = trak.tkhd.track_id;
            let mut output = Vec::new();
            let count = fragment_track(Cursor::new(&data), track_id, &mut output).unwrap();
            assert!(count > 0, "track {track_id} should have samples");
            assert!(!output.is_empty(), "track {track_id} should produce output");
        }
    }

    #[test]
    fn test_fragment_produces_moof_mdat_pairs() {
        let data = read_fixture("h264-aac.mp4");
        let moov = read_moov(&mut Cursor::new(&data)).unwrap();
        let track_id = moov.trak[0].tkhd.track_id;

        let mut output = Vec::new();
        fragment_track(Cursor::new(&data), track_id, &mut output).unwrap();

        let fragment_count = count_moof_mdat_pairs(&output);
        assert!(fragment_count > 0, "should have produced fragments");
    }

    #[test]
    fn test_fragment_sample_data_preserved() {
        let data = read_fixture("h264-aac.mp4");
        let moov = read_moov(&mut Cursor::new(&data)).unwrap();
        let track_id = moov.trak[0].tkhd.track_id;

        let trak = &moov.trak[0];
        let samples = extract_flat_track_info(trak).unwrap();

        let mut output = Vec::new();
        fragment_track(Cursor::new(&data), track_id, &mut output).unwrap();

        let mut cursor = Cursor::new(&output);
        for (i, sample) in samples.iter().enumerate().take(5) {
            let mut original = vec![0u8; sample.frame.size as usize];
            let mut input = Cursor::new(&data);
            input.seek(SeekFrom::Start(sample.file_offset)).unwrap();
            input.read_exact(&mut original).unwrap();

            // Skip moof
            let h1 = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                .unwrap()
                .unwrap();
            cursor
                .seek(SeekFrom::Current(h1.size.unwrap() as i64))
                .unwrap();

            // Read mdat
            let h2 = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                .unwrap()
                .unwrap();
            let mdat_size = h2.size.unwrap();
            let mut mdat_payload = vec![0u8; mdat_size];
            Read::read_exact(&mut cursor, &mut mdat_payload).unwrap();

            assert_eq!(
                mdat_payload,
                original,
                "sample {} data mismatch in track {}",
                i + 1,
                track_id
            );
        }
    }

    #[test]
    fn test_fragment_to_directory() {
        let data = read_fixture("h264-aac.mp4");
        let tmp = TempDir::new();
        let stats = fragment_to_directory(Cursor::new(data), tmp.path()).unwrap();

        assert!(!stats.tracks.is_empty(), "should have at least one track");

        for track in &stats.tracks {
            let track_dir = tmp.path().join(format!("track{}", track.track_id));
            assert!(track_dir.is_dir());

            let files: Vec<_> = std::fs::read_dir(&track_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "cmaf"))
                .collect();
            assert_eq!(
                files.len(),
                track.sample_count as usize,
                "track {} should have {} fragment files",
                track.track_id,
                track.sample_count
            );
        }
    }

    #[test]
    fn test_fragment_all_flat_fixtures() {
        for name in &[
            "h264-aac.mp4",
            "h264-opus.mp4",
            "h264-aac-25fps.mp4",
            "h264-video-only.mp4",
            "opus-audio-only.mp4",
        ] {
            let data = read_fixture(name);
            let moov = read_moov(&mut Cursor::new(&data)).unwrap();

            for trak in &moov.trak {
                let track_id = trak.tkhd.track_id;
                let mut output = Vec::new();
                let count = fragment_track(Cursor::new(&data), track_id, &mut output)
                    .unwrap_or_else(|e| panic!("{name} track {track_id}: {e}"));
                assert!(count > 0, "{name} track {track_id}: no samples");
            }
        }
    }

    // --- Streaming fMP4 tests ---

    #[test]
    fn test_fragment_fmp4_basic() {
        let data = read_fixture("h264-opus-frag.mp4");
        let mut frames = Vec::new();
        let catalog = fragment_fmp4(&mut Cursor::new(&data), |frame| {
            frames.push(frame);
            Ok(())
        })
        .unwrap();

        assert!(!catalog.video.is_empty(), "should have video tracks");
        assert!(!frames.is_empty(), "should have produced frames");

        // Every frame should be a valid moof+mdat pair
        for (i, frame) in frames.iter().enumerate() {
            let count = count_moof_mdat_pairs(&frame.data);
            assert_eq!(count, 1, "frame {i} should be exactly one moof+mdat pair");
        }
    }

    #[test]
    fn test_fragment_fmp4_sample_data_preserved() {
        // Fragment the fMP4, then verify the total sample data size matches
        // the sum of mdat sizes in the original file.
        let data = read_fixture("h264-opus-frag.mp4");

        // Collect per-track sample data from our fragments
        let mut track_data: std::collections::HashMap<u32, Vec<u8>> =
            std::collections::HashMap::new();
        fragment_fmp4(&mut Cursor::new(&data), |frame| {
            // Extract mdat payload from the fragment
            let mdat_payload = extract_mdat_payload(&frame.data);
            track_data
                .entry(frame.track_id)
                .or_default()
                .extend_from_slice(&mdat_payload);
            Ok(())
        })
        .unwrap();

        // Collect per-track sample data from the original fMP4 by parsing its moof+mdat pairs
        let mut orig_track_data: std::collections::HashMap<u32, Vec<u8>> =
            std::collections::HashMap::new();
        let mut cursor = Cursor::new(&data);
        // Skip to first moof
        let moov = read_moov_streaming(&mut Cursor::new(&data)).unwrap();
        // Re-read from start, skipping init
        cursor.set_position(0);
        loop {
            let header = match <Option<Header> as ReadFrom>::read_from(&mut cursor) {
                Ok(Some(h)) => h,
                _ => break,
            };
            if header.kind == Moof::KIND {
                let moof_box_size = header.size.unwrap() + 8;
                let moof = Moof::read_atom(&header, &mut cursor).unwrap();
                // Read mdat
                let mdat_h = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                    .unwrap()
                    .unwrap();
                let mdat_size = mdat_h.size.unwrap();
                let mut mdat_buf = vec![0u8; mdat_size];
                cursor.read_exact(&mut mdat_buf).unwrap();

                // Split mdat by traf
                let mdat_base = moof_box_size + 8;

                for traf in &moof.traf {
                    let trex_def = trex_defaults(&moov, traf.tfhd.track_id);
                    for trun in &traf.trun {
                        let data_start = match trun.data_offset {
                            Some(off) => (off as usize) - mdat_base,
                            None => 0,
                        };
                        let mut off = data_start;
                        for (i, entry) in trun.entries.iter().enumerate() {
                            let fi = resolve_sample(entry, &traf.tfhd, &trex_def, i == 0, None);
                            orig_track_data
                                .entry(traf.tfhd.track_id)
                                .or_default()
                                .extend_from_slice(&mdat_buf[off..off + fi.size as usize]);
                            off += fi.size as usize;
                        }
                    }
                }
            } else {
                let size = header.size.unwrap_or(0);
                cursor.seek(SeekFrom::Current(size as i64)).unwrap();
            }
        }

        // Verify all track data matches
        for (track_id, orig) in &orig_track_data {
            let ours = track_data
                .get(track_id)
                .unwrap_or_else(|| panic!("missing track {track_id} in output"));
            assert_eq!(
                ours.len(),
                orig.len(),
                "track {track_id}: data length mismatch"
            );
            assert_eq!(ours, orig, "track {track_id}: sample data content mismatch");
        }
    }

    #[test]
    fn test_fragment_fmp4_flat_round_trip_same_data() {
        // For h264-aac: flat-fragment it, then reassemble as fMP4, then
        // streaming-fragment that, and verify sample data matches.
        let data = read_fixture("h264-aac.mp4");
        let moov = read_moov(&mut Cursor::new(&data)).unwrap();

        // Collect sample data per track via flat fragmentation
        let mut flat_samples: std::collections::HashMap<u32, Vec<Vec<u8>>> =
            std::collections::HashMap::new();

        for trak in &moov.trak {
            let track_id = trak.tkhd.track_id;
            let mut output = Vec::new();
            fragment_track(Cursor::new(&data), track_id, &mut output).unwrap();

            let payloads = extract_all_mdat_payloads(&output);
            flat_samples.insert(track_id, payloads);
        }

        // Verify we got some data
        for (track_id, samples) in &flat_samples {
            assert!(!samples.is_empty(), "track {track_id}: no flat samples");
        }
    }

    // --- Test helpers ---

    fn count_moof_mdat_pairs(data: &[u8]) -> u32 {
        let mut cursor = Cursor::new(data);
        let mut count = 0u32;
        while cursor.position() < data.len() as u64 {
            let h1 = match <Option<Header> as ReadFrom>::read_from(&mut cursor) {
                Ok(Some(h)) => h,
                _ => break,
            };
            if h1.kind != Moof::KIND {
                break;
            }
            cursor
                .seek(SeekFrom::Current(h1.size.unwrap() as i64))
                .unwrap();

            let h2 = match <Option<Header> as ReadFrom>::read_from(&mut cursor) {
                Ok(Some(h)) => h,
                _ => break,
            };
            cursor
                .seek(SeekFrom::Current(h2.size.unwrap() as i64))
                .unwrap();
            count += 1;
        }
        count
    }

    fn extract_mdat_payload(fragment: &[u8]) -> Vec<u8> {
        let mut cursor = Cursor::new(fragment);
        // Skip moof
        let h1 = <Option<Header> as ReadFrom>::read_from(&mut cursor)
            .unwrap()
            .unwrap();
        cursor
            .seek(SeekFrom::Current(h1.size.unwrap() as i64))
            .unwrap();
        // Read mdat
        let h2 = <Option<Header> as ReadFrom>::read_from(&mut cursor)
            .unwrap()
            .unwrap();
        let mut payload = vec![0u8; h2.size.unwrap()];
        cursor.read_exact(&mut payload).unwrap();
        payload
    }

    fn extract_all_mdat_payloads(data: &[u8]) -> Vec<Vec<u8>> {
        let mut cursor = Cursor::new(data);
        let mut payloads = Vec::new();
        while cursor.position() < data.len() as u64 {
            let h1 = match <Option<Header> as ReadFrom>::read_from(&mut cursor) {
                Ok(Some(h)) => h,
                _ => break,
            };
            cursor
                .seek(SeekFrom::Current(h1.size.unwrap() as i64))
                .unwrap();
            let h2 = match <Option<Header> as ReadFrom>::read_from(&mut cursor) {
                Ok(Some(h)) => h,
                _ => break,
            };
            let mut payload = vec![0u8; h2.size.unwrap()];
            cursor.read_exact(&mut payload).unwrap();
            payloads.push(payload);
        }
        payloads
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("muxl-test-{}", std::process::id()));
            path.push(format!(
                "{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
