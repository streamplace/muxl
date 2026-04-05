use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process;

fn usage() -> ! {
    eprintln!("Usage: muxl <command> [args...]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  catalog <input.mp4>                       Extract catalog from MP4");
    eprintln!("  init <input.mp4> <output_init.mp4>        Build canonical init segment");
    eprintln!("  segment <input> --dir <output_dir>        Segment fMP4 into directory");
    eprintln!("  segment <input> --archive <output.mp4>    Segment fMP4 into archive file");
    eprintln!("  segment <input> --stdout                  Stream segments to stdout (framed)");
    eprintln!("  concat                                    Concatenate MUXL archives from stdin (CBOR out)");
    eprintln!("  hls <archive.mp4> <output_dir> [--blobs <dir>]  Generate HLS playlists from archive");
    eprintln!();
    eprintln!("  <input> can be a file path or \"-\" for stdin");
    process::exit(1);
}

pub fn cli_main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }

    let result = match args[1].as_str() {
        "catalog" => cmd_catalog(&args[2..]),
        "init" => cmd_init(&args[2..]),
        "segment" => cmd_segment(&args[2..]),
        "concat" => cmd_concat(),
        "hls" => cmd_hls(&args[2..]),
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            usage();
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn cmd_catalog(args: &[String]) -> crate::Result<()> {
    if args.len() != 1 {
        eprintln!("Usage: muxl catalog <input.mp4>");
        process::exit(1);
    }
    let data = fs::read(&args[0])?;
    let catalog = crate::catalog_from_mp4(Cursor::new(data))?;

    for (name, v) in &catalog.video {
        eprintln!(
            "video \"{name}\": {} {}x{} (track {}, {} desc bytes)",
            v.codec,
            v.coded_width,
            v.coded_height,
            v.track_id,
            v.description.len()
        );
    }
    for (name, a) in &catalog.audio {
        eprintln!(
            "audio \"{name}\": {} {}Hz {}ch (track {}, {} desc bytes)",
            a.codec,
            a.sample_rate,
            a.number_of_channels,
            a.track_id,
            a.description.len()
        );
    }

    Ok(())
}

fn cmd_init(args: &[String]) -> crate::Result<()> {
    if args.len() != 2 {
        eprintln!("Usage: muxl init <input.mp4> <output_init.mp4>");
        process::exit(1);
    }
    let data = fs::read(&args[0])?;
    let catalog = crate::catalog_from_mp4(Cursor::new(data))?;
    let init = crate::build_init_segment(&catalog)?;
    fs::write(&args[1], &init)?;
    eprintln!("Wrote {} bytes", init.len());
    Ok(())
}

fn cmd_segment(args: &[String]) -> crate::Result<()> {
    if args.len() < 2 {
        eprintln!("Usage: muxl segment <input> --dir <output_dir>");
        eprintln!("       muxl segment <input> --archive <output.mp4>");
        eprintln!("       muxl segment <input> --stdout");
        eprintln!("  <input> can be a file path or \"-\" for stdin");
        process::exit(1);
    }

    let input_path = &args[0];
    let mode = &args[1];

    // Open input: file or stdin
    let mut input: Box<dyn Read> = if input_path == "-" {
        Box::new(io::stdin().lock())
    } else {
        Box::new(fs::File::open(input_path)?)
    };

    match mode.as_str() {
        "--dir" => {
            let output_path = args.get(2).unwrap_or_else(|| {
                eprintln!("Missing output directory");
                process::exit(1);
            });
            cmd_segment_dir(&mut input, output_path)
        }
        "--archive" => {
            let output_path = args.get(2).unwrap_or_else(|| {
                eprintln!("Missing output file");
                process::exit(1);
            });
            cmd_segment_archive(&mut input, output_path)
        }
        "--stdout" => cmd_segment_stdout(&mut input),
        _ => {
            eprintln!("Unknown segment mode: {mode}");
            eprintln!("Use --dir, --archive, or --stdout");
            process::exit(1);
        }
    }
}

fn cmd_segment_dir(input: &mut impl Read, output_dir: &str) -> crate::Result<()> {
    let output_dir = std::path::Path::new(output_dir);
    fs::create_dir_all(output_dir)?;

    let catalog = crate::segment_fmp4(input, |gop| {
        for (&track_id, data) in &gop.tracks {
            let track_dir = output_dir.join(format!("track{}", track_id));
            fs::create_dir_all(&track_dir)?;
            let filename = track_dir.join(format!("segment_{:04}.m4s", gop.number));
            fs::write(&filename, data)?;
            eprintln!(
                "track {} segment {:4}: {} bytes",
                track_id, gop.number, data.len()
            );
        }
        Ok(())
    })?;

    // Write init segment
    let init = crate::build_init_segment(&catalog)?;
    let init_path = output_dir.join("init.mp4");
    fs::write(&init_path, &init)?;
    eprintln!("init: {} bytes", init.len());

    Ok(())
}

/// Stream segments to stdout as CBOR (DRISL) events.
///
/// Each event is a separate CBOR value in the stream:
///   {"type": "init", "data": <bstr>}
///   {"type": "segment", "number": <uint>, "data": <bstr>}
///
/// Uses the push-based segmenter so init is emitted first (before segments).
fn cmd_segment_stdout(input: &mut impl Read) -> crate::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 64 * 1024];
    let mut segmenter = crate::Segmenter::new();

    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for event in segmenter.feed(&buf[..n])? {
            write_cbor_event(&mut stdout, &event)?;
        }
    }
    for event in segmenter.flush()? {
        write_cbor_event(&mut stdout, &event)?;
    }
    Ok(())
}

