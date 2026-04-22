//! MUXL Flat MP4: canonical MP4 that doubles as HLS byte-range CMAF source.
//!
//! A MUXL flat MP4 is laid out as:
//!
//! ```text
//! ftyp
//! moov       (populated sample tables; NO mvex; faststart)
//! mdat       (64-bit largesize envelope)
//!   [moof+mdat pairs, grouped by track_id asc]
//! ```
//!
//! Two views of the same bytes:
//!
//! - **Flat MP4 view.** The top-level boxes are `ftyp`, `moov`, `mdat`. The
//!   moov has populated `stts`/`ctts`/`stsz`/`stsc`/`co64`/`stss` with
//!   per-sample `co64` entries pointing at sample bytes inside the inner
//!   mdats. Players and tools that don't speak fMP4 (video editors, LosslessCut,
//!   classic MP4 decoders) treat the file as a regular progressive MP4.
//!
//! - **CMAF byte-range view.** Each inner `moof+mdat` pair is a self-contained
//!   CMAF fragment (single-sample, `default-base-is-moof`). HLS byte-range
//!   playlists can reference those pairs directly, and the outer mdat envelope
//!   is invisible to the HLS player.
//!
//! This is essentially OBS Studio's hybrid-MP4 trick, done up-front instead of
//! at finalize and with moov at the start. Writing is two-pass:
//!
//! 1. Walk the sample plan, measuring each `moof`'s encoded size, to compute
//!    per-sample absolute file offsets. Build the populated moov with those
//!    offsets in `co64`.
//! 2. Stream ftyp → moov → mdat envelope header → `moof+mdat` pairs.
//!
//! The moov's encoded size is invariant under `co64` *value* changes (entries
//! are fixed-size u64), so the first encode gives the final size.
//!
//! Spec: canonical-form.md § MUXL Flat MP4

use std::io::Write;

use mp4_atom::{
    Co64, Ctts, CttsEntry, Edts, Elst, ElstEntry, Encode, Ftyp, Mfhd, Moof, Moov, Mvhd, Stbl, Stsc,
    StscEntry, Stss, Stsz, StszSamples, Stts, SttsEntry, Tfdt, Tfhd, Traf, Trak, Trun, TrunEntry,
    WriteTo,
};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::fragment::extract_flat_track_info;
use crate::init::{
    MOVIE_TIMESCALE, build_audio_trak, build_video_trak, read_moov, start_offset_from_trak,
};
use crate::io::{ReadAt, ReadAtCursor};

pub use crate::source::{Sample, TrackPlan};

use crate::source::{Plan, Source};

/// Read a flat MP4 into a [`Source`]. Auto-detects fMP4-bodied inputs and
/// delegates to [`crate::fmp4::read`] for those.
pub fn read<R: ReadAt + ?Sized>(input: &R) -> Result<Source> {
    let (catalog, tracks) = if detect_is_fmp4(input)? {
        plan_from_fmp4(input)?
    } else {
        plan_from_flat_mp4(input)?
    };
    Ok(Source {
        catalog,
        plan: Plan::new(tracks),
    })
}

/// Write a [`Source`] as a canonical MUXL flat MP4.
///
/// `input` is the original ReadAt the source was built from — sample bytes
/// are streamed from it on demand (offsets live in the plan).
pub fn write<R: ReadAt + ?Sized, W: Write>(
    source: &Source,
    input: &R,
    output: &mut W,
) -> Result<FlatMp4Info> {
    write_flat_mp4(&source.catalog, &source.plan.tracks, input, output)
}

/// Metadata returned from a flat MP4 write — file sizes, per-track fragment
/// locations, and everything downstream needs to build HLS byte-range playlists.
#[derive(Debug, Clone)]
pub struct FlatMp4Info {
    pub total_bytes: u64,
    pub mdat_payload_offset: u64,
    pub tracks: std::collections::BTreeMap<u32, FlatTrackInfo>,
}

/// Per-track fragment metadata in a written MUXL flat MP4.
#[derive(Debug, Clone)]
pub struct FlatTrackInfo {
    pub is_video: bool,
    pub timescale: u32,
    /// The absolute offset of the track's first fragment in the output file.
    pub track_offset: u64,
    /// Per-sample fragment locations, in decode order.
    pub fragments: Vec<FlatFragment>,
}

/// A single inner moof+mdat fragment in a MUXL flat MP4.
#[derive(Debug, Clone)]
pub struct FlatFragment {
    /// Absolute file offset of the inner `moof` header.
    pub offset: u64,
    /// Total bytes of the moof+mdat pair.
    pub size: u64,
    /// Sample duration in the track's media timescale.
    pub duration: u32,
    /// Encoded sample size.
    pub sample_size: u32,
    /// Whether the sample is a sync (key) sample.
    pub is_sync: bool,
}

/// Convert a flat MP4 into a canonical MUXL flat MP4.
pub fn flat_mp4_to_flat<R: ReadAt + ?Sized, W: Write>(
    input: &R,
    output: &mut W,
) -> Result<FlatMp4Info> {
    let (catalog, plans) = plan_from_flat_mp4(input)?;
    write_flat_mp4(&catalog, &plans, input, output)
}

/// Build a canonical MUXL flat MP4 from any supported input (flat MP4 or
/// MUXL fMP4). Auto-detects input layout.
pub fn to_flat<R: ReadAt + ?Sized, W: Write>(
    input: &R,
    output: &mut W,
) -> Result<FlatMp4Info> {
    let (catalog, plans) = if detect_is_fmp4(input)? {
        plan_from_fmp4(input)?
    } else {
        plan_from_flat_mp4(input)?
    };
    write_flat_mp4(&catalog, &plans, input, output)
}

