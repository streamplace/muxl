use std::env;
use std::fs::File;
use std::process;

fn usage() -> ! {
    eprintln!("Usage: muxl <command> [args...]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  canonicalize <input.mp4> <output.mp4>   Canonicalize an MP4 file");
    eprintln!("  segment <input.mp4> <output>            Split into signable segments");
    eprintln!("  concatenate <output.mp4> <seg1> [seg2...]  Combine segments into one MP4");
    process::exit(1);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }

    let result = match args[1].as_str() {
        "canonicalize" => cmd_canonicalize(&args[2..]),
        "segment" => cmd_segment(&args[2..]),
        "concatenate" => cmd_concatenate(&args[2..]),
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

fn cmd_canonicalize(args: &[String]) -> muxl::Result<()> {
    if args.len() != 2 {
        eprintln!("Usage: muxl canonicalize <input.mp4> <output.mp4>");
        process::exit(1);
    }
    let input = File::open(&args[0])?;
    let output = File::create(&args[1])?;
    muxl::canonicalize(input, output)
}

fn cmd_segment(args: &[String]) -> muxl::Result<()> {
    if args.len() != 2 {
        eprintln!("Usage: muxl segment <input.mp4> <output>");
        process::exit(1);
    }
    let input = File::open(&args[0])?;
    let output = File::create(&args[1])?;
    muxl::segment(input, output)
}

fn cmd_concatenate(args: &[String]) -> muxl::Result<()> {
    if args.len() < 2 {
        eprintln!("Usage: muxl concatenate <output.mp4> <seg1> [seg2...]");
        process::exit(1);
    }
    let output = File::create(&args[0])?;
    let mut inputs: Vec<File> = args[1..]
        .iter()
        .map(|p| File::open(p).map_err(muxl::Error::from))
        .collect::<muxl::Result<Vec<_>>>()?;
    muxl::concatenate(&mut inputs, output)
}