fn write_cbor_event(w: &mut impl io::Write, event: &crate::SegmenterEvent) -> crate::Result<()> {
    let cbor_event = crate::cbor::CborEvent::from_event(event);
    dasl::drisl::to_writer(&mut *w, &cbor_event)
        .map_err(|e| crate::Error::Io(io::Error::new(io::ErrorKind::Other, e.to_string())))?;
    w.flush()?;
    match event {
        crate::SegmenterEvent::InitSegment { data, .. } => {
            eprintln!("init: {} bytes", data.len());
        }
        crate::SegmenterEvent::Segment(gop) => {
            let total: usize = gop.tracks.values().map(|d| d.len()).sum();
            eprintln!(
                "segment {}: {} tracks, {} bytes",
                gop.number,
                gop.tracks.len(),
                total
            );
        }
    }
    Ok(())
}

/// Concatenate MUXL archives from stdin, emit CBOR events to stdout.
///
/// Reads concatenated MUXL fMP4 archives from stdin. Emits init events only
/// when the catalog changes between archives. UUID atoms delimit segments
/// and are passed through in the segment data.
fn cmd_concat() -> crate::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 64 * 1024];
    let mut concat = crate::Concatenator::new();

    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for event in concat.feed(&buf[..n])? {
            write_cbor_event(&mut stdout, &event)?;
        }
    }
    for event in concat.flush()? {
        write_cbor_event(&mut stdout, &event)?;
    }
    Ok(())
}

/// Per-track info extracted from an archive, with byte offsets and blob CID.
/// Metadata for one track in a MUXL archive.
pub struct ArchiveTrack {
    pub track_id: u32,
    pub track_type: String, // "video" or "audio"
    pub codec: String,
    pub timescale: u32,
    pub init_cid: String,
    pub init_data: Vec<u8>,
    pub blob_cid: String,
    pub blob_size: u64,
    pub segments: Vec<ArchiveSegment>,
    // video-specific
    pub width: u32,
    pub height: u32,
    // audio-specific
    pub channels: u32,
    pub sample_rate: u32,
}

/// Byte-range segment metadata within a MUXL archive.
pub struct ArchiveSegment {
    pub offset: u64,
    pub size: u64,
    pub duration_ticks: u64,
    pub sample_count: u32,
}