/// Scan top-level boxes to decide whether the input has a moof (fMP4) or
/// reaches EOF without finding one (flat MP4).
fn detect_is_fmp4<R: ReadAt + ?Sized>(input: &R) -> Result<bool> {
    use mp4_atom::{FourCC, Header, ReadFrom};
    use std::io::{Seek, SeekFrom};

    let size = input.size().map_err(Error::Io)?;
    let mut cursor = ReadAtCursor::new(input).map_err(Error::Io)?;
    let mut pos: u64 = 0;
    while pos < size {
        cursor.seek(SeekFrom::Start(pos)).map_err(Error::Io)?;
        let header =
            match <Option<Header> as ReadFrom>::read_from(&mut cursor).map_err(mp4_err)? {
                Some(h) => h,
                None => break,
            };
        if header.kind == FourCC::new(b"moof") {
            return Ok(true);
        }
        let body = header
            .size
            .ok_or_else(|| Error::InvalidMp4("open-ended box during detect".into()))?
            as u64;
        let after_header = cursor.seek(SeekFrom::Current(0)).map_err(Error::Io)?;
        pos = after_header + body;
    }
    Ok(false)
}

/// Build a per-track sample plan from a MUXL fMP4 (or any fMP4 stream
/// with `ftyp + moov + [moof+mdat]*` at the top level). Per-sample
/// `input_offset` values point at sample bytes inside the source fMP4's
/// inner mdats.
pub fn plan_from_fmp4<R: ReadAt + ?Sized>(
    input: &R,
) -> Result<(Catalog, Vec<TrackPlan>)> {
    use mp4_atom::{Atom, FourCC, Header, Moof, ReadAtom, ReadFrom};
    use std::io::{Seek, SeekFrom};

    let file_size = input.size().map_err(Error::Io)?;
    let mut cursor = ReadAtCursor::new(input).map_err(Error::Io)?;

    // Parse moov to get catalog + trex defaults.
    let moov = read_moov(&mut cursor)?;
    let catalog = crate::init::catalog_from_moov(&moov)?;
    let video_ids: std::collections::HashSet<u32> =
        catalog.video_configs().map(|v| v.track_id()).collect();

    // Initialize per-track plans.
    //
    // A MUXL fMP4's init segment has no edts/elst (canonical form), so
    // leading presentation offsets are expected to live in the first
    // fragment's tfdt. We recover that below by reading tfdt on each
    // track's first traf.
    let mut track_plans: std::collections::BTreeMap<u32, TrackPlan> =
        std::collections::BTreeMap::new();
    for trak in &moov.trak {
        let tid = trak.tkhd.track_id;
        track_plans.insert(
            tid,
            TrackPlan {
                track_id: tid,
                is_video: video_ids.contains(&tid),
                timescale: trak.mdia.mdhd.timescale,
                samples: Vec::new(),
                start_offset_ticks: 0,
            },
        );
    }

    // Walk top-level boxes. For each moof, parse its trun entries and
    // compute each sample's absolute byte offset in the input.
    cursor.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    let mut pos: u64 = 0;
    while pos < file_size {
        cursor.seek(SeekFrom::Start(pos)).map_err(Error::Io)?;
        let header =
            match <Option<Header> as ReadFrom>::read_from(&mut cursor).map_err(mp4_err)? {
                Some(h) => h,
                None => break,
            };
        let body_size = header
            .size
            .ok_or_else(|| Error::InvalidMp4("open-ended box in fMP4".into()))?
            as u64;
        // `Header::read_from` returns `size` as the body size (excluding the
        // 8-byte header in the normal case). Total box size is body_size + 8,
        // or body_size + 16 if the largesize form was used — but the atom
        // library abstracts that by giving us body size directly. We need the
        // total box size to advance. Compute it from current cursor position.
        let after_header = cursor.seek(SeekFrom::Current(0)).map_err(Error::Io)?;
        let total_box_size = (after_header - pos) + body_size;

        if header.kind == Moof::KIND {
            let moof_abs_offset = pos;
            let moof = Moof::read_atom(&header, &mut cursor).map_err(mp4_err)?;
            // moof's on-disk size (header + body)
            let moof_box_size = total_box_size;

            for traf in &moof.traf {
                let track_id = traf.tfhd.track_id;
                let trex = crate::fragment::trex_defaults(&moov, track_id);

                // First tfdt seen on a track carries the presentation-start
                // offset (MUXL fMP4 canonical form has no elst, so the
                // offset lives here). Subsequent samples' decode times are
                // recovered implicitly from per-sample durations during
                // re-emission, so we only pick up the first one.
                if let Some(tfdt) = traf.tfdt.as_ref() {
                    if let Some(plan) = track_plans.get_mut(&track_id) {
                        if plan.samples.is_empty() {
                            plan.start_offset_ticks = tfdt.base_media_decode_time;
                        }
                    }
                }

                for trun in &traf.trun {
                    // With default-base-is-moof (MUXL fMP4 files), data_offset
                    // is relative to the start of the moof box.
                    let data_offset_in_moof = trun
                        .data_offset
                        .ok_or_else(|| Error::InvalidMp4("trun missing data_offset".into()))?;
                    let mut sample_abs_offset =
                        moof_abs_offset + data_offset_in_moof as u64;

                    for (i, entry) in trun.entries.iter().enumerate() {
                        let frame = crate::fragment::resolve_sample(
                            entry,
                            &traf.tfhd,
                            &trex,
                            i == 0,
                            None,
                        );
                        let plan = track_plans
                            .get_mut(&track_id)
                            .ok_or_else(|| {
                                Error::InvalidMp4(format!(
                                    "trun references unknown track_id {}",
                                    track_id
                                ))
                            })?;
                        plan.samples.push(Sample {
                            duration: frame.duration,
                            size: frame.size,
                            is_sync: frame.is_sync,
                            cts_offset: frame.cts_offset,
                            input_offset: sample_abs_offset,
                        });
                        sample_abs_offset += frame.size as u64;
                    }
                }
            }
            // Advance past moof. The next box should be mdat; skip it.
            pos = moof_abs_offset + moof_box_size;
            // Read mdat header to skip.
            cursor.seek(SeekFrom::Start(pos)).map_err(Error::Io)?;
            if pos < file_size {
                let mdat_hdr = <Option<Header> as ReadFrom>::read_from(&mut cursor)
                    .map_err(mp4_err)?;
                if let Some(h) = mdat_hdr {
                    if h.kind == FourCC::new(b"mdat") {
                        let mdat_body = h
                            .size
                            .ok_or_else(|| Error::InvalidMp4("mdat missing size".into()))?
                            as u64;
                        let header_bytes =
                            cursor.seek(SeekFrom::Current(0)).map_err(Error::Io)? - pos;
                        pos += header_bytes + mdat_body;
                    } else {
                        // Unexpected box after moof; just advance past it.
                        let body = h
                            .size
                            .ok_or_else(|| Error::InvalidMp4("box missing size".into()))?
                            as u64;
                        let header_bytes =
                            cursor.seek(SeekFrom::Current(0)).map_err(Error::Io)? - pos;
                        pos += header_bytes + body;
                    }
                }
            }
        } else {
            // Non-moof top-level box: skip.
            pos += total_box_size;
        }
    }

    let plans: Vec<TrackPlan> = track_plans.into_values().collect();
    Ok((catalog, plans))
}

