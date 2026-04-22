use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

/// Output encoding for `muxl catalog --format`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum CatalogFormat {
    /// Canonical deterministic CBOR (MUXL's content-addressed wire form).
    /// Written to stdout as raw bytes.
    Drisl,
    /// Hang-shaped JSON (pretty-printed, camelCase, hex description).
    Json,
}

#[derive(Parser)]
#[command(name = "muxl", about = "Deterministic MP4 canonicalization tool", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract catalog (track config) from an MP4.
    Catalog {
        /// Input MP4 file.
        input: PathBuf,
        /// Machine-readable output format. Omit for a human-readable summary.
        #[arg(long, value_enum)]
        format: Option<CatalogFormat>,
    },
    /// Write a canonical MUXL fMP4 (or just its init segment with --init-only).
    Fmp4 {
        /// Input MP4 file (flat or fragmented).
        input: PathBuf,
        /// Output fMP4 path.
        output: PathBuf,
        /// Write only the canonical ftyp+moov init segment (no fragments).
        /// The input's fragment data is not touched.
        #[arg(long)]
        init_only: bool,
    },
    /// Write a canonical MUXL flat MP4 (faststart) from an input MP4.
    Mp4 {
        /// Input MP4 file (flat or fragmented).
        input: PathBuf,
        /// Output flat MP4 path.
        output: PathBuf,
    },
    /// Segment an fMP4 into per-GoP MUXL segments.
    Segment(SegmentArgs),
    /// Concatenate MUXL fMP4 files from stdin, emit CBOR events to stdout.
    Concat,
    /// Generate HLS playback artifacts (CID-addressed blobs + optional playlists).
    Hls(HlsArgs),
}

#[derive(Args)]
#[command(group(ArgGroup::new("mode").required(true).args(["dir", "fmp4", "stdout"])))]
struct SegmentArgs {
    /// Input fMP4 file, or "-" for stdin.
    input: String,
    /// Write segments into this directory (one file per segment).
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// Emit a single MUXL fMP4 file covering the whole input.
    #[arg(long, value_name = "FILE")]
    fmp4: Option<PathBuf>,
    /// Stream segments to stdout as framed CBOR events.
    #[arg(long)]
    stdout: bool,
}

#[derive(Args)]
struct HlsArgs {
    /// Input MP4 file (flat or fragmented).
    input: PathBuf,
    /// Output directory for content-addressed blobs.
    output_dir: PathBuf,
    /// Alternate rendition from another MP4 file (repeatable).
    #[arg(long = "sidecar", value_name = "FILE")]
    sidecars: Vec<PathBuf>,
    /// Also generate static HLS playlists (master.m3u8, per-track media playlists).
    #[arg(long)]
    playlists: bool,
}

pub fn cli_main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Catalog { input, format } => cmd_catalog(&input, format),
        Command::Fmp4 {
            input,
            output,
            init_only,
        } => cmd_fmp4(&input, &output, init_only),
        Command::Mp4 { input, output } => cmd_mp4(&input, &output),
        Command::Segment(args) => cmd_segment(args),
        Command::Concat => cmd_concat(),
        Command::Hls(args) => cmd_hls(args),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn cmd_catalog(input: &Path, format: Option<CatalogFormat>) -> crate::Result<()> {
    // Open with FileReadAt so arbitrarily-long inputs don't load into memory —
    // catalog extraction reads only the moov box.
    let input = crate::io::FileReadAt::open(input)?;
    let catalog = crate::catalog::from_input(&input)?;

    match format {
        Some(CatalogFormat::Drisl) => {
            let bytes = crate::catalog::to_drisl(&catalog)?;
            io::stdout().lock().write_all(&bytes)?;
            return Ok(());
        }
        Some(CatalogFormat::Json) => {
            let json = crate::catalog::to_hang_json(&catalog)?;
            println!("{json}");
            return Ok(());
        }
        None => {}
    }

    if let Some(video) = &catalog.video {
        for (name, v) in &video.renditions {
            eprintln!(
                "video \"{name}\": {} {}x{} (track {}, {} desc bytes)",
                v.codec,
                v.coded_width,
                v.coded_height,
                v.track_id(),
                v.description.len()
            );
        }
    }
    if let Some(audio) = &catalog.audio {
        for (name, a) in &audio.renditions {
            eprintln!(
                "audio \"{name}\": {} {}Hz {}ch (track {}, {} desc bytes)",
                a.codec,
                a.sample_rate,
                a.number_of_channels,
                a.track_id(),
                a.description.len()
            );
        }
    }

    Ok(())
}

