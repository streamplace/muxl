//! `muxl` CLI building blocks.
//!
//! Not a CLI of its own: the single `muxl` binary is built by the `muxl` crate
//! (crates/muxl), which composes these pieces with its signing subcommands. This
//! module exports the reusable parts so that consolidated CLI needn't duplicate
//! any arg-parsing code:
//!
//! - One named `*Args` struct per subcommand (e.g. [`CatalogArgs`],
//!   [`SegmentArgs`]).
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
#[command(group(ArgGroup::new("mode").required(true).args(["dir", "fmp4", "stdout", "flat"])))]
pub struct SegmentArgs {
    /// Input MP4. A file path is read with random access, so --flat and --fmp4
    /// accept a flat (faststart) *or* fragmented MP4. "-" is stdin: --flat
    /// slurps it; --dir/--fmp4/--stdout stream it and so need a fragmented
    /// (fMP4) stream (the live-ingest path).
    pub input: String,
    /// Write segments into this directory (one file per segment).
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Emit a single MUXL fMP4 file covering the whole input. From a file this
    /// reads with random access (flat or fragmented input); from stdin ("-") it
    /// stream-segments a fragmented pipe (live ingest). "-" writes stdout.
    #[arg(long, value_name = "FILE")]
    pub fmp4: Option<PathBuf>,
    /// Stream segments to stdout as framed CBOR events.
    #[arg(long)]
    pub stdout: bool,
    /// Canonicalize the whole input into a single flat (faststart) MP4 at this
    /// path ("-" for stdout). Reads with random access, so it accepts a flat
    /// or fragmented input — the one-shot "any MP4 → canonical flat MP4" path.
    #[arg(long, value_name = "FILE")]
    pub flat: Option<PathBuf>,
    /// Relabel a track id in the output, given as `OLD:NEW` (repeatable). The
    /// emitted catalog and every minted fragment are minted at the new id —
    /// e.g. to give a transcoded rendition a free id so it concatenates
    /// alongside the tracks it derives from without colliding.
    #[arg(long = "remap-track", value_name = "OLD:NEW", value_parser = parse_remap_pair)]
    pub remap_track: Vec<(u32, u32)>,
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

pub fn cmd_segment(args: SegmentArgs) -> crate::Result<()> {
    let remap: std::collections::BTreeMap<u32, u32> = args.remap_track.into_iter().collect();

    // --flat and --fmp4 emit a single bounded presentation file. Given a
    // seekable input they read with random access — which builds the full
    // sample plan and so also accepts a *flat* (faststart) MP4, not just a
    // fragmented one — while minting with constant memory. The streaming modes
    // further down take a plain Read (fragmented input only): that's the
    // live-ingest path, minting segments from chunks as they arrive.
    if let Some(flat_out) = &args.flat {
        return cmd_segment_presentation(&args.input, flat_out, remap, Presentation::Flat);
    }
    // A seekable --fmp4 input takes the same random-access path: byte-identical
    // to the streaming segmenter for fragmented input, plus it handles flat.
    // stdin can't seek, so "-" falls through to the streaming segmenter below,
    // preserving live fMP4 minting from a pipe.
    if let Some(fmp4_out) = &args.fmp4 {
        if args.input != "-" {
            return cmd_segment_presentation(&args.input, fmp4_out, remap, Presentation::Fmp4);
        }
    }

    let mut input: Box<dyn Read> = if args.input == "-" {
        Box::new(io::stdin().lock())
    } else {
        Box::new(fs::File::open(&args.input)?)
    };

    if let Some(dir) = args.dir {
        cmd_segment_dir(&mut input, &dir, remap)
    } else if let Some(file) = args.fmp4 {
        // Reached only for stdin (a seekable file took the random-access path
        // above): stream-segment the fragmented pipe into one fMP4.
        cmd_segment_fmp4_stream(&mut input, &file, remap)
    } else if args.stdout {
        cmd_segment_stdout(&mut input, remap)
    } else {
        // clap's ArgGroup guarantees one mode is set; unreachable in practice.
        unreachable!("segment requires --dir, --fmp4, --stdout, or --flat")
    }
}

/// Target container for the random-access "whole input → one presentation
/// file" segment modes.
#[derive(Clone, Copy)]
enum Presentation {
    /// Finalized flat (faststart) MP4 — `segment --flat`.
    Flat,
    /// Appendable fMP4 — `segment --fmp4` reading a seekable file.
    Fmp4,
}

/// Canonicalize an arbitrary MP4 (flat *or* fragmented) into a single
/// presentation file. Unlike the streaming modes (`--dir`/`--stdout`, and
/// `--fmp4` reading stdin), this reads with random access: `crate::read` builds
/// the full sample plan in one pass, then the writer mints the canonical output
/// while streaming sample bytes from the input on demand (constant memory).
/// That random-access read is what lets it accept a flat (faststart) input.
/// "-" slurps stdin into memory / writes stdout.
fn cmd_segment_presentation(
    input: &str,
    output: &Path,
    remap: std::collections::BTreeMap<u32, u32>,
    fmt: Presentation,
) -> crate::Result<()> {
    let sink: Box<dyn Write> = if output.as_os_str() == "-" {
        Box::new(io::stdout().lock())
    } else {
        Box::new(fs::File::create(output)?)
    };
    let mut out = CountingWriter::new(BufWriter::new(sink));

    // pread(2) isn't available on stdin (nor on wasm), so "-" slurps into the
    // in-memory ReadAt impl; a real path uses FileReadAt directly. Both expose
    // the same `ReadAt`, so the canonicalization below is identical. (These
    // owners must outlive `reader`, hence the deferred-init binding.)
    let file_reader;
    let slurped;
    let reader: &dyn crate::io::ReadAt = if input == "-" {
        let mut bytes = Vec::new();
        io::stdin().lock().read_to_end(&mut bytes)?;
        slurped = bytes;
        &slurped
    } else {
        file_reader = crate::io::FileReadAt::open(Path::new(input))?;
        &file_reader
    };

    let mut source = crate::read(reader)?;
    if !remap.is_empty() {
        source.remap_track_ids(&remap);
    }

    let (label, tracks) = match fmt {
        Presentation::Flat => (
            "flat MP4",
            crate::flat::write(&source, reader, &mut out)?.tracks.len(),
        ),
        Presentation::Fmp4 => ("fMP4", crate::fmp4::write(&source, reader, &mut out)?.len()),
    };
    out.flush()?;

    eprintln!("{label}: {} bytes ({} tracks)", out.count, tracks);
    Ok(())
}

/// A `Write` adapter that tallies bytes passed through to the inner writer —
/// used only to report output size for the random-access presentation modes,
/// whose writers don't return a total (and where the sink may be stdout).
struct CountingWriter<W> {
    inner: W,
    count: u64,
}

impl<W: Write> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, count: 0 }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn cmd_segment_dir(
    input: &mut impl Read,
    output_dir: &Path,
    remap: std::collections::BTreeMap<u32, u32>,
) -> crate::Result<()> {
    fs::create_dir_all(output_dir)?;

    let catalog = crate::segment_fmp4_with_remap(input, remap, |gop| {
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
fn cmd_segment_stdout(
    input: &mut impl Read,
    remap: std::collections::BTreeMap<u32, u32>,
) -> crate::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 64 * 1024];
    let mut segmenter = crate::Segmenter::with_remap(remap);

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

    // --events: re-emit the per-track CBOR event stream (for a host that drives
    // the live-HLS window off already-stored segments). Fully streaming — the
    // input is consumed front-to-back and never held in full, so even a
    // multi-GB VOD runs in ~one GoP of memory (well under the wasm cap). Each
    // event is serialized straight to stdout as it's finalized.
    if events {
        let reader: Box<dyn Read> = if input.as_os_str() == "-" {
            Box::new(io::stdin())
        } else {
            Box::new(fs::File::open(&input)?)
        };
        let mut stdout = io::stdout().lock();
        let mut n = 0usize;
        crate::reader::segment_events_stream(reader, |ev| {
            dasl::drisl::to_writer(&mut stdout, &ev).map_err(|e| {
                crate::Error::Io(io::Error::new(io::ErrorKind::Other, e.to_string()))
            })?;
            n += 1;
            Ok(())
        })?;
        stdout.flush()?;
        eprintln!("emitted {n} events");
        return Ok(());
    }

    // Non-events paths unwrap to verbatim slices, which borrow the whole input,
    // so they still slurp it.
    let bytes: Vec<u8> = if input.as_os_str() == "-" {
        let mut buf = Vec::new();
        io::stdin().lock().read_to_end(&mut buf)?;
        buf
    } else {
        fs::read(&input)?
    };
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


/// Streaming `--fmp4`: stream-segment a fragmented (fMP4) input arriving on a
/// plain `Read` (e.g. a stdin pipe / live ingest) into one fMP4 file. A
/// seekable file takes the random-access path in [`cmd_segment`] instead — both
/// emit byte-identical output for the same fragmented input. Unlike that path,
/// this cannot accept a flat input (no moofs ⇒ no segments).
fn cmd_segment_fmp4_stream(
    input: &mut impl Read,
    output_path: &Path,
    remap: std::collections::BTreeMap<u32, u32>,
) -> crate::Result<()> {
    let mut gops = Vec::new();

    let catalog = crate::segment_fmp4_with_remap(input, remap, |gop| {
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

    // Canonical interleave (spec § Presentation Formats): each GoP's tracks
    // contiguously in track_id order (gop.tracks is a BTreeMap), then the next
    // GoP — NOT grouped per-track.
    let mut fmp4 = crate::fmp4::init_segment(&catalog)?;
    for gop in &gops {
        for data in gop.tracks.values() {
            fmp4.extend_from_slice(data);
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