/// Build a per-track sample plan from a flat (non-fragmented) MP4.
pub fn plan_from_flat_mp4<R: ReadAt + ?Sized>(
    input: &R,
) -> Result<(Catalog, Vec<TrackPlan>)> {
    let mut cursor = ReadAtCursor::new(input).map_err(Error::Io)?;
    let moov = read_moov(&mut cursor)?;
    let catalog = crate::init::catalog_from_moov(&moov)?;

    let video_ids: std::collections::HashSet<u32> =
        catalog.video_configs().map(|v| v.track_id()).collect();

    let movie_ts = moov.mvhd.timescale;
    let mut plans = Vec::new();
    for trak in &moov.trak {
        let track_id = trak.tkhd.track_id;
        let samples = extract_flat_track_info(trak)?;
        let samples = samples
            .into_iter()
            .map(|s| Sample {
                duration: s.frame.duration,
                size: s.frame.size,
                is_sync: s.frame.is_sync,
                cts_offset: s.frame.cts_offset,
                input_offset: s.file_offset,
            })
            .collect();
        // Bake the source track's leading empty edit into the plan — it
        // becomes the track's first-fragment tfdt (and a canonical
        // synthesized elst in the flat MP4 moov). See start_offset_from_trak.
        let start_offset_ticks = start_offset_from_trak(trak, movie_ts);
        plans.push(TrackPlan {
            track_id,
            is_video: video_ids.contains(&track_id),
            timescale: trak.mdia.mdhd.timescale,
            samples,
            start_offset_ticks,
        });
    }
    plans.sort_by_key(|p| p.track_id);

    Ok((catalog, plans))
}