fn cmd_fmp4(input: &Path, output: &Path, init_only: bool) -> crate::Result<()> {
    let input = crate::io::FileReadAt::open(input)?;
    let out_file = fs::File::create(output)?;
    let mut out = BufWriter::new(out_file);

    if init_only {
        // Cheap path — only needs the moov, not a full sample plan.
        let catalog = crate::catalog::from_input(&input)?;
        let init = crate::fmp4::init_segment(&catalog)?;
        out.write_all(&init)?;
        out.flush()?;
        eprintln!("init segment: {} bytes", init.len());
        return Ok(());
    }

    let source = crate::read(&input)?;
    crate::fmp4::write(&source, &input, &mut out)?;
    out.flush()?;
    Ok(())
}

fn cmd_mp4(input: &Path, output: &Path) -> crate::Result<()> {
    let input = crate::io::FileReadAt::open(input)?;
    let out_file = fs::File::create(output)?;
    let mut out = BufWriter::new(out_file);
    let source = crate::read(&input)?;
    let info = crate::flat::write(&source, &input, &mut out)?;
    out.flush()?;
    eprintln!(
        "flat MP4: {} bytes (mdat payload @ {}, {} tracks)",
        info.total_bytes,
        info.mdat_payload_offset,
        info.tracks.len(),
    );
    Ok(())
}

fn cmd_segment(args: SegmentArgs) -> crate::Result<()> {
    let mut input: Box<dyn Read> = if args.input == "-" {
        Box::new(io::stdin().lock())
    } else {
        Box::new(fs::File::open(&args.input)?)
    };

    if let Some(dir) = args.dir {
        cmd_segment_dir(&mut input, &dir)
    } else if let Some(file) = args.fmp4 {
        cmd_segment_fmp4(&mut input, &file)
    } else if args.stdout {
        cmd_segment_stdout(&mut input)
    } else {
        // clap's ArgGroup guarantees one mode is set; unreachable in practice.
        unreachable!("segment requires --dir, --fmp4, or --stdout")
    }
}

fn cmd_segment_dir(input: &mut impl Read, output_dir: &Path) -> crate::Result<()> {
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
    let init = crate::fmp4::init_segment(&catalog)?;
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

/// Concatenate MUXL fMP4 files from stdin, emit CBOR events to stdout.
///
/// Reads concatenated MUXL fMP4s from stdin. Emits init events only
/// when the catalog changes between fMP4 files. UUID atoms delimit segments
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

fn cmd_hls(args: HlsArgs) -> crate::Result<()> {
    let HlsArgs {
        input,
        output_dir,
        sidecars,
        playlists,
    } = args;
    let opts = crate::hls::HlsOpts {
        sidecars,
        write_playlists: playlists,
    };
    crate::hls::emit(&input, &output_dir, &opts)?;
    Ok(())
}


fn cmd_segment_fmp4(input: &mut impl Read, output_path: &Path) -> crate::Result<()> {
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

    // Build per-track fMP4: init + [all track1 segments] + [all track2 segments]
    let init = crate::fmp4::init_segment(&catalog)?;
    let mut fmp4 = init;
    for &tid in &track_ids {
        for gop in &gops {
            if let Some(data) = gop.tracks.get(&tid) {
                fmp4.extend_from_slice(data);
            }
        }
    }

    fs::write(output_path, &fmp4)?;
    eprintln!(
        "fMP4: {} bytes ({} GOPs, {} tracks)",
        fmp4.len(),
        gops.len(),
        track_ids.len()
    );

    Ok(())
}