/// Convert a flat MP4 into a MUXL archive (ftyp+moov + per-track fMP4 segments).
///
/// Fragments the flat MP4 per-track, then feeds frames through the push-based
/// segmenter to produce GOP-aligned MUXL segments. The segmenter needs frames
/// in interleaved order (by presentation time), so we collect all per-track
/// fragments first and merge-sort them.
/// Convert a flat MP4 to a MUXL archive, writing directly to `output`.
///
/// Uses [`ReadAt`] for positional reads — one sample at a time, constant
/// memory overhead. Works with any `ReadAt` backend: native files, WASM
/// SharedArrayBuffer, etc.
///
/// Returns per-track metadata (segments, codecs, init CIDs) collected
/// during the write — no re-read of the output is needed.
///
/// The archive layout is:
///   init + [track1 per-frame moof+mdat ...] + [track2 per-frame moof+mdat ...]
pub fn flat_mp4_to_archive<R: crate::io::ReadAt + ?Sized, W: Write>(
    input: &R,
    output: &mut W,
) -> crate::Result<Vec<ArchiveTrack>> {
    use crate::fragment::{extract_flat_track_info, write_frame_fragment};
    use crate::io::ReadAtCursor;

    let mut cursor = ReadAtCursor::new(input)
        .map_err(|e| crate::Error::Io(e))?;
    let catalog = crate::catalog_from_mp4(&mut cursor)?;
    let init = crate::build_init_segment(&catalog)?;
    let moov = crate::read_moov(&mut cursor)?;
    let track_inits = crate::init::build_track_init_segments(&catalog)?;

    let mut track_ids: Vec<u32> = moov.trak.iter().map(|t| t.tkhd.track_id).collect();
    track_ids.sort();

    // Write init segment
    output.write_all(&init)?;
    let mut write_offset = init.len() as u64;

    let mut sequence_number: u32 = 1;
    let mut tracks: Vec<ArchiveTrack> = Vec::new();

    // Write per-track fragments in track order. For each track, read
    // samples via positional reads, write moof+mdat, and collect metadata.
    for &tid in &track_ids {
        let trak = moov.trak.iter().find(|t| t.tkhd.track_id == tid)
            .ok_or_else(|| crate::Error::InvalidMp4(format!("track {tid} not found")))?;
        let samples = extract_flat_track_info(trak)?;
        let mut decode_time: u64 = 0;
        let mut segments: Vec<ArchiveSegment> = Vec::new();
        let mut cur_seg_offset = write_offset;
        let mut cur_seg_size: u64 = 0;
        let mut cur_seg_dur: u64 = 0;
        let mut cur_seg_samples: u32 = 0;

        let is_video = catalog.video.values().any(|v| v.track_id == tid);
        let ts = if is_video {
            catalog.video.values().find(|v| v.track_id == tid)
                .map(|v| v.timescale).unwrap_or(1)
        } else {
            catalog.audio.values().find(|a| a.track_id == tid)
                .map(|a| a.timescale).unwrap_or(1)
        };
        // For video: flush segment at each keyframe (GOP boundaries)
        // For audio: flush when we've accumulated ~2s of data
        let audio_target_ticks = ts as u64 * 2;

        for sample in &samples {
            // Decide whether to flush the current segment
            let should_flush = if is_video {
                sample.frame.is_sync && cur_seg_size > 0
            } else {
                cur_seg_dur >= audio_target_ticks
            };

            if should_flush {
                segments.push(ArchiveSegment {
                    offset: cur_seg_offset,
                    size: cur_seg_size,
                    duration_ticks: cur_seg_dur,
                    sample_count: cur_seg_samples,
                });
                cur_seg_offset = write_offset;
                cur_seg_size = 0;
                cur_seg_dur = 0;
                cur_seg_samples = 0;
            }

            // Read sample data via positional read
            let mut data = vec![0u8; sample.frame.size as usize];
            input.read_exact_at(sample.file_offset, &mut data)
                .map_err(|e| crate::Error::Io(e))?;

            let bytes_written = write_frame_fragment(
                output,
                sequence_number,
                tid,
                decode_time,
                &sample.frame,
                &data,
            )?;

            cur_seg_size += bytes_written;
            cur_seg_dur += sample.frame.duration as u64;
            cur_seg_samples += 1;
            write_offset += bytes_written;
            sequence_number += 1;
            decode_time += sample.frame.duration as u64;
        }

        // Flush final segment
        if cur_seg_size > 0 {
            segments.push(ArchiveSegment {
                offset: cur_seg_offset,
                size: cur_seg_size,
                duration_ticks: cur_seg_dur,
                sample_count: cur_seg_samples,
            });
        }

        let init_data = track_inits.get(&tid).cloned().unwrap_or_default();
        let init_cid = bdasl_cid(&init_data);

        let (track_type, codec, width, height, channels, sample_rate) =
            if let Some(v) = catalog.video.values().find(|v| v.track_id == tid) {
                ("video", v.codec.clone(), v.coded_width, v.coded_height, 0, 0)
            } else if let Some(a) = catalog.audio.values().find(|a| a.track_id == tid) {
                ("audio", a.codec.clone(), 0, 0, a.number_of_channels, a.sample_rate)
            } else {
                ("unknown", String::new(), 0, 0, 0, 0)
            };

        tracks.push(ArchiveTrack {
            track_id: tid,
            track_type: track_type.to_string(),
            codec,
            timescale: ts,
            init_cid,
            init_data,
            blob_cid: String::new(), // caller fills after hashing
            blob_size: 0,            // caller fills after hashing
            segments,
            width,
            height,
            channels,
            sample_rate,
        });
    }

    let total_gops: usize = tracks.iter()
        .filter(|t| t.track_type == "video")
        .flat_map(|t| &t.segments)
        .count();
    eprintln!("converted flat MP4 → MUXL archive ({total_gops} GOPs, {write_offset} bytes)");
    Ok(tracks)
}