/// Write a canonical MUXL flat MP4.
///
/// Inner moof+mdat pairs are grouped by track_id ascending. Per-sample
/// `co64` entries in the moov point directly at sample bytes inside the
/// inner mdats, making the file a valid flat MP4 to parsers that don't
/// peer into the mdat envelope.
pub fn write_flat_mp4<R: ReadAt + ?Sized, W: Write>(
    catalog: &Catalog,
    plans: &[TrackPlan],
    input: &R,
    output: &mut W,
) -> Result<FlatMp4Info> {
    let mut ordered: Vec<&TrackPlan> = plans.iter().collect();
    ordered.sort_by_key(|p| p.track_id);

    // ftyp — canonical-form.md § ftyp
    let ftyp = Ftyp {
        major_brand: b"muxl".into(),
        minor_version: 0,
        compatible_brands: vec![b"muxl".into(), b"isom".into(), b"iso2".into()],
    };
    let mut ftyp_buf = Vec::new();
    ftyp.write_to(&mut ftyp_buf).map_err(mp4_err)?;
    let ftyp_size = ftyp_buf.len() as u64;

    // Pass 1: measure each inner moof's size so we know per-sample byte layout.
    // mfhd sequence numbers increment globally across all tracks, matching the
    // fMP4 emitter. The initial decode_time per track is the plan's
    // start_offset_ticks — baking any leading-empty-edit presentation offset
    // into the first-fragment tfdt.
    let mut per_sample_moof_sizes: Vec<Vec<u32>> = Vec::with_capacity(ordered.len());
    let mut per_track_byte_totals: Vec<u64> = Vec::with_capacity(ordered.len());
    let mut seq: u32 = 1;
    let mut per_track_final_decode_time: Vec<u64> = vec![0; ordered.len()];

    for (ti, plan) in ordered.iter().enumerate() {
        let mut sizes = Vec::with_capacity(plan.samples.len());
        let mut track_bytes: u64 = 0;
        let mut dt: u64 = plan.start_offset_ticks;

        for sample in &plan.samples {
            let moof_size = measure_frame_moof(seq, plan.track_id, dt, sample)?;
            sizes.push(moof_size);
            track_bytes += (moof_size as u64) + 8 + (sample.size as u64);
            dt += sample.duration as u64;
            seq += 1;
        }
        per_sample_moof_sizes.push(sizes);
        per_track_byte_totals.push(track_bytes);
        per_track_final_decode_time[ti] = dt;
    }

    // Build base traks (tkhd + mdia + empty stbl) from catalog.
    let mut traks: Vec<Trak> = Vec::with_capacity(ordered.len());
    let mut track_ts: Vec<u32> = Vec::with_capacity(ordered.len());
    for plan in &ordered {
        let (trak, timescale) = build_base_trak(catalog, plan.track_id)?;
        traks.push(trak);
        track_ts.push(timescale);
    }

    // Populate sample tables with placeholder co64 (correct length, zero
    // values — moov size is invariant under co64 value changes). Durations
    // exclude the leading start_offset (which lives in the synthesized elst,
    // not in the media timeline) — `mdhd.duration` is the sum of sample
    // durations, `tkhd.duration` is that plus the presentation offset.
    for (ti, plan) in ordered.iter().enumerate() {
        let n = plan.samples.len() as u32;
        populate_stbl(
            &mut traks[ti].mdia.minf.stbl,
            plan,
            &vec![0u64; n as usize],
        );
        let media_duration = per_track_final_decode_time[ti] - plan.start_offset_ticks;
        traks[ti].mdia.mdhd.duration = media_duration;
        // Synthesize a canonical elst for non-zero presentation offsets so
        // flat-MP4 players honor the same start offset baked into the
        // first-fragment tfdt. Spec: canonical-form.md § edts/elst.
        traks[ti].edts =
            build_canonical_elst(plan.start_offset_ticks, media_duration, track_ts[ti]);
        let tkhd_duration = rescale_to_movie(
            media_duration + plan.start_offset_ticks,
            track_ts[ti],
        );
        traks[ti].tkhd.duration = tkhd_duration;
    }

    let movie_duration: u64 = per_track_final_decode_time
        .iter()
        .zip(&track_ts)
        .map(|(md, ts)| rescale_to_movie(*md, *ts))
        .max()
        .unwrap_or(0);

    let max_track_id = ordered.iter().map(|p| p.track_id).max().unwrap_or(0);

    let mvhd = Mvhd {
        creation_time: 0,
        modification_time: 0,
        timescale: MOVIE_TIMESCALE,
        duration: movie_duration,
        rate: 1u16.into(),
        volume: 1u8.into(),
        matrix: Default::default(),
        next_track_id: max_track_id + 1,
    };

    let mut moov = Moov {
        mvhd: mvhd.clone(),
        meta: None,
        mvex: None,
        trak: traks,
        udta: None,
        ainf: None,
    };

    // Encode moov once to measure its size, with placeholder offsets.
    let mut moov_buf = Vec::new();
    moov.write_to(&mut moov_buf).map_err(mp4_err)?;
    let moov_size = moov_buf.len() as u64;

    const ENVELOPE_HEADER_SIZE: u64 = 16; // size=1 + "mdat" + 8-byte largesize
    let mdat_payload_offset = ftyp_size + moov_size + ENVELOPE_HEADER_SIZE;

    // Compute per-sample absolute offsets now that mdat_payload_offset is known.
    // Also build the per-track fragment metadata that HLS needs.
    let mut running = mdat_payload_offset;
    let mut track_info: std::collections::BTreeMap<u32, FlatTrackInfo> =
        std::collections::BTreeMap::new();
    for (ti, plan) in ordered.iter().enumerate() {
        let track_start = running;
        let n = plan.samples.len();
        let mut co64_entries = Vec::with_capacity(n);
        let mut fragments = Vec::with_capacity(n);
        for si in 0..n {
            let moof_size = per_sample_moof_sizes[ti][si];
            let sample_offset = running + moof_size as u64 + 8;
            co64_entries.push(sample_offset);
            let sample = &plan.samples[si];
            let frag_size = moof_size as u64 + 8 + sample.size as u64;
            fragments.push(FlatFragment {
                offset: running,
                size: frag_size,
                duration: sample.duration,
                sample_size: sample.size,
                is_sync: sample.is_sync,
            });
            running += frag_size;
        }
        moov.trak[ti].mdia.minf.stbl.co64 = Some(Co64 {
            entries: co64_entries,
        });
        track_info.insert(
            plan.track_id,
            FlatTrackInfo {
                is_video: plan.is_video,
                timescale: track_ts[ti],
                track_offset: track_start,
                fragments,
            },
        );
    }

    // Re-encode moov with final offsets.
    let mut moov_buf2 = Vec::new();
    moov.write_to(&mut moov_buf2).map_err(mp4_err)?;
    if moov_buf2.len() as u64 != moov_size {
        return Err(Error::InvalidMp4(format!(
            "moov size changed between passes: {} -> {}",
            moov_size,
            moov_buf2.len()
        )));
    }

    // Write ftyp, moov.
    output.write_all(&ftyp_buf)?;
    output.write_all(&moov_buf2)?;

    // Write outer mdat envelope header (64-bit largesize).
    let total_payload: u64 = per_track_byte_totals.iter().sum();
    let largesize: u64 = ENVELOPE_HEADER_SIZE + total_payload;
    output.write_all(&1u32.to_be_bytes())?;
    output.write_all(b"mdat")?;
    output.write_all(&largesize.to_be_bytes())?;

    // Write inner moof+mdat pairs. Use the same seq-number cadence as pass 1.
    let mut io_buf = vec![0u8; 256 * 1024];
    let mut seq: u32 = 1;
    for (ti, plan) in ordered.iter().enumerate() {
        let mut dt: u64 = plan.start_offset_ticks;
        for (si, sample) in plan.samples.iter().enumerate() {
            let size = sample.size as usize;
            if io_buf.len() < size {
                io_buf.resize(size, 0);
            }
            input
                .read_exact_at(sample.input_offset, &mut io_buf[..size])
                .map_err(Error::Io)?;

            write_frame_pair(
                output,
                seq,
                plan.track_id,
                dt,
                sample,
                &io_buf[..size],
                per_sample_moof_sizes[ti][si],
            )?;

            dt += sample.duration as u64;
            seq += 1;
        }
    }

    Ok(FlatMp4Info {
        total_bytes: ftyp_size + moov_size + ENVELOPE_HEADER_SIZE + total_payload,
        mdat_payload_offset,
        tracks: track_info,
    })
}

