//! `muxl` CLI building blocks.
//!
//! Not a CLI of its own: the single `muxl` binary is built by the `muxl` crate
//! (crates/muxl), which composes these pieces with its signing subcommands. This
//! module exports the reusable parts so that consolidated CLI needn't duplicate
//! any arg-parsing code:
//!
//! - One named `*Args` struct per subcommand (e.g. [`CatalogArgs`],
//!   [`Fmp4Args`]).
//! - One public `cmd_*` handler per subcommand.

use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Args, ValueEnum};

/// Output encoding for `muxl catalog --format`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CatalogFormat {
    /// Canonical deterministic CBOR (MUXL's content-addressed wire form).
    /// Written to stdout as raw bytes.
    Drisl,
    /// Hang-shaped JSON (pretty-printed, camelCase, hex description).
    Json,
}

/// Output container for `muxl wrap`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum WrapFormat {
    /// Appendable fMP4: ftyp+moov(init) + verbatim segments. Streamable; the
    /// segment bytes (and any signatures over them) are untouched.
    Fmp4,
    /// Finalized flat MP4 (faststart): synthesized moov over the verbatim
    /// segment stream. Fast-forward + m4s-native; accepts fMP4, flat, or bare
    /// m4s input.
    Flat,
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
    /// Remap a track id in the output, given as `OLD:NEW` (repeatable). The
    /// rewrite applies to both the moov and every minted moof, so the
    /// canonical bytes are minted at the chosen id. Used to give a freshly
    /// canonicalized single-track segment (e.g. a transcoded rendition) a
    /// free id so it can be concatenated alongside the tracks it derives
    /// from without colliding.
    #[arg(long = "remap-track", value_name = "OLD:NEW", value_parser = parse_remap_pair)]
    pub remap_track: Vec<(u32, u32)>,
}

/// Parse a `OLD:NEW` track-id remap pair for `--remap-track`.
fn parse_remap_pair(s: &str) -> std::result::Result<(u32, u32), String> {
    let (old, new) = s
        .split_once(':')
        .ok_or_else(|| format!("expected OLD:NEW, got {s:?}"))?;
    let old = old
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("invalid source track id {:?}: {e}", old.trim()))?;
    let new = new
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("invalid target track id {:?}: {e}", new.trim()))?;
    Ok((old, new))
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
pub struct WrapArgs {
    /// Output MP4 path. "-" writes stdout.
    pub output: PathBuf,
    /// Input MUXL wrappers — fMP4, flat MP4, or bare m4s segment streams.
    /// Given `tar`-style (output first, then inputs): each input is unwrapped
    /// to its canonical segments, and the segments are concatenated in
    /// argument order into one presentation MP4. "-" reads stdin — use it as
    /// the sole input.
    #[arg(required = true, num_args = 1..)]
    pub inputs: Vec<PathBuf>,
    /// Presentation container to synthesize.
    #[arg(long, value_enum, default_value = "fmp4")]
    pub format: WrapFormat,
    /// Emit only the synthesized init segment (ftyp+moov) — the EXT-X-MAP
    /// target for HLS playback — instead of init+segments. Requires
    /// --format fmp4.
    #[arg(long)]
    pub init_only: bool,
}

#[derive(Args)]
pub struct UnwrapArgs {
    /// Input MUXL wrapper: fMP4, flat MP4, or a bare m4s segment stream.
    /// "-" reads stdin.
    pub input: PathBuf,
    /// Re-derive the per-track CBOR event stream (init + one segment per GoP;
    /// bytes verbatim, durations recomputed) to stdout — the inverse of
    /// `segment --stdout` for already-stored segments. Mutually exclusive
    /// with --dir.
    #[arg(long)]
    pub events: bool,
    /// Write recovered segments here — one `.m4s` per segment under
    /// `track<id>/`, plus a per-track `init.mp4`. Omit for a stderr summary.
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
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

#[derive(Args)]
pub struct CidArgs {
    /// Input file. Any bytes for a whole-file CID; a MUXL wrapper with --segments.
    pub input: PathBuf,
    /// Unwrap the input and print the CID of each canonical segment (the
    /// content-addressed unit) instead of one CID for the whole file.
    #[arg(long)]
    pub segments: bool,
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
        remap_track,
    } = args;
    let remap: std::collections::BTreeMap<u32, u32> = remap_track.into_iter().collect();
    let input_reader = crate::io::FileReadAt::open(&input)?;
    let out_file = fs::File::create(&output)?;
    let mut out = BufWriter::new(out_file);