/// Analyze an archive file and return per-track HLS metadata.
/// Accepts both fMP4 archives and flat MP4 files (auto-detected).
fn analyze_archive(path: &str, blobs_dir: Option<&Path>) -> crate::Result<Vec<ArchiveTrack>> {
    // Detect flat vs fMP4 by scanning top-level boxes (no full read needed).
    let is_flat = {
        let mut f = fs::File::open(path)?;
        find_first_moof_offset_seekable(&mut f)?.is_none()
    };

    if is_flat {
        // Flat MP4: convert to archive, streaming through BLAKE3 hasher.
        // flat_mp4_to_archive returns metadata so we never re-read the output.
        eprintln!("detected flat MP4, converting...");
        let tmp = tempfile::NamedTempFile::new()?;
        let mut tracks = {
            let input = crate::io::FileReadAt::open(Path::new(path))?;
            let mut output = BufWriter::new(tmp.as_file());
            let tracks = flat_mp4_to_archive(&input, &mut output)?;
            output.flush()?;
            tracks
        };

        let blob_cid = bdasl_cid_file(tmp.path())?;
        let blob_size = fs::metadata(tmp.path())?.len();

        // Store blob + init segments
        if let Some(bd) = blobs_dir {
            let blob_path = bd.join(format!("{blob_cid}.mp4"));
            if !blob_path.exists() {
                fs::copy(tmp.path(), &blob_path)?;
            }
            for track in &tracks {
                let p = bd.join(format!("{}.mp4", track.init_cid));
                if !p.exists() { fs::write(&p, &track.init_data)?; }
            }
        }

        // Fill in blob CID/size that the converter left blank
        for track in &mut tracks {
            track.blob_cid = blob_cid.clone();
            track.blob_size = blob_size;
        }

        return Ok(tracks);
    }

    // fMP4 archive: read and analyze
    let data = fs::read(path)?;

    let blob_cid = bdasl_cid(&data);
    let blob_size = data.len() as u64;

    // Store blob
    if let Some(bd) = blobs_dir {
        let blob_path = bd.join(format!("{blob_cid}.mp4"));
        if !blob_path.exists() {
            if fs::hard_link(path, &blob_path).is_err() {
                fs::copy(path, &blob_path)?;
            }
        }
    }

    let catalog = crate::catalog_from_mp4(Cursor::new(&data))?;
    let track_inits = crate::init::build_track_init_segments(&catalog)?;
    let init_size = find_first_moof_offset(&data) as u64;
    let is_audio_only = catalog.video.is_empty();

    let mut tracks = Vec::new();

    if is_audio_only {
        // Audio-only archive: parse moof+mdat boxes directly and merge into ~2s segments.
        let fragments = parse_moof_mdat_fragments(&data, init_size);
        let mut track_ids: Vec<u32> = catalog.audio.values().map(|a| a.track_id).collect();
        track_ids.sort();

        for &tid in &track_ids {
            let ts = catalog.audio.values().find(|a| a.track_id == tid)
                .map(|a| a.timescale).unwrap_or(1);
            let target_ticks = ts as u64 * 2; // ~2 seconds

            // Merge fragments into segments
            let mut segments = Vec::new();
            let mut cur_offset = 0u64;
            let mut cur_size = 0u64;
            let mut cur_dur = 0u64;
            let mut cur_samples = 0u32;

            for frag in &fragments {
                if cur_size == 0 {
                    cur_offset = frag.offset;
                }
                cur_size += frag.size;
                cur_dur += frag.duration_ticks;
                cur_samples += frag.sample_count;

                if cur_dur >= target_ticks {
                    segments.push(ArchiveSegment {
                        offset: cur_offset,
                        size: cur_size,
                        duration_ticks: cur_dur,
                        sample_count: cur_samples,
                    });
                    cur_size = 0;
                    cur_dur = 0;
                    cur_samples = 0;
                }
            }
            if cur_size > 0 {
                segments.push(ArchiveSegment {
                    offset: cur_offset,
                    size: cur_size,
                    duration_ticks: cur_dur,
                    sample_count: cur_samples,
                });
            }

            let init_data = track_inits.get(&tid).cloned().unwrap_or_default();
            let init_cid = bdasl_cid(&init_data);
            if let Some(bd) = blobs_dir {
                let p = bd.join(format!("{init_cid}.mp4"));
                if !p.exists() { fs::write(&p, &init_data)?; }
            }

            let a = catalog.audio.values().find(|a| a.track_id == tid);
            tracks.push(ArchiveTrack {
                track_id: tid,
                track_type: "audio".to_string(),
                codec: a.map(|a| a.codec.clone()).unwrap_or_default(),
                timescale: ts,
                init_cid,
                init_data,
                blob_cid: blob_cid.clone(),
                blob_size,
                segments,
                width: 0, height: 0,
                channels: a.map(|a| a.number_of_channels).unwrap_or(0),
                sample_rate: a.map(|a| a.sample_rate).unwrap_or(0),
            });
        }
    } else {
        // Normal archive with video: use GOP-based segmenter
        let mut gops: Vec<crate::GopSegment> = Vec::new();
        crate::segment_fmp4(&mut Cursor::new(&data), |gop| {
            gops.push(gop);
            Ok(())
        })?;

        let mut track_ids: Vec<u32> = gops
            .iter()
            .flat_map(|g| g.tracks.keys().copied())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        track_ids.sort();

        // First pass: compute the byte offset where each track's data begins
        // in the archive (tracks are stored sequentially).
        let mut track_start_offsets: BTreeMap<u32, u64> = BTreeMap::new();
        {
            let mut offset = init_size;
            for &tid in &track_ids {
                track_start_offsets.insert(tid, offset);
                for gop in &gops {
                    if let Some(seg_data) = gop.tracks.get(&tid) {
                        offset += seg_data.len() as u64;
                    }
                }
            }
        }

        let audio_track_ids: std::collections::HashSet<u32> =
            catalog.audio.values().map(|a| a.track_id).collect();

        for &tid in &track_ids {
            let is_audio = audio_track_ids.contains(&tid);
            let track_offset = track_start_offsets[&tid];

            let segments = if is_audio {
                // Audio tracks in a MUXL archive are stored as a contiguous
                // block of moof+mdat pairs after all video data. The GOP
                // segmenter lumps them into one huge segment. Re-parse the
                // raw fragments and merge into ~2s HLS segments.
                let ts = catalog.audio.values().find(|a| a.track_id == tid)
                    .map(|a| a.timescale).unwrap_or(1);
                let target_ticks = ts as u64 * 2;

                let fragments = parse_moof_mdat_fragments(&data, track_offset);
                let mut segments = Vec::new();
                let mut cur_offset = 0u64;
                let mut cur_size = 0u64;
                let mut cur_dur = 0u64;
                let mut cur_samples = 0u32;

                for frag in &fragments {
                    if cur_size == 0 {
                        cur_offset = frag.offset;
                    }
                    cur_size += frag.size;
                    cur_dur += frag.duration_ticks;
                    cur_samples += frag.sample_count;

                    if cur_dur >= target_ticks {
                        segments.push(ArchiveSegment {
                            offset: cur_offset,
                            size: cur_size,
                            duration_ticks: cur_dur,
                            sample_count: cur_samples,
                        });
                        cur_size = 0;
                        cur_dur = 0;
                        cur_samples = 0;
                    }
                }
                if cur_size > 0 {
                    segments.push(ArchiveSegment {
                        offset: cur_offset,
                        size: cur_size,
                        duration_ticks: cur_dur,
                        sample_count: cur_samples,
                    });
                }
                segments
            } else {
                // Video tracks: use GOP-based segments with sequential offsets.
                let mut offset = track_offset;
                let mut segments = Vec::new();
                for gop in &gops {
                    if let Some(seg_data) = gop.tracks.get(&tid) {
                        segments.push(ArchiveSegment {
                            offset,
                            size: seg_data.len() as u64,
                            duration_ticks: gop.durations.get(&tid).copied().unwrap_or(0),
                            sample_count: gop.sample_counts.get(&tid).copied().unwrap_or(0),
                        });
                        offset += seg_data.len() as u64;
                    }
                }
                segments
            };

            let init_data = track_inits.get(&tid).cloned().unwrap_or_default();
            let init_cid = bdasl_cid(&init_data);

            // Store init blob
            if let Some(bd) = blobs_dir {
                let p = bd.join(format!("{init_cid}.mp4"));
                if !p.exists() {
                    fs::write(&p, &init_data)?;
                }
            }

            let (track_type, codec, width, height, channels, sample_rate, timescale) =
                if let Some(v) = catalog.video.values().find(|v| v.track_id == tid) {
                    ("video", v.codec.clone(), v.coded_width, v.coded_height, 0, 0, v.timescale)
                } else if let Some(a) = catalog.audio.values().find(|a| a.track_id == tid) {
                    ("audio", a.codec.clone(), 0, 0, a.number_of_channels, a.sample_rate, a.timescale)
                } else {
                    ("unknown", String::new(), 0, 0, 0, 0, 1)
                };

            tracks.push(ArchiveTrack {
                track_id: tid,
                track_type: track_type.to_string(),
                codec,
                timescale,
                init_cid,
                init_data,
                blob_cid: blob_cid.clone(),
                blob_size,
                segments,
                width,
                height,
                channels,
                sample_rate,
            });
        }
    }

    eprintln!(
        "blob: {blob_cid} ({blob_size} bytes, {} tracks)",
        tracks.len(),
    );

    Ok(tracks)
}