/// Build the Moof data structure for a single sample.
fn build_frame_moof(
    sequence_number: u32,
    track_id: u32,
    base_decode_time: u64,
    sample: &Sample,
    data_offset: i32,
) -> Moof {
    let sample_flags: u32 = if sample.is_sync {
        0x02000000
    } else {
        0x01010000
    };
    let has_cts = sample.cts_offset != 0;

    let entry = TrunEntry {
        duration: Some(sample.duration),
        size: Some(sample.size),
        flags: Some(sample_flags),
        cts: if has_cts { Some(sample.cts_offset) } else { None },
    };

    Moof {
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
                data_offset: Some(data_offset),
                entries: vec![entry],
            }],
            ..Default::default()
        }],
    }
}

/// Measure the encoded size of the moof for a single sample.
fn measure_frame_moof(
    sequence_number: u32,
    track_id: u32,
    base_decode_time: u64,
    sample: &Sample,
) -> Result<u32> {
    // data_offset is a fixed-size i32, so its value doesn't affect size.
    let moof = build_frame_moof(sequence_number, track_id, base_decode_time, sample, 0);
    let mut buf = Vec::new();
    moof.encode(&mut buf).map_err(mp4_err)?;
    Ok(buf.len() as u32)
}

/// Write a single moof+mdat pair to `output`. `expected_moof_size` must match
/// the pass-1 measurement — the pass-1 value is what was baked into `co64`.
fn write_frame_pair<W: Write>(
    output: &mut W,
    sequence_number: u32,
    track_id: u32,
    base_decode_time: u64,
    sample: &Sample,
    sample_data: &[u8],
    expected_moof_size: u32,
) -> Result<()> {
    let data_offset = (expected_moof_size + 8) as i32;
    let moof = build_frame_moof(
        sequence_number,
        track_id,
        base_decode_time,
        sample,
        data_offset,
    );
    let mut moof_buf = Vec::new();
    moof.encode(&mut moof_buf).map_err(mp4_err)?;
    if moof_buf.len() as u32 != expected_moof_size {
        return Err(Error::InvalidMp4(format!(
            "moof size drift: measured {}, wrote {}",
            expected_moof_size,
            moof_buf.len()
        )));
    }
    output.write_all(&moof_buf)?;

    let mdat_total_size = 8u32 + sample_data.len() as u32;
    output.write_all(&mdat_total_size.to_be_bytes())?;
    output.write_all(b"mdat")?;
    output.write_all(sample_data)?;
    Ok(())
}

/// Build a base trak (tkhd + mdia + empty stbl) for a track_id from the catalog.
fn build_base_trak(catalog: &Catalog, track_id: u32) -> Result<(Trak, u32)> {
    if let Some(v) = catalog.video_configs().find(|v| v.track_id() == track_id) {
        return Ok((build_video_trak(v)?, v.timescale()));
    }
    if let Some(a) = catalog.audio_configs().find(|a| a.track_id() == track_id) {
        return Ok((build_audio_trak(a)?, a.timescale()));
    }
    Err(Error::InvalidMp4(format!(
        "no track config for track_id {}",
        track_id
    )))
}

/// Populate an empty stbl with canonical flat-MP4 sample tables.
///
/// `sample_offsets` has one entry per sample (absolute file offsets). Length
/// must match `plan.samples.len()`. Using placeholder zeros during sizing is
/// fine: moov size depends on entry count, not entry values.
fn populate_stbl(stbl: &mut Stbl, plan: &TrackPlan, sample_offsets: &[u64]) {
    let n = plan.samples.len() as u32;
    assert_eq!(sample_offsets.len(), plan.samples.len());

    stbl.stts = Stts {
        entries: rle_stts(plan.samples.iter().map(|s| s.duration)),
    };

    let any_cts = plan.samples.iter().any(|s| s.cts_offset != 0);
    stbl.ctts = if any_cts {
        Some(Ctts {
            entries: rle_ctts(plan.samples.iter().map(|s| s.cts_offset)),
        })
    } else {
        None
    };

    stbl.stsz = if n > 0 {
        let first = plan.samples[0].size;
        if plan.samples.iter().all(|s| s.size == first) {
            Stsz {
                samples: StszSamples::Identical {
                    count: n,
                    size: first,
                },
            }
        } else {
            Stsz {
                samples: StszSamples::Different {
                    sizes: plan.samples.iter().map(|s| s.size).collect(),
                },
            }
        }
    } else {
        Stsz {
            samples: StszSamples::Different { sizes: vec![] },
        }
    };

    // Each sample sits inside its own inner mdat → one sample per chunk.
    stbl.stsc = Stsc {
        entries: if n > 0 {
            vec![StscEntry {
                first_chunk: 1,
                samples_per_chunk: 1,
                sample_description_index: 1,
            }]
        } else {
            vec![]
        },
    };

    stbl.co64 = Some(Co64 {
        entries: sample_offsets.to_vec(),
    });
    stbl.stco = None;

    stbl.stss = if plan.is_video {
        let syncs: Vec<u32> = plan
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_sync)
            .map(|(i, _)| (i as u32) + 1)
            .collect();
        if !syncs.is_empty() && (syncs.len() as u32) < n {
            Some(Stss { entries: syncs })
        } else {
            None
        }
    } else {
        None
    };
}