    if init_only {
        // Cheap path — only needs the moov, not a full sample plan.
        let mut catalog = crate::catalog::from_input(&input_reader)?;
        if !remap.is_empty() {
            catalog.remap_track_ids(&remap);
        }
        let init = crate::fmp4::init_segment(&catalog)?;
        out.write_all(&init)?;
        out.flush()?;
        eprintln!("init segment: {} bytes", init.len());
        return Ok(());
    }

    let mut source = crate::read(&input_reader)?;
    if !remap.is_empty() {
        source.remap_track_ids(&remap);
    }
    crate::fmp4::write(&source, &input_reader, &mut out)?;
    out.flush()?;
    Ok(())
}

pub fn cmd_mp4(args: Mp4Args) -> crate::Result<()> {
    let Mp4Args { input, output } = args;

    // "-" reads stdin / writes stdout. Required for the wasm32-wasip1
    // build, where FileReadAt's pread(2) isn't available — slurping the
    // input into a Vec<u8> uses the in-memory ReadAt impl instead, the
    // same workaround sign-per-track uses.
    let stdin_input = input.as_os_str() == "-";
    let stdout_output = output.as_os_str() == "-";

    let info = if stdin_input || stdout_output {
        let input_bytes: Vec<u8> = if stdin_input {
            let mut buf = Vec::new();
            io::stdin().lock().read_to_end(&mut buf)?;
            buf
        } else {
            fs::read(&input)?
        };
        let source = crate::read(&input_bytes)?;
        let mut out: Box<dyn Write> = if stdout_output {
            Box::new(BufWriter::new(io::stdout().lock()))
        } else {
            Box::new(BufWriter::new(fs::File::create(&output)?))
        };
        let info = crate::flat::write(&source, &input_bytes, &mut out)?;
        out.flush()?;
        info
    } else {
        let input_reader = crate::io::FileReadAt::open(&input)?;
        let out_file = fs::File::create(&output)?;
        let mut out = BufWriter::new(out_file);
        let source = crate::read(&input_reader)?;
        let info = crate::flat::write(&source, &input_reader, &mut out)?;
        out.flush()?;
        info
    };

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

/// Wrap canonical MUXL segments (recovered from any input wrapper) into a
/// presentation MP4. fMP4 is the verbatim fast-forward path; flat is the
/// interim Source-based flatten.
pub fn cmd_wrap(args: WrapArgs) -> crate::Result<()> {
    let WrapArgs { output, inputs, format, init_only } = args;

    // Read every input wrapper up front. The segments recovered by `unwrap`
    // borrow these buffers, so all of them must outlive the combined segment
    // list. "-" reads stdin, so a wasm host can pipe the hot bytes without
    // touching the filesystem (matching `mp4` and `sign-per-track`).
    let buffers: Vec<Vec<u8>> = inputs
        .iter()
        .map(|input| -> crate::Result<Vec<u8>> {
            if input.as_os_str() == "-" {
                let mut buf = Vec::new();
                io::stdin().lock().read_to_end(&mut buf)?;
                Ok(buf)
            } else {
                Ok(fs::read(input)?)
            }
        })
        .collect::<crate::Result<Vec<_>>>()?;

    let mut out: Box<dyn Write> = if output.as_os_str() == "-" {
        Box::new(BufWriter::new(io::stdout().lock()))
    } else {
        Box::new(BufWriter::new(fs::File::create(&output)?))
    };

    // Unwrap each input and splice its canonical segments together in argument
    // order — like `tar`, the inputs are concatenated in the sequence given.
    let mut segments = Vec::new();
    for buf in &buffers {
        segments.extend(crate::reader::unwrap(buf)?);
    }
    let catalog = crate::reader::aggregate_catalog(&segments);

    if init_only {
        if !matches!(format, WrapFormat::Fmp4) {
            return Err(crate::Error::InvalidMp4(
                "--init-only requires --format fmp4".into(),
            ));
        }
        // Synthesize just the combined ftyp+moov from the segments' embedded
        // catalogs — the per-stream init the HLS read side maps to.
        let init = crate::present::init(&catalog)?;
        out.write_all(&init)?;
        out.flush()?;
        eprintln!(
            "init segment: {} bytes (from {} segments across {} input(s))",
            init.len(),
            segments.len(),
            buffers.len()
        );
        return Ok(());
    }

    match format {
        WrapFormat::Fmp4 => {
            crate::present::write_fmp4(&catalog, segments.iter().map(|s| s.data), &mut out)?;
            out.flush()?;
            eprintln!(
                "fMP4: wrapped {} segments from {} input(s)",
                segments.len(),
                buffers.len()
            );
        }
        WrapFormat::Flat => {
            // Fast-forward + m4s-native: unwrap to verbatim segments, then
            // synthesize the flat moov from their parsed metadata.
            let slices: Vec<&[u8]> = segments.iter().map(|s| s.data).collect();
            crate::present::write_flat_from_m4s(&catalog, &slices, &mut out)?;
            out.flush()?;
            eprintln!(
                "flat MP4: wrapped {} segments from {} input(s)",
                segments.len(),
                buffers.len()
            );
        }
    }
    Ok(())
}

/// Unwrap any MUXL wrapper into its canonical segments via the fast-forward
/// reader. With `--dir`, writes one `.m4s` per segment under `track<id>/`
/// plus a per-track `init.mp4`; otherwise prints a summary.
pub fn cmd_unwrap(args: UnwrapArgs) -> crate::Result<()> {
    use std::collections::{BTreeMap, HashSet};
    let UnwrapArgs { input, dir, events } = args;
    let bytes: Vec<u8> = if input.as_os_str() == "-" {
        let mut buf = Vec::new();
        io::stdin().lock().read_to_end(&mut buf)?;
        buf
    } else {
        fs::read(&input)?
    };

    // --events: re-emit the per-track CBOR event stream (for a host that
    // drives the live-HLS window off already-stored segments).
    if events {
        let mut stdout = io::stdout().lock();
        let evs = crate::reader::segment_events(&bytes)?;
        for ev in &evs {
            dasl::drisl::to_writer(&mut stdout, ev).map_err(|e| {
                crate::Error::Io(io::Error::new(io::ErrorKind::Other, e.to_string()))
            })?;
        }
        stdout.flush()?;
        eprintln!("emitted {} events", evs.len());
        return Ok(());
    }

    let segments = crate::reader::unwrap(&bytes)?;

    if let Some(dir) = dir {
        let mut counters: BTreeMap<u32, u32> = BTreeMap::new();
        let mut inited: HashSet<u32> = HashSet::new();
        for seg in &segments {
            let tid = seg.track_id;
            let track_dir = dir.join(format!("track{tid}"));
            fs::create_dir_all(&track_dir)?;
            if inited.insert(tid) {
                fs::write(track_dir.join("init.mp4"), crate::present::init(&seg.catalog)?)?;
            }
            let n = counters.entry(tid).or_default();
            fs::write(track_dir.join(format!("segment_{n:04}.m4s")), seg.data)?;
            *n += 1;
        }
        eprintln!(
            "unwrapped {} segments across {} tracks",
            segments.len(),
            counters.len()
        );
    } else {
        for (i, seg) in segments.iter().enumerate() {
            eprintln!("segment {i}: track {} ({} bytes)", seg.track_id, seg.data.len());
        }
        eprintln!("{} segments total", segments.len());
    }
    Ok(())
}

/// Print the BDASL CID of a file. With --segments, unwrap the input and print
/// the CID of each canonical segment (track and per-track index alongside).
pub fn cmd_cid(args: CidArgs) -> crate::Result<()> {
    let CidArgs { input, segments } = args;
    if segments {
        use std::collections::BTreeMap;
        let bytes = fs::read(&input)?;
        let segs = crate::reader::unwrap(&bytes)?;
        let mut idx: BTreeMap<u32, u32> = BTreeMap::new();
        for seg in &segs {
            let n = idx.entry(seg.track_id).or_default();
            println!(
                "{}\ttrack{} seg{}\t{} bytes",
                crate::cid::from_bytes(seg.data),
                seg.track_id,
                n,
                seg.data.len()
            );
            *n += 1;
        }
    } else {
        println!("{}", crate::cid::from_file(&input)?);
    }
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