fn cmd_hls(args: &[String]) -> crate::Result<()> {
    if args.is_empty() || args.len() < 2 || args[0] == "--help" || args[0] == "-h" {
        eprintln!("Process an MP4 file into content-addressed blobs for the VOD worker.");
        eprintln!();
        eprintln!("Usage: muxl hls <input.mp4> <output_dir> [options]");
        eprintln!();
        eprintln!("Arguments:");
        eprintln!("  <input.mp4>       Input MP4 file (flat or fragmented)");
        eprintln!("  <output_dir>      Output directory for content-addressed blobs");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --sidecar <file>  Add an alternate rendition from another MP4 file.");
        eprintln!("                    Can be repeated. Each sidecar gets its own blob.");
        eprintln!("                    Example: --sidecar aac-audio.mp4 --sidecar 720p.mp4");
        eprintln!("  --playlists       Also generate static HLS playlists (master.m3u8,");
        eprintln!("                    per-track media playlists, init segments) for serving");
        eprintln!("                    directly from a file server without the VOD worker.");
        eprintln!();
        eprintln!("Output (always):");
        eprintln!("  <output_dir>/CID.mp4     Content-addressed archive + init segment blobs");
        eprintln!("  <output_dir>/CID.json    Playback metadata (tracks, segments, byte offsets)");
        eprintln!();
        eprintln!("Output (with --playlists):");
        eprintln!("  <output_dir>/master.m3u8       HLS master playlist");
        eprintln!("  <output_dir>/video-N.m3u8      Per-track video media playlists");
        eprintln!("  <output_dir>/audio-N.m3u8      Per-track audio media playlists");
        process::exit(if args.first().is_some_and(|a| a == "--help" || a == "-h") { 0 } else { 1 });
    }
    let archive_path = &args[0];
    let output_dir = Path::new(&args[1]);

    // Parse flags
    let mut sidecar_paths: Vec<&str> = Vec::new();
    let mut write_playlists = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--playlists" => {
                write_playlists = true;
                i += 1;
            }
            "--sidecar" => {
                sidecar_paths.push(args.get(i + 1).unwrap_or_else(|| {
                    eprintln!("Missing file after --sidecar");
                    process::exit(1);
                }));
                i += 2;
            }
            _ => { i += 1; }
        }
    }

    fs::create_dir_all(output_dir)?;
    // output_dir is the blobs directory — all CID-addressed files go here
    let blobs_dir = Some(output_dir);

    // Analyze main archive and all sidecars
    let mut all_tracks: Vec<ArchiveTrack> = analyze_archive(archive_path, blobs_dir)?;
    let primary_blob_cid = all_tracks.first().map(|t| t.blob_cid.clone()).unwrap_or_default();
    let primary_blob_size = all_tracks.first().map(|t| t.blob_size).unwrap_or(0);

    for sidecar_path in &sidecar_paths {
        let sidecar_tracks = analyze_archive(sidecar_path, blobs_dir)?;
        all_tracks.extend(sidecar_tracks);
    }

    // Assign unique track keys — use "{blob_cid_prefix}.{track_id}" for sidecars
    // to avoid collisions with primary track IDs.
    // Primary tracks keep simple IDs, sidecar tracks get prefixed.
    struct TrackEntry {
        key: String,      // unique key for playlists/metadata
        track: ArchiveTrack,
    }

    let mut entries: Vec<TrackEntry> = Vec::new();
    for track in all_tracks {
        let key = if track.blob_cid == primary_blob_cid {
            track.track_id.to_string()
        } else {
            // Use first 8 chars of blob CID + track_id to disambiguate
            format!("{}.{}", &track.blob_cid[..track.blob_cid.len().min(16)], track.track_id)
        };
        entries.push(TrackEntry { key, track });
    }

    // Generate static HLS playlists if requested
    if write_playlists {

        // Generate master playlist
        let mut master = String::new();
        master.push_str("#EXTM3U\n#EXT-X-VERSION:6\n\n");

        // Audio renditions — prefer AAC as DEFAULT for Safari compatibility
        let default_audio_key = entries.iter()
            .find(|e| e.track.track_type == "audio" && e.track.codec.starts_with("mp4a"))
            .or_else(|| entries.iter().find(|e| e.track.track_type == "audio"))
            .map(|e| e.key.clone());

        for entry in &entries {
            if entry.track.track_type != "audio" {
                continue;
            }
            let is_default = default_audio_key.as_deref() == Some(&entry.key);
            let default = if is_default { "YES" } else { "NO" };
            master.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{}\",\
                 DEFAULT={default},AUTOSELECT=YES,CHANNELS=\"{}\",URI=\"audio-{}.m3u8\"\n",
                entry.track.codec,
                entry.track.channels,
                entry.key,
            ));
        }
        master.push('\n');

        // Collect audio codec for CODECS string — prefer AAC for Safari compatibility
        let audio_codec = entries
            .iter()
            .find(|e| e.track.track_type == "audio" && e.track.codec.starts_with("mp4a"))
            .or_else(|| entries.iter().find(|e| e.track.track_type == "audio"))
            .map(|e| e.track.codec.as_str())
            .unwrap_or("mp4a.40.2");

        // Video variants
        for entry in &entries {
            if entry.track.track_type != "video" {
                continue;
            }
            let t = &entry.track;
            let total_bytes: u64 = t.segments.iter().map(|s| s.size).sum();
            let total_ticks: u64 = t.segments.iter().map(|s| s.duration_ticks).sum();
            let total_samples: u32 = t.segments.iter().map(|s| s.sample_count).sum();
            let ts = t.timescale as f64;
            let total_dur = total_ticks as f64 / ts;
            let bandwidth = if total_dur > 0.0 { (total_bytes as f64 * 8.0 / total_dur) as u64 } else { 0 };
            let frame_rate = if total_dur > 0.0 { total_samples as f64 / total_dur } else { 0.0 };

            master.push_str(&format!(
                "#EXT-X-STREAM-INF:AUDIO=\"audio\",BANDWIDTH={bandwidth},\
                 CODECS=\"{},{audio_codec}\",RESOLUTION={}x{},FRAME-RATE={frame_rate:.3}\n",
                t.codec, t.width, t.height,
            ));
            master.push_str(&format!("video-{}.m3u8\n", entry.key));
        }

        fs::write(output_dir.join("master.m3u8"), &master)?;

        // Generate per-track media playlists
        for entry in &entries {
            let t = &entry.track;
            let ts = t.timescale as f64;
            let archive_file = format!("{}.mp4", t.blob_cid);

            let max_dur: f64 = t.segments.iter()
                .map(|s| s.duration_ticks as f64 / ts)
                .fold(0.0, f64::max);
            let target_dur = (max_dur.ceil() as u64).max(1);

            let mut playlist = String::new();
            playlist.push_str("#EXTM3U\n");
            playlist.push_str("#EXT-X-VERSION:6\n");
            playlist.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
            playlist.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
            playlist.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur}\n"));
            playlist.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
            playlist.push_str(&format!("#EXT-X-MAP:URI=\"{}.mp4\"\n\n", entry.track.init_cid));

            for seg in &t.segments {
                let dur_sec = seg.duration_ticks as f64 / ts;
                playlist.push_str(&format!("#EXTINF:{dur_sec:.6},\n"));
                playlist.push_str(&format!("#EXT-X-BYTERANGE:{}@{}\n", seg.size, seg.offset));
                playlist.push_str(&archive_file);
                playlist.push('\n');
            }

            playlist.push_str("#EXT-X-ENDLIST\n");
            let prefix = if t.track_type == "video" { "video" } else { "audio" };
            fs::write(
                output_dir.join(format!("{prefix}-{}.m3u8", entry.key)),
                &playlist,
            )?;
        }
    }

    // Write metadata JSON
    let mut meta_tracks = serde_json::Map::new();
    for entry in &entries {
        let t = &entry.track;
        let segments: Vec<serde_json::Value> = t.segments.iter()
            .map(|s| serde_json::json!({
                "offset": s.offset,
                "size": s.size,
                "durationTicks": s.duration_ticks,
                "sampleCount": s.sample_count,
            }))
            .collect();

        let mut info = serde_json::json!({
            "type": t.track_type,
            "codec": t.codec,
            "timescale": t.timescale,
            "initCid": t.init_cid,
            "blobCid": t.blob_cid,
            "blobSize": t.blob_size,
            "segments": segments,
        });
        if t.track_type == "video" {
            info["width"] = serde_json::json!(t.width);
            info["height"] = serde_json::json!(t.height);
        } else {
            info["channels"] = serde_json::json!(t.channels);
            info["sampleRate"] = serde_json::json!(t.sample_rate);
        }
        meta_tracks.insert(entry.key.clone(), info);
    }

    let metadata = serde_json::json!({
        "blobCid": primary_blob_cid,
        "blobSize": primary_blob_size,
        "tracks": meta_tracks,
    });
    let metadata_str = serde_json::to_string_pretty(&metadata).unwrap_or_default();
    // Write blob-keyed metadata — this is what the VOD worker uses
    fs::write(output_dir.join(format!("{primary_blob_cid}.json")), &metadata_str)?;

    let total_tracks = entries.len();
    let total_blobs: std::collections::HashSet<_> = entries.iter().map(|e| &e.track.blob_cid).collect();
    eprintln!(
        "  {} tracks, {} blobs{}",
        total_tracks,
        total_blobs.len(),
        if write_playlists { " + static playlists" } else { "" },
    );
    Ok(())
}