fn rle_stts(durations: impl IntoIterator<Item = u32>) -> Vec<SttsEntry> {
    let mut out: Vec<SttsEntry> = Vec::new();
    for d in durations {
        match out.last_mut() {
            Some(entry) if entry.sample_delta == d => entry.sample_count += 1,
            _ => out.push(SttsEntry {
                sample_count: 1,
                sample_delta: d,
            }),
        }
    }
    out
}

fn rle_ctts(offsets: impl IntoIterator<Item = i32>) -> Vec<CttsEntry> {
    let mut out: Vec<CttsEntry> = Vec::new();
    for o in offsets {
        match out.last_mut() {
            Some(entry) if entry.sample_offset == o => entry.sample_count += 1,
            _ => out.push(CttsEntry {
                sample_count: 1,
                sample_offset: o,
            }),
        }
    }
    out
}

fn rescale_to_movie(media_duration: u64, media_timescale: u32) -> u64 {
    if media_timescale == 0 {
        return 0;
    }
    let ts = media_timescale as u64;
    let movie = MOVIE_TIMESCALE as u64;
    media_duration
        .saturating_mul(movie)
        .saturating_add(ts / 2)
        / ts
}

/// Build a canonical `edts/elst` for a flat MP4 track.
///
/// Returns `None` when `start_offset_track_ts == 0` — the common case where
/// no presentation offset is needed. Otherwise, emits the canonical two-entry
/// form: an empty edit for the offset, then a normal entry covering the
/// media duration.
///
/// `segment_duration` values are in the movie timescale (per ISOBMFF),
/// `media_time` in the track's media timescale.
///
/// Empty-edit `media_time` is encoded as `u64::MAX` (the v1 signed `-1` bit
/// pattern); mp4-atom always writes v1.
fn build_canonical_elst(
    start_offset_track_ts: u64,
    media_duration_track_ts: u64,
    media_timescale: u32,
) -> Option<Edts> {
    if start_offset_track_ts == 0 {
        return None;
    }
    let empty_movie = rescale_to_movie(start_offset_track_ts, media_timescale);
    let normal_movie = rescale_to_movie(media_duration_track_ts, media_timescale);
    Some(Edts {
        elst: Some(Elst {
            entries: vec![
                ElstEntry {
                    segment_duration: empty_movie,
                    media_time: u64::MAX,
                    media_rate: 1,
                    media_rate_fraction: 0,
                },
                ElstEntry {
                    segment_duration: normal_movie,
                    media_time: 0,
                    media_rate: 1,
                    media_rate_fraction: 0,
                },
            ],
        }),
    })
}

