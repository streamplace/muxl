use std::env;
use std::fs;
use std::io::{self, Cursor, Read};
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

    let catalog = crate::segment_fmp4(input, |seg| {
        let filename = output_dir.join(format!("segment_{:04}.m4s", seg.number));
        fs::write(&filename, &seg.data)?;
        eprintln!("segment {:4}: {} bytes", seg.number, seg.data.len());
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
        crate::SegmenterEvent::Segment(seg) => {
            eprintln!("segment: {} bytes", seg.data.len());
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

fn cmd_segment_archive(input: &mut impl Read, output_path: &str) -> crate::Result<()> {
    let mut segments: Vec<Vec<u8>> = Vec::new();

    let catalog = crate::segment_fmp4(input, |seg| {
        eprintln!("segment {:4}: {} bytes", seg.number, seg.data.len());
        segments.push(seg.data);
        Ok(())
    })?;

    // Build archive: init + all segments
    let init = crate::build_init_segment(&catalog)?;
    let total_size: usize = init.len() + segments.iter().map(|s| s.len()).sum::<usize>();

    let mut archive = Vec::with_capacity(total_size);
    archive.extend_from_slice(&init);
    for seg in &segments {
        archive.extend_from_slice(seg);
    }

    fs::write(output_path, &archive)?;
    eprintln!(
        "archive: {} bytes ({} segments)",
        archive.len(),
        segments.len()
    );

    Ok(())
}
