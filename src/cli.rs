//! `muxl` CLI surface.
//!
//! Public so that downstream binaries (notably `muxl-sign`) can include
//! these subcommands in a single consolidated CLI without duplicating
//! arg-parsing code. The shape:
//!
//! - [`Command`] — the top-level subcommand enum.
//! - One named `*Args` struct per subcommand (e.g. [`CatalogArgs`],
//!   [`Fmp4Args`]).
//! - One `cmd_*` handler per subcommand, plus [`dispatch`] which
//!   pattern-matches a [`Command`] to its handler.
//! - [`cli_main`] — the binary entry point: `Cli::parse() ; dispatch(...)`.

use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

/// Output encoding for `muxl catalog --format`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CatalogFormat {
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

/// All `muxl` subcommands. Reused as variants in `muxl-sign`'s consolidated CLI.
#[derive(Subcommand)]
pub enum Command {
    /// Extract catalog (track config) from an MP4.
    Catalog(CatalogArgs),
    /// Write a canonical MUXL fMP4 (or just its init segment with --init-only).
    Fmp4(Fmp4Args),
    /// Write a canonical MUXL flat MP4 (faststart) from an input MP4.
    Mp4(Mp4Args),
    /// Segment an fMP4 into per-GoP MUXL segments.
    Segment(SegmentArgs),
    /// Concatenate MUXL fMP4 files from stdin, emit CBOR events to stdout.
    Concat,
    /// Generate HLS playback artifacts (CID-addressed blobs + optional playlists).
    Hls(HlsArgs),
}

#[derive(Args)]
pub struct CatalogArgs {
    /// Input MP4 file.
    pub input: PathBuf,
    /// Machine-readable output format. Omit for a human-readable summary.
    #[arg(long, value_enum)]
    pub format: Option<CatalogFormat>,
}

#[derive(Args)]
pub struct Fmp4Args {
    /// Input MP4 file (flat or fragmented).
    pub input: PathBuf,
    /// Output fMP4 path.
    pub output: PathBuf,
    /// Write only the canonical ftyp+moov init segment (no fragments).
    /// The input's fragment data is not touched.
    #[arg(long)]
    pub init_only: bool,
}

#[derive(Args)]
pub struct Mp4Args {
    /// Input MP4 file (flat or fragmented).
    pub input: PathBuf,
    /// Output flat MP4 path.
    pub output: PathBuf,
}

#[derive(Args)]
#[command(group(ArgGroup::new("mode").required(true).args(["dir", "fmp4", "stdout"])))]
pub struct SegmentArgs {
    /// Input fMP4 file, or "-" for stdin.
    pub input: String,
    /// Write segments into this directory (one file per segment).
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Emit a single MUXL fMP4 file covering the whole input.
    #[arg(long, value_name = "FILE")]
    pub fmp4: Option<PathBuf>,
    /// Stream segments to stdout as framed CBOR events.
    #[arg(long)]
    pub stdout: bool,
}

#[derive(Args)]
pub struct HlsArgs {
    /// Input MP4 file (flat or fragmented).
    pub input: PathBuf,
    /// Output directory for content-addressed blobs.
    pub output_dir: PathBuf,
    /// Alternate rendition from another MP4 file (repeatable).
    #[arg(long = "sidecar", value_name = "FILE")]
    pub sidecars: Vec<PathBuf>,
    /// Also generate static HLS playlists (master.m3u8, per-track media playlists).
    #[arg(long)]
    pub playlists: bool,
}

/// Run the parsed `muxl` CLI: parse argv into a [`Command`] and [`dispatch`] it.
pub fn cli_main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli.command) {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

/// Run a parsed [`Command`]. Used by both `cli_main` and downstream
/// binaries that flatten muxl's subcommands into a wider CLI.
pub fn dispatch(cmd: Command) -> crate::Result<()> {
    match cmd {
        Command::Catalog(args) => cmd_catalog(args),
        Command::Fmp4(args) => cmd_fmp4(args),
        Command::Mp4(args) => cmd_mp4(args),
        Command::Segment(args) => cmd_segment(args),
        Command::Concat => cmd_concat(),
        Command::Hls(args) => cmd_hls(args),
    }
}

pub fn cmd_catalog(args: CatalogArgs) -> crate::Result<()> {
    let CatalogArgs { input, format } = args;
    // Open with FileReadAt so arbitrarily-long inputs don't load into memory —
    // catalog extraction reads only the moov box.
    let input_reader = crate::io::FileReadAt::open(&input)?;
    let catalog = crate::catalog::from_input(&input_reader)?;

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

    let _ = input;
    Ok(())
}

pub fn cmd_fmp4(args: Fmp4Args) -> crate::Result<()> {
    let Fmp4Args {
        input,
        output,
        init_only,
    } = args;
    let input_reader = crate::io::FileReadAt::open(&input)?;
    let out_file = fs::File::create(&output)?;
    let mut out = BufWriter::new(out_file);

    if init_only {
        // Cheap path — only needs the moov, not a full sample plan.
        let catalog = crate::catalog::from_input(&input_reader)?;
        let init = crate::fmp4::init_segment(&catalog)?;
        out.write_all(&init)?;
        out.flush()?;
        eprintln!("init segment: {} bytes", init.len());
        return Ok(());
    }

    let source = crate::read(&input_reader)?;
    crate::fmp4::write(&source, &input_reader, &mut out)?;
    out.flush()?;
    Ok(())
}

pub fn cmd_mp4(args: Mp4Args) -> crate::Result<()> {
    let Mp4Args { input, output } = args;
    let input_reader = crate::io::FileReadAt::open(&input)?;
    let out_file = fs::File::create(&output)?;
    let mut out = BufWriter::new(out_file);
    let source = crate::read(&input_reader)?;
    let info = crate::flat::write(&source, &input_reader, &mut out)?;
    out.flush()?;
    eprintln!(
        "flat MP4: {} bytes (mdat payload @ {}, {} tracks)",
        info.total_bytes,
        info.mdat_payload_offset,
        info.tracks.len(),
    );
    Ok(())
}

pub fn cmd_segment(args: SegmentArgs) -> crate::Result<()> {
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
pub fn cmd_concat() -> crate::Result<()> {
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

pub fn cmd_hls(args: HlsArgs) -> crate::Result<()> {
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