fn mp4_err(e: mp4_atom::Error) -> Error {
    Error::InvalidMp4(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::FileReadAt;
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

    fn convert(name: &str) -> Vec<u8> {
        let input = FileReadAt::open(&fixture_path(name)).unwrap();
        let mut out = Vec::new();
        flat_mp4_to_flat(&input, &mut out).unwrap();
        out
    }

    #[test]
    fn flat_has_muxl_brand() {
        let out = convert("h264-aac.mp4");
        assert_eq!(&out[4..8], b"ftyp");
        assert_eq!(&out[8..12], b"muxl");
    }

    #[test]
    fn flat_top_level_layout_is_ftyp_moov_mdat() {
        let out = convert("h264-aac.mp4");
        let mut pos = 0usize;
        let mut types: Vec<[u8; 4]> = Vec::new();
        while pos + 8 <= out.len() {
            let size =
                u32::from_be_bytes([out[pos], out[pos + 1], out[pos + 2], out[pos + 3]]) as usize;
            let kind = [out[pos + 4], out[pos + 5], out[pos + 6], out[pos + 7]];
            types.push(kind);
            if size == 1 {
                let largesize = u64::from_be_bytes([
                    out[pos + 8], out[pos + 9], out[pos + 10], out[pos + 11],
                    out[pos + 12], out[pos + 13], out[pos + 14], out[pos + 15],
                ]) as usize;
                pos += largesize;
            } else if size == 0 {
                break;
            } else {
                pos += size;
            }
        }
        assert_eq!(&types[0], b"ftyp");
        assert_eq!(&types[1], b"moov");
        assert_eq!(&types[2], b"mdat");
        assert_eq!(types.len(), 3, "unexpected top-level boxes: {:?}", types);
    }

    #[test]
    fn flat_is_parseable_as_mp4() {
        let out = convert("h264-aac.mp4");
        let catalog = crate::init::catalog_from_mp4(Cursor::new(&out)).unwrap();
        assert_eq!(catalog.video_configs().count(), 1);
        assert_eq!(catalog.audio_configs().count(), 1);
    }

    #[test]
    fn flat_inner_fragments_are_self_contained_cmaf() {
        // Walk the moov to find co64 offsets, then check each sample's preceding
        // bytes form a valid moof+mdat pair that parses back to the same sample data.
        use mp4_atom::{Atom, Decode, Header, ReadFrom};

        let name = "h264-aac.mp4";
        let out = convert(name);
        let orig = std::fs::read(fixture_path(name)).unwrap();

        // Extract the plan to know sample sizes / input offsets for comparison.
        let input = FileReadAt::open(&fixture_path(name)).unwrap();
        let (_catalog, plans) = plan_from_flat_mp4(&input).unwrap();

        // Re-read the hybrid's moov to get per-sample file offsets.
        let mut cur = Cursor::new(&out[..]);
        let moov = crate::init::read_moov(&mut cur).unwrap();

        for plan in &plans {
            let trak = moov
                .trak
                .iter()
                .find(|t| t.tkhd.track_id == plan.track_id)
                .unwrap();
            let co64 = trak.mdia.minf.stbl.co64.as_ref().unwrap();
            assert_eq!(co64.entries.len(), plan.samples.len());

            for (i, &sample_abs_offset) in co64.entries.iter().enumerate() {
                let sample = &plan.samples[i];
                // Sample bytes at abs_offset must match the input's sample bytes.
                let sample_in_out =
                    &out[sample_abs_offset as usize..sample_abs_offset as usize + sample.size as usize];
                let sample_in_input = &orig
                    [sample.input_offset as usize..sample.input_offset as usize + sample.size as usize];
                assert_eq!(sample_in_out, sample_in_input, "sample {} data differs", i);

                // The 8 bytes immediately before sample_abs_offset should be the
                // inner mdat header: 4-byte size + "mdat".
                let mdat_hdr_start = sample_abs_offset as usize - 8;
                assert_eq!(&out[mdat_hdr_start + 4..mdat_hdr_start + 8], b"mdat");
                let inner_mdat_size = u32::from_be_bytes([
                    out[mdat_hdr_start],
                    out[mdat_hdr_start + 1],
                    out[mdat_hdr_start + 2],
                    out[mdat_hdr_start + 3],
                ]) as usize;
                assert_eq!(inner_mdat_size, 8 + sample.size as usize);

                // Walk backwards to find the moof: it starts right after the
                // previous sample's payload, or at the outer mdat payload if
                // this is the first fragment. Instead of walking back, decode
                // the moof: the moof ends at mdat_hdr_start and starts at
                // some earlier offset. We find it by scanning the first few
                // bytes back: moof's first 4 bytes are its big-endian size.
                //
                // Simpler: we know moof is at (mdat_hdr_start - moof_size).
                // Decode the moof header to validate.
                let moof_size_hdr = moof_size_ending_at(&out, mdat_hdr_start);
                let moof_start = mdat_hdr_start - moof_size_hdr;
                let mut moof_cur = Cursor::new(&out[moof_start..mdat_hdr_start]);
                let hdr = Header::read_from(&mut moof_cur).unwrap();
                assert_eq!(hdr.kind, Moof::KIND, "moof expected at offset {}", moof_start);
                // Decode the moof fully
                let mut body = Cursor::new(&out[moof_start..mdat_hdr_start]);
                let _moof = Moof::decode(&mut body).unwrap();
            }
        }
    }

    // Scan backwards from `end` to find the start of a moof box. We know the
    // last 4 bytes before end are NOT the moof's size field (they're inside
    // the moof). Instead we try candidate sizes by reading the size field at
    // candidate positions until we find one whose size leads exactly to end.
    fn moof_size_ending_at(data: &[u8], end: usize) -> usize {
        // A moof is at most a few hundred bytes for single-sample fragments.
        // Search upward from end-16 to end-1024.
        for size in 8..1024 {
            if end < size {
                break;
            }
            let start = end - size;
            if start + 4 > data.len() {
                continue;
            }
            let sz = u32::from_be_bytes([data[start], data[start + 1], data[start + 2], data[start + 3]])
                as usize;
            if sz == size && &data[start + 4..start + 8] == b"moof" {
                return size;
            }
        }
        panic!("could not locate moof ending at {}", end);
    }

    #[test]
    fn flat_idempotent_h264_aac() {
        let a = convert("h264-aac.mp4");
        let input: &[u8] = &a;
        let mut b = Vec::new();
        flat_mp4_to_flat(input, &mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn flat_idempotent_h264_opus() {
        let a = convert("h264-opus.mp4");
        let input: &[u8] = &a;
        let mut b = Vec::new();
        flat_mp4_to_flat(input, &mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn flat_idempotent_audio_only() {
        let a = convert("opus-audio-only.mp4");
        let input: &[u8] = &a;
        let mut b = Vec::new();
        flat_mp4_to_flat(input, &mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn flat_stts_rle_merges_equal_durations() {
        let entries = rle_stts(vec![100, 100, 100, 200, 200, 100].into_iter());
        assert_eq!(
            entries,
            vec![
                SttsEntry { sample_count: 3, sample_delta: 100 },
                SttsEntry { sample_count: 2, sample_delta: 200 },
                SttsEntry { sample_count: 1, sample_delta: 100 },
            ]
        );
    }

    #[test]
    fn hybrid_from_fmp4_matches_hybrid_from_flat() {
        // flat MP4 → hybrid A. flat MP4 → fMP4 → hybrid B. A should equal B
        // because both paths produce the same MUXL hybrid for the same samples.
        let name = "h264-aac.mp4";
        let flat_input = FileReadAt::open(&fixture_path(name)).unwrap();

        // Path A: flat → hybrid
        let source_a = read(&flat_input).unwrap();
        let mut hybrid_a = Vec::new();
        write(&source_a, &flat_input, &mut hybrid_a).unwrap();

        // Path B: flat → fMP4 → hybrid
        let mut fmp4 = Vec::new();
        crate::fmp4::write(&source_a, &flat_input, &mut fmp4).unwrap();
        let fmp4_ra: &[u8] = &fmp4;
        let source_b = read(&fmp4_ra.to_vec()).unwrap();
        let mut hybrid_b = Vec::new();
        write(&source_b, &fmp4_ra.to_vec(), &mut hybrid_b).unwrap();

        assert_eq!(
            hybrid_a, hybrid_b,
            "hybrid produced via flat path and via fMP4 path should be identical"
        );
    }

    #[test]
    fn to_flat_autodetects_input_type() {
        let name = "h264-aac.mp4";
        let flat_input = FileReadAt::open(&fixture_path(name)).unwrap();

        // Baseline: flat → hybrid
        let source = read(&flat_input).unwrap();
        let mut expected = Vec::new();
        write(&source, &flat_input, &mut expected).unwrap();

        // Via auto-detect (read) on a flat input
        let mut via_autodetect_flat = Vec::new();
        write(&read(&flat_input).unwrap(), &flat_input, &mut via_autodetect_flat).unwrap();
        assert_eq!(expected, via_autodetect_flat);

        // Via auto-detect on an fMP4 input
        let mut fmp4 = Vec::new();
        crate::fmp4::write(&source, &flat_input, &mut fmp4).unwrap();
        let fmp4_ra: Vec<u8> = fmp4;
        let mut via_autodetect_fmp4 = Vec::new();
        write(&read(&fmp4_ra).unwrap(), &fmp4_ra, &mut via_autodetect_fmp4).unwrap();
        assert_eq!(expected, via_autodetect_fmp4);
    }

    #[test]
    fn hybrid_bakes_leading_empty_edit_into_tfdt_and_elst() {
        // LosslessCut scenario: one track has a 9ms leading empty edit.
        // After normalization, the canonical flat MP4 should have:
        //   - A synthesized 2-entry elst in that track's moov trak
        //   - The inner first moof for that track carrying tfdt=138 (9ms
        //     at 15360 video timescale)
        // Both views agree on the presentation offset; no per-sample bytes
        // change.
        use mp4_atom::FourCC;

        let flat_input = FileReadAt::open(&fixture_path("h264-aac.mp4")).unwrap();
        let (catalog, mut plans) = plan_from_flat_mp4(&flat_input).unwrap();
        // Pick the video plan (matches the catalog video track_id).
        let video_track_id = catalog.video_configs().next().unwrap().track_id();
        let video_ts = catalog.video_configs().next().unwrap().timescale();
        for plan in &mut plans {
            if plan.track_id == video_track_id {
                // 9 ms in movie timescale (1000) → 138 ticks in video timescale (15360).
                plan.start_offset_ticks = 138;
            }
        }

        let mut out = Vec::new();
        write_flat_mp4(&catalog, &plans, &flat_input, &mut out).unwrap();

        // 1. The output moov contains a canonical 2-entry elst on the video trak.
        let moov = read_moov(&mut Cursor::new(&out)).unwrap();
        let video_trak = moov
            .trak
            .iter()
            .find(|t| t.tkhd.track_id == video_track_id)
            .expect("video trak missing");
        let elst = video_trak
            .edts
            .as_ref()
            .and_then(|e| e.elst.as_ref())
            .expect("video trak should have synthesized elst");
        assert_eq!(elst.entries.len(), 2);
        // Leading empty edit: segment_duration in movie timescale (≈9ms),
        // media_time is v1 -1 (u64::MAX).
        assert_eq!(elst.entries[0].segment_duration, 9);
        assert_eq!(elst.entries[0].media_time, u64::MAX);
        assert_eq!(elst.entries[1].media_time, 0);
        // Audio trak (no offset) gets no elst.
        let audio_trak = moov
            .trak
            .iter()
            .find(|t| t.tkhd.track_id != video_track_id)
            .expect("audio trak missing");
        assert!(
            audio_trak.edts.is_none(),
            "audio trak should not get a synthesized elst"
        );

        // 2. The first video inner moof carries tfdt = start_offset_ticks.
        let first_tfdt = find_first_tfdt_for_track(&out, video_track_id).unwrap();
        assert_eq!(first_tfdt, 138);

        // 3. And for good measure, the ftyp and overall file still parse.
        assert!(out.windows(4).any(|w| w == FourCC::new(b"ftyp").as_ref()));
        let _ = video_ts;
    }

    /// Scan a canonical MUXL flat MP4 for the first inner `moof` whose `traf`
    /// matches `track_id`, and return the `tfdt.base_media_decode_time`.
    fn find_first_tfdt_for_track(bytes: &[u8], track_id: u32) -> Option<u64> {
        use mp4_atom::{Atom, FourCC, Header, Moof, ReadAtom, ReadFrom};
        use std::io::{Cursor, Seek, SeekFrom};

        let mut cursor = Cursor::new(bytes);
        let len = bytes.len() as u64;
        let mut pos: u64 = 0;
        while pos < len {
            cursor.seek(SeekFrom::Start(pos)).ok()?;
            let header = <Option<Header> as ReadFrom>::read_from(&mut cursor).ok()??;
            let body = header.size? as u64;
            let after_header = cursor.seek(SeekFrom::Current(0)).ok()?;
            let total = (after_header - pos) + body;
            if header.kind == Moof::KIND {
                let moof = Moof::read_atom(&header, &mut cursor).ok()?;
                for traf in &moof.traf {
                    if traf.tfhd.track_id == track_id {
                        return traf.tfdt.as_ref().map(|t| t.base_media_decode_time);
                    }
                }
            } else if header.kind == FourCC::new(b"mdat") {
                // Descend into the outer envelope so we see the inner moofs.
                let _ = body;
            }
            // For both top-level mdat (envelope) and other boxes, stepping
            // past the header is enough — the outer mdat's payload IS the
            // inner moof+mdat sequence we want to scan.
            if header.kind == FourCC::new(b"mdat") {
                pos = after_header; // step into envelope payload
            } else {
                pos = pos + total;
            }
        }
        None
    }

    #[test]
    fn flat_ctts_rle_merges_equal_offsets() {
        let entries = rle_ctts(vec![0, 0, 100, 100, -50].into_iter());
        assert_eq!(
            entries,
            vec![
                CttsEntry { sample_count: 2, sample_offset: 0 },
                CttsEntry { sample_count: 2, sample_offset: 100 },
                CttsEntry { sample_count: 1, sample_offset: -50 },
            ]
        );
    }
}
