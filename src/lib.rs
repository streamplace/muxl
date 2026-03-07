use std::io::{self, Read, Seek, SeekFrom, Write};

use mp4::{BoxHeader, BoxType, FtypBox, Mp4Reader, MoovBox, TrakBox, WriteBox};

/// Errors returned by muxl operations.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Mp4(mp4::Error),
    InvalidMp4(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Mp4(e) => write!(f, "MP4 error: {e}"),
            Error::InvalidMp4(msg) => write!(f, "invalid MP4: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<mp4::Error> for Error {
    fn from(e: mp4::Error) -> Self {
        Error::Mp4(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// Resolve the sample_description_index for every sample from the stsc table.
// The stsc table uses a run-length style encoding: each entry says "starting at
// first_chunk, each chunk has N samples with description index D". We need to
// expand this to per-sample values. In canonical form each chunk has 1 sample,
// but input files may have arbitrary chunk layouts.
fn resolve_sample_description_indices(stsc_entries: &[mp4::StscEntry], sample_count: u32) -> Vec<u32> {
    if stsc_entries.is_empty() || sample_count == 0 {
        return vec![1; sample_count as usize]; // default to index 1
    }

    let mut result = Vec::with_capacity(sample_count as usize);
    let mut sample_idx = 0u32;

    for (i, entry) in stsc_entries.iter().enumerate() {
        let next_first_chunk = if i + 1 < stsc_entries.len() {
            stsc_entries[i + 1].first_chunk
        } else {
            // Last entry extends to cover remaining samples.
            // Calculate how many chunks we need.
            let remaining = sample_count - sample_idx;
            let chunks_needed = (remaining + entry.samples_per_chunk - 1) / entry.samples_per_chunk;
            entry.first_chunk + chunks_needed
        };

        for _chunk in entry.first_chunk..next_first_chunk {
            for _s in 0..entry.samples_per_chunk {
                if sample_idx >= sample_count {
                    return result;
                }
                result.push(entry.sample_description_index);
                sample_idx += 1;
            }
        }
    }

    result
}

// Canonical ftyp: isom, minor version 0, compatible brands [isom, iso2, avc1, mp41].
// Spec: canonical-form.md § ftyp
fn canonical_ftyp() -> FtypBox {
    FtypBox {
        major_brand: str::parse("isom").unwrap(),
        minor_version: 0,
        compatible_brands: vec![
            str::parse("isom").unwrap(),
            str::parse("iso2").unwrap(),
            str::parse("avc1").unwrap(),
            str::parse("mp41").unwrap(),
        ],
    }
}

// Canonical media timescales per handler type.
// Spec: canonical-form.md § mdhd
const CANONICAL_VIDEO_TIMESCALE: u32 = 60000;
const CANONICAL_AUDIO_TIMESCALE: u32 = 48000;

// Rescale a value from one timescale to another. Returns None if lossy.
fn rescale_exact(value: u64, old_ts: u64, new_ts: u64) -> Option<u64> {
    if old_ts == new_ts || old_ts == 0 {
        return Some(value);
    }
    let numerator = value * new_ts;
    if numerator % old_ts != 0 {
        None
    } else {
        Some(numerator / old_ts)
    }
}

fn rescale_exact_i32(value: i32, old_ts: u64, new_ts: u64) -> Option<i32> {
    if old_ts == new_ts || old_ts == 0 {
        return Some(value);
    }
    let abs = value.unsigned_abs() as u64;
    let scaled = rescale_exact(abs, old_ts, new_ts)?;
    let result = i32::try_from(scaled).ok()?;
    Some(if value < 0 { -result } else { result })
}

// Attempt to rescale a track's media timescale to the canonical value.
// Returns Ok(true) if rescaled, Ok(false) if already canonical, Err if lossy.
fn rescale_track_timescale(trak: &mut TrakBox, canonical_ts: u32) -> Result<bool> {
    let old_ts = trak.mdia.mdhd.timescale as u64;
    let new_ts = canonical_ts as u64;
    if old_ts == new_ts {
        return Ok(false);
    }

    // Check all stts deltas
    for entry in &trak.mdia.minf.stbl.stts.entries {
        if rescale_exact(entry.sample_delta as u64, old_ts, new_ts).is_none() {
            return Err(Error::InvalidMp4(format!(
                "cannot losslessly rescale stts delta {} from timescale {old_ts} to {new_ts}",
                entry.sample_delta
            )));
        }
    }

    // Check all ctts offsets
    if let Some(ref ctts) = trak.mdia.minf.stbl.ctts {
        for entry in &ctts.entries {
            if rescale_exact_i32(entry.sample_offset, old_ts, new_ts).is_none() {
                return Err(Error::InvalidMp4(format!(
                    "cannot losslessly rescale ctts offset {} from timescale {old_ts} to {new_ts}",
                    entry.sample_offset
                )));
            }
        }
    }

    // Check mdhd duration
    if rescale_exact(trak.mdia.mdhd.duration, old_ts, new_ts).is_none() {
        return Err(Error::InvalidMp4(format!(
            "cannot losslessly rescale mdhd duration {} from timescale {old_ts} to {new_ts}",
            trak.mdia.mdhd.duration
        )));
    }

    // Check elst media_time entries
    if let Some(ref edts) = trak.edts {
        if let Some(ref elst) = edts.elst {
            for entry in &elst.entries {
                // media_time == u32::MAX or u64::MAX means "empty edit", don't scale
                if entry.media_time != u32::MAX as u64 && entry.media_time != u64::MAX {
                    if rescale_exact(entry.media_time, old_ts, new_ts).is_none() {
                        return Err(Error::InvalidMp4(format!(
                            "cannot losslessly rescale elst media_time {} from timescale {old_ts} to {new_ts}",
                            entry.media_time
                        )));
                    }
                }
            }
        }
    }

    // All checks passed — apply the rescaling.
    for entry in &mut trak.mdia.minf.stbl.stts.entries {
        entry.sample_delta = rescale_exact(entry.sample_delta as u64, old_ts, new_ts).unwrap() as u32;
    }
    if let Some(ref mut ctts) = trak.mdia.minf.stbl.ctts {
        for entry in &mut ctts.entries {
            entry.sample_offset = rescale_exact_i32(entry.sample_offset, old_ts, new_ts).unwrap();
        }
    }
    trak.mdia.mdhd.duration = rescale_exact(trak.mdia.mdhd.duration, old_ts, new_ts).unwrap();
    trak.mdia.mdhd.timescale = canonical_ts;

    if let Some(ref mut edts) = trak.edts {
        if let Some(ref mut elst) = edts.elst {
            for entry in &mut elst.entries {
                if entry.media_time != u32::MAX as u64 && entry.media_time != u64::MAX {
                    entry.media_time = rescale_exact(entry.media_time, old_ts, new_ts).unwrap();
                }
            }
        }
    }

    Ok(true)
}

// Canonicalize moov in-place: zero timestamps, canonical hdlr names, strip udta/meta.
// Spec: canonical-form.md § moov, mvhd, tkhd, mdhd, hdlr
fn canonicalize_moov(moov: &mut MoovBox) -> Result<()> {
    // mvhd: zero timestamps, version 0, timescale 1000, flags 0
    moov.mvhd.version = 0;
    moov.mvhd.flags = 0;
    moov.mvhd.creation_time = 0;
    moov.mvhd.modification_time = 0;
    // Normalize movie timescale to 1000 and recompute duration.
    let old_movie_timescale = moov.mvhd.timescale as u64;
    let new_timescale = 1000u64;
    if old_movie_timescale != new_timescale && old_movie_timescale != 0 {
        moov.mvhd.duration = moov.mvhd.duration * new_timescale / old_movie_timescale;
    }
    moov.mvhd.timescale = new_timescale as u32;

    // Sort tracks by track_id for deterministic order.
    moov.traks.sort_by_key(|t| t.tkhd.track_id);

    for trak in &mut moov.traks {
        // tkhd: zero timestamps, flags = 3 (enabled + in_movie), version 0
        trak.tkhd.version = 0;
        trak.tkhd.flags = 3; // track_enabled | track_in_movie
        trak.tkhd.creation_time = 0;
        trak.tkhd.modification_time = 0;

        // Rescale elst segment_duration from old movie timescale to new
        if let Some(ref mut edts) = trak.edts {
            if let Some(ref mut elst) = edts.elst {
                for entry in &mut elst.entries {
                    if old_movie_timescale != 0 {
                        entry.segment_duration =
                            entry.segment_duration * new_timescale / old_movie_timescale;
                    }
                }
            }
        }

        // Recompute tkhd.duration in new movie timescale
        let media_timescale = trak.mdia.mdhd.timescale as u64;
        let media_duration = trak.mdia.mdhd.duration;
        if media_timescale != 0 {
            trak.tkhd.duration = media_duration * new_timescale / media_timescale;
        }

        // mdhd: zero timestamps, version 0, preserve timescale/duration/language
        trak.mdia.mdhd.version = 0;
        trak.mdia.mdhd.flags = 0;
        trak.mdia.mdhd.creation_time = 0;
        trak.mdia.mdhd.modification_time = 0;

        // hdlr: canonical handler names
        trak.mdia.hdlr.version = 0;
        trak.mdia.hdlr.flags = 0;
        let handler_type: String = trak.mdia.hdlr.handler_type.to_string();
        trak.mdia.hdlr.name = match handler_type.as_str() {
            "vide" => "VideoHandler".to_string(),
            "soun" => "SoundHandler".to_string(),
            "sbtl" | "text" => "SubtitleHandler".to_string(),
            _ => String::new(),
        };

        // Normalize media timescale to canonical value.
        // Spec: canonical-form.md § mdhd
        let canonical_ts = match handler_type.as_str() {
            "vide" => Some(CANONICAL_VIDEO_TIMESCALE),
            "soun" => Some(CANONICAL_AUDIO_TIMESCALE),
            _ => None,
        };
        if let Some(ts) = canonical_ts {
            rescale_track_timescale(trak, ts)?;
        }

        // Strip trak-level meta
        trak.meta = None;
    }

    // Recompute next_track_id
    moov.mvhd.next_track_id = moov
        .traks
        .iter()
        .map(|t| t.tkhd.track_id)
        .max()
        .unwrap_or(0)
        + 1;

    // Recompute mvhd.duration as max of track durations (in movie timescale)
    moov.mvhd.duration = moov.traks.iter().map(|t| t.tkhd.duration).max().unwrap_or(0);

    // Strip udta and moov-level meta (tool tags, etc.)
    moov.udta = None;
    moov.meta = None;

    Ok(())
}

/// Transform an arbitrary MP4 into MUXL canonical form.
///
/// Reads a complete MP4 from `input` and writes the canonicalized MP4 to `output`.
/// The output is byte-deterministic: the same logical content always produces
/// identical bytes.
pub fn canonicalize<RS: Read + Seek, WS: Write + Seek>(mut input: RS, mut output: WS) -> Result<()> {
    let end = input.seek(SeekFrom::End(0))?;
    input.seek(SeekFrom::Start(0))?;
    let mut reader = Mp4Reader::read_header(input, end)?;
    canonicalize_from_reader(&mut reader, &mut output)
}

fn canonicalize_from_reader<RS: Read + Seek, WS: Write + Seek>(
    reader: &mut Mp4Reader<RS>,
    writer: &mut WS,
) -> Result<()> {
    // 1. Write canonical ftyp
    let ftyp = canonical_ftyp();
    ftyp.write_box(writer)?;

    // 2. Write mdat with placeholder size, then copy all samples per-track
    let mdat_pos = writer.stream_position()?;
    // Placeholder mdat header (will be fixed later)
    BoxHeader::new(BoxType::MdatBox, 0).write(writer)?;

    let mut moov = reader.moov.clone();

    // Collect track IDs sorted
    let mut track_ids: Vec<u32> = reader.tracks().keys().copied().collect();
    track_ids.sort();

    // For each track, write all samples sequentially. Record chunk offsets.
    // Canonical layout: one sample per chunk, tracks written sequentially.
    for &track_id in &track_ids {
        let sample_count = reader.sample_count(track_id)?;

        let mut chunk_offsets: Vec<u64> = Vec::with_capacity(sample_count as usize);

        for sample_id in 1..=sample_count {
            let offset = writer.stream_position()?;
            chunk_offsets.push(offset);

            let sample = reader
                .read_sample(track_id, sample_id)?
                .ok_or_else(|| Error::InvalidMp4(format!("missing sample {sample_id} in track {track_id}")))?;
            writer.write_all(&sample.bytes)?;
        }

        // Update stbl for this track: one sample per chunk
        let trak = moov
            .traks
            .iter_mut()
            .find(|t| t.tkhd.track_id == track_id)
            .unwrap();

        // stsc: one sample per chunk, preserving sample_description_index changes.
        // Spec: canonical-form.md § Multiple Sample Descriptions
        // Walk the original stsc to find what sample_description_index each sample uses,
        // then emit a new stsc entry at each index transition.
        let sample_desc_indices = resolve_sample_description_indices(
            &trak.mdia.minf.stbl.stsc.entries,
            sample_count,
        );
        trak.mdia.minf.stbl.stsc.entries.clear();
        let mut current_desc_idx = 0u32;
        let mut first_sample_in_run = 1u32;
        for (i, &desc_idx) in sample_desc_indices.iter().enumerate() {
            let sample_num = (i as u32) + 1; // 1-based
            if desc_idx != current_desc_idx {
                if current_desc_idx != 0 {
                    trak.mdia.minf.stbl.stsc.entries.push(mp4::StscEntry {
                        first_chunk: first_sample_in_run, // chunk == sample in 1-sample-per-chunk layout
                        samples_per_chunk: 1,
                        sample_description_index: current_desc_idx,
                        first_sample: first_sample_in_run,
                    });
                }
                current_desc_idx = desc_idx;
                first_sample_in_run = sample_num;
            }
        }
        // Emit final run
        if current_desc_idx != 0 {
            trak.mdia.minf.stbl.stsc.entries.push(mp4::StscEntry {
                first_chunk: first_sample_in_run,
                samples_per_chunk: 1,
                sample_description_index: current_desc_idx,
                first_sample: first_sample_in_run,
            });
        }

        // Use stco (32-bit) if all offsets fit, otherwise co64
        let max_offset = chunk_offsets.iter().copied().max().unwrap_or(0);
        if max_offset <= u32::MAX as u64 {
            trak.mdia.minf.stbl.stco = Some(mp4::StcoBox::default());
            trak.mdia.minf.stbl.stco.as_mut().unwrap().entries =
                chunk_offsets.iter().map(|&o| o as u32).collect();
            trak.mdia.minf.stbl.co64 = None;
        } else {
            trak.mdia.minf.stbl.co64 = Some(mp4::Co64Box::default());
            trak.mdia.minf.stbl.co64.as_mut().unwrap().entries = chunk_offsets;
            trak.mdia.minf.stbl.stco = None;
        }
    }

    // 3. Fix mdat size
    let mdat_end = writer.stream_position()?;
    let mdat_size = mdat_end - mdat_pos;
    writer.seek(SeekFrom::Start(mdat_pos))?;
    BoxHeader::new(BoxType::MdatBox, mdat_size).write(writer)?;
    writer.seek(SeekFrom::Start(mdat_end))?;

    // 4. Canonicalize moov metadata
    canonicalize_moov(&mut moov)?;

    // 5. Write moov
    moov.write_box(writer)?;

    Ok(())
}

/// Split a MUXL canonical MP4 into independently-signable segments.
///
/// Reads a canonical MP4 from `input` and writes segments to `output`.
/// The segment format is TBD.
pub fn segment<RS: Read + Seek, WS: Write + Seek>(_input: RS, _output: WS) -> Result<()> {
    todo!("segment: not yet implemented")
}

/// Concatenate MUXL segments into a single canonical MP4.
///
/// Reads segments from `inputs` and writes the combined MP4 to `output`.
/// Per-segment signatures are preserved.
pub fn concatenate<RS: Read + Seek, WS: Write + Seek>(
    _inputs: &mut [RS],
    _output: WS,
) -> Result<()> {
    todo!("concatenate: not yet implemented")
}