/// Compute a BDASL CID: CIDv1, raw codec (0x55), BLAKE3 (0x1e), base32lower.
fn bdasl_cid(data: &[u8]) -> String {
    let digest = blake3::hash(data);
    bdasl_cid_from_digest(&digest)
}

/// Compute a BDASL CID by streaming a file through BLAKE3 (no full-file buffer).
fn bdasl_cid_file(path: &Path) -> crate::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(bdasl_cid_from_digest(&hasher.finalize()))
}

fn bdasl_cid_from_digest(digest: &blake3::Hash) -> String {
    // CID binary: version(1) + codec(0x55=raw) + hash_fn(0x1e=blake3) + hash_len(0x20) + digest
    let mut cid_bytes = vec![0x01, 0x55, 0x1e, 0x20];
    cid_bytes.extend_from_slice(digest.as_bytes());
    let mut out = String::from("b");
    base32_lower_encode(&cid_bytes, &mut out);
    out
}

fn base32_lower_encode(data: &[u8], out: &mut String) {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut buffer: u64 = 0;
    let mut bits = 0;
    for &byte in data {
        buffer = (buffer << 8) | byte as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1F) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1F) as usize] as char);
    }
}

/// A single moof+mdat fragment with byte offset and duration metadata.
struct MoofFragment {
    offset: u64,
    size: u64,
    duration_ticks: u64,
    sample_count: u32,
}

/// Parse moof+mdat pairs directly from raw bytes, returning byte offsets and durations.
/// Used for audio-only archives where the keyframe-based segmenter can't split.
fn parse_moof_mdat_fragments(data: &[u8], start_offset: u64) -> Vec<MoofFragment> {
    let mut fragments = Vec::new();
    let mut pos = start_offset as usize;

    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let box_type = &data[pos + 4..pos + 8];

        if size == 0 || pos + size > data.len() {
            break;
        }

        if box_type == b"moof" {
            let moof_offset = pos as u64;
            let moof_size = size;

            // Parse trun inside moof → traf → trun for duration
            let (dur, samples) = parse_moof_duration(&data[pos..pos + size]);

            // Expect mdat immediately after
            let mdat_pos = pos + moof_size;
            if mdat_pos + 8 <= data.len() && &data[mdat_pos + 4..mdat_pos + 8] == b"mdat" {
                let mdat_size = u32::from_be_bytes([
                    data[mdat_pos], data[mdat_pos + 1], data[mdat_pos + 2], data[mdat_pos + 3],
                ]) as usize;
                fragments.push(MoofFragment {
                    offset: moof_offset,
                    size: (moof_size + mdat_size) as u64,
                    duration_ticks: dur,
                    sample_count: samples,
                });
                pos = mdat_pos + mdat_size;
                continue;
            }
        }

        pos += size;
    }

    fragments
}

/// Extract total duration and sample count from a moof box.
fn parse_moof_duration(moof: &[u8]) -> (u64, u32) {
    let mut total_dur: u64 = 0;
    let mut total_samples: u32 = 0;

    // Walk moof → traf → trun/tfhd
    let mut pos = 8; // skip moof header
    while pos + 8 <= moof.len() {
        let bsize = u32::from_be_bytes([moof[pos], moof[pos + 1], moof[pos + 2], moof[pos + 3]]) as usize;
        let btype = &moof[pos + 4..pos + 8];
        if bsize == 0 || pos + bsize > moof.len() { break; }

        if btype == b"traf" {
            let traf_end = pos + bsize;
            let mut tpos = pos + 8;
            let mut default_duration: u32 = 0;

            while tpos + 8 <= traf_end {
                let tsize = u32::from_be_bytes([moof[tpos], moof[tpos + 1], moof[tpos + 2], moof[tpos + 3]]) as usize;
                let ttype = &moof[tpos + 4..tpos + 8];
                if tsize == 0 { break; }

                if ttype == b"tfhd" && tpos + 16 <= traf_end {
                    let flags = u32::from_be_bytes([0, moof[tpos + 9], moof[tpos + 10], moof[tpos + 11]]);
                    let mut off = tpos + 16;
                    if flags & 0x01 != 0 { off += 8; } // base_data_offset
                    if flags & 0x02 != 0 { off += 4; } // sample_description_index
                    if flags & 0x08 != 0 && off + 4 <= traf_end {
                        default_duration = u32::from_be_bytes([moof[off], moof[off + 1], moof[off + 2], moof[off + 3]]);
                    }
                } else if ttype == b"trun" && tpos + 16 <= traf_end {
                    let flags = u32::from_be_bytes([0, moof[tpos + 9], moof[tpos + 10], moof[tpos + 11]]);
                    let sample_count = u32::from_be_bytes([moof[tpos + 12], moof[tpos + 13], moof[tpos + 14], moof[tpos + 15]]);
                    total_samples += sample_count;

                    let mut off = tpos + 16;
                    if flags & 0x01 != 0 { off += 4; } // data_offset
                    if flags & 0x04 != 0 { off += 4; } // first_sample_flags

                    for _ in 0..sample_count {
                        let dur = if flags & 0x100 != 0 && off + 4 <= traf_end {
                            let d = u32::from_be_bytes([moof[off], moof[off + 1], moof[off + 2], moof[off + 3]]);
                            off += 4;
                            d
                        } else {
                            default_duration
                        };
                        total_dur += dur as u64;
                        if flags & 0x200 != 0 { off += 4; } // sample_size
                        if flags & 0x400 != 0 { off += 4; } // sample_flags
                        if flags & 0x800 != 0 { off += 4; } // composition_time_offset
                    }
                }

                tpos += tsize;
            }
        }

        pos += bsize;
    }

    (total_dur, total_samples)
}

/// Find the byte offset of the first moof box (= end of init segment).
fn find_first_moof_offset(data: &[u8]) -> usize {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size32 = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let box_type = &data[pos + 4..pos + 8];
        let size = if size32 == 1 && pos + 16 <= data.len() {
            // Extended size (64-bit)
            u64::from_be_bytes([
                data[pos+8], data[pos+9], data[pos+10], data[pos+11],
                data[pos+12], data[pos+13], data[pos+14], data[pos+15],
            ]) as usize
        } else {
            size32 as usize
        };
        if box_type == b"moof" {
            return pos;
        }
        if size == 0 || pos + size > data.len() {
            break;
        }
        pos += size;
    }
    pos
}

/// Seekable version: scan top-level boxes to find the first moof.
/// Returns `Ok(Some(offset))` if found, `Ok(None)` if flat (no moof).
fn find_first_moof_offset_seekable<RS: Read + Seek>(reader: &mut RS) -> io::Result<Option<u64>> {
    let file_size = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;

    let mut hdr = [0u8; 16];
    let mut pos: u64 = 0;

    while pos + 8 <= file_size {
        reader.seek(SeekFrom::Start(pos))?;
        let n = reader.read(&mut hdr)?;
        if n < 8 { break; }

        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let box_type = &hdr[4..8];
        let size: u64 = if size32 == 1 && n >= 16 {
            u64::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15]])
        } else {
            size32 as u64
        };

        if box_type == b"moof" {
            return Ok(Some(pos));
        }
        if size == 0 || pos + size > file_size {
            break;
        }
        pos += size;
    }
    Ok(None)
}

fn cmd_segment_archive(input: &mut impl Read, output_path: &str) -> crate::Result<()> {
    let mut gops = Vec::new();

    let catalog = crate::segment_fmp4(input, |gop| {
        let total: usize = gop.tracks.values().map(|d| d.len()).sum();
        eprintln!(
            "segment {:4}: {} tracks, {} bytes",
            gop.number,
            gop.tracks.len(),
            total
        );
        gops.push(gop);
        Ok(())
    })?;

    // Collect track IDs in order
    let mut track_ids: Vec<u32> = gops
        .iter()
        .flat_map(|g| g.tracks.keys().copied())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    track_ids.sort();

    // Build per-track archive: init + [all track1 segments] + [all track2 segments]
    let init = crate::build_init_segment(&catalog)?;
    let mut archive = init;
    for &tid in &track_ids {
        for gop in &gops {
            if let Some(data) = gop.tracks.get(&tid) {
                archive.extend_from_slice(data);
            }
        }
    }

    fs::write(output_path, &archive)?;
    eprintln!(
        "archive: {} bytes ({} GOPs, {} tracks)",
        archive.len(),
        gops.len(),
        track_ids.len()
    );

    Ok(())
}
