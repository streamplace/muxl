//! CLI entry point for the `muxl-sign` binary.
//!
//! Consolidated CLI: every `muxl` subcommand is reachable here under the
//! same name (catalog, fmp4, mp4, segment, concat, hls), plus the
//! sign-specific subcommands `sign-per-track` and `sign-segment`. This
//! lets Streamplace ship a single `muxl-sign.wasm` that covers both the
//! unsigned-muxing path and the per-track signing path.

use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process;
use std::time::Instant;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use muxl::cli as muxl_cli;

use crate::{Result, SignerKey, SigningAlg, sign_per_track, sign_segment_stream};

#[derive(Parser)]
#[command(
    name = "muxl-sign",
    about = "MUXL canonicalization + per-track C2PA signing",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    // Sign-specific subcommands. ----------------------------------------------
    /// Split a multi-track flat MP4 per-track, sign each, and combine
    /// into a wrapper signed flat MP4 whose manifest carries each
    /// per-track signed asset as a c2pa Ingredient.
    SignPerTrack(SignPerTrackArgs),
    /// Stream-sign an fMP4 input on stdin: for each GoP emitted by the
    /// MUXL segmenter, produce one signed flat MP4 (per-track + wrapper)
    /// as a CBOR `signed-segment` event on stdout.
    SignSegment(SignSegmentArgs),
    /// Microbenchmark: hash N×size bytes via either in-wasm sha2 or the
    /// `streamplace.host_sha256` import. Used to size the upper bound on
    /// what a host-SHA256 path could save before committing to a
    /// full-crate sha2 patch in c2pa-rs's hot path.
    BenchSha256(BenchSha256Args),

    // muxl subcommands, lifted verbatim. --------------------------------------
    /// Extract catalog (track config) from an MP4.
    Catalog(muxl_cli::CatalogArgs),
    /// Write a canonical MUXL fMP4 (or just its init segment with --init-only).
    Fmp4(muxl_cli::Fmp4Args),
    /// Write a canonical MUXL flat MP4 (faststart) from an input MP4.
    Mp4(muxl_cli::Mp4Args),
    /// Segment an fMP4 into per-GoP MUXL segments.
    Segment(muxl_cli::SegmentArgs),
    /// Concatenate MUXL fMP4 files from stdin, emit CBOR events to stdout.
    Concat,
    /// Generate HLS playback artifacts (CID-addressed blobs + optional playlists).
    Hls(muxl_cli::HlsArgs),
}

#[derive(clap::Args)]
#[command(group(
    ArgGroup::new("signing-key")
        .required(true)
        .args(["key", "host_sign"])
))]
struct SigningArgs {
    /// PEM-encoded signing cert chain (leaf first).
    #[arg(long, value_name = "PATH")]
    cert: PathBuf,
    /// PEM-encoded private key matching `--cert`. Mutually exclusive with
    /// `--host-sign`.
    #[arg(long, value_name = "PATH")]
    key: Option<PathBuf>,
    /// Delegate signing to the wasm host via the `streamplace.host_sign`
    /// import. The host receives the bytes to sign and returns the
    /// signature; the private key never enters the wasm sandbox. Mutually
    /// exclusive with `--key`. Only useful inside a wasm runtime that
    /// wires up the import.
    #[arg(long)]
    host_sign: bool,
    /// Signing algorithm. Defaults to ES256K (Streamplace's default).
    #[arg(long, value_enum, default_value_t = Alg::Es256K)]
    alg: Alg,
    /// JSON manifest applied to each per-track signed asset.
    #[arg(long, value_name = "PATH")]
    track_manifest: PathBuf,
    /// JSON manifest applied to the multi-track wrapper.
    #[arg(long, value_name = "PATH")]
    wrapper_manifest: PathBuf,
    /// Optional RFC 3161 timestamp authority URL.
    #[arg(long, value_name = "URL")]
    tsa_url: Option<String>,
}

impl SigningArgs {
    fn into_signer_and_manifests(self) -> Result<(SignerKey, String, String)> {
        let SigningArgs {
            cert,
            key,
            host_sign,
            alg,
            track_manifest,
            wrapper_manifest,
            tsa_url,
        } = self;
        let mut signer = if host_sign {
            SignerKey::host_from_pem_file(&cert, alg.into())?
        } else {
            // ArgGroup guarantees one of {key, host_sign} is set.
            let key = key.expect("clap ArgGroup guarantees --key when --host-sign is absent");
            SignerKey::from_pem_files(&cert, &key, alg.into())?
        };
        if let Some(url) = tsa_url {
            signer = signer.with_tsa_url(url);
        }
        Ok((
            signer,
            fs::read_to_string(&track_manifest)?,
            fs::read_to_string(&wrapper_manifest)?,
        ))
    }
}

#[derive(clap::Args)]
struct SignPerTrackArgs {
    /// Input MP4 (flat or fragmented; auto-detected).
    #[arg(long, value_name = "PATH")]
    input: PathBuf,
    /// Output path for the signed wrapper flat MP4.
    #[arg(long, value_name = "PATH")]
    output: PathBuf,
    #[command(flatten)]
    signing: SigningArgs,
}

#[derive(clap::Args)]
struct SignSegmentArgs {
    #[command(flatten)]
    signing: SigningArgs,
}

#[derive(clap::Args)]
struct BenchSha256Args {
    /// Bytes of pseudo-random input per iteration.
    #[arg(long, default_value_t = 1_048_576)]
    size: usize,
    /// Number of iterations.
    #[arg(long, default_value_t = 100)]
    iterations: usize,
    /// Hashing backend.
    #[arg(long, value_enum, default_value_t = Sha256Mode::Wasm)]
    mode: Sha256Mode,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Sha256Mode {
    /// Hash inside wasm using the bundled `sha2` crate. Same code path
    /// c2pa-rs follows today.
    Wasm,
    /// Hash on the host via the `streamplace.host_sha256` import.
    Host,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Alg {
    Es256,
    #[value(name = "es256k")]
    Es256K,
    Es384,
    Es512,
    Ps256,
    Ps384,
    Ps512,
    Ed25519,
}

impl From<Alg> for SigningAlg {
    fn from(alg: Alg) -> Self {
        match alg {
            Alg::Es256 => SigningAlg::Es256,
            Alg::Es256K => SigningAlg::Es256K,
            Alg::Es384 => SigningAlg::Es384,
            Alg::Es512 => SigningAlg::Es512,
            Alg::Ps256 => SigningAlg::Ps256,
            Alg::Ps384 => SigningAlg::Ps384,
            Alg::Ps512 => SigningAlg::Ps512,
            Alg::Ed25519 => SigningAlg::Ed25519,
        }
    }
}

pub fn cli_main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::SignPerTrack(args) => cmd_sign_per_track(args),
        Command::SignSegment(args) => cmd_sign_segment(args),
        Command::BenchSha256(args) => cmd_bench_sha256(args),
        // muxl subcommands delegate to muxl::cli::dispatch via its
        // matching enum variant — we just rebuild the muxl Command from
        // our payload and hand it off.
        Command::Catalog(args) => muxl_cli::cmd_catalog(args).map_err(Into::into),
        Command::Fmp4(args) => muxl_cli::cmd_fmp4(args).map_err(Into::into),
        Command::Mp4(args) => muxl_cli::cmd_mp4(args).map_err(Into::into),
        Command::Segment(args) => muxl_cli::cmd_segment(args).map_err(Into::into),
        Command::Concat => muxl_cli::cmd_concat().map_err(Into::into),
        Command::Hls(args) => muxl_cli::cmd_hls(args).map_err(Into::into),
    };
    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn cmd_sign_per_track(args: SignPerTrackArgs) -> Result<()> {
    let SignPerTrackArgs {
        input,
        output,
        signing,
    } = args;
    let (signer, track_manifest, wrapper_manifest) = signing.into_signer_and_manifests()?;

    // "-" reads stdin / writes stdout. Lets a host runtime skip the
    // filesystem entirely for the hot input/output bytes — useful on
    // platforms where temp-file I/O is expensive (Windows %TEMP% on NTFS,
    // antivirus scanning, etc.). Cert/key/manifests stay path-based and
    // can come from a read-only FS mount.
    //
    // FileReadAt uses pread(2) which isn't implemented for wasip1; we
    // also slurp file inputs into a Vec<u8> so the same in-memory
    // ReadAt code path covers both file and stdin sources.
    let input_bytes: Vec<u8> = if input.as_os_str() == "-" {
        let mut buf = Vec::new();
        io::stdin().lock().read_to_end(&mut buf)?;
        buf
    } else {
        fs::read(&input)?
    };
    let source = muxl::read(&input_bytes)?;

    let mut out: Box<dyn Write> = if output.as_os_str() == "-" {
        Box::new(BufWriter::new(io::stdout().lock()))
    } else {
        Box::new(BufWriter::new(fs::File::create(&output)?))
    };
    sign_per_track(
        &source,
        &input_bytes,
        &signer,
        &track_manifest,
        &wrapper_manifest,
        &mut out,
    )?;
    out.flush()?;

    if input.as_os_str() != "-" && output.as_os_str() != "-" {
        eprintln!(
            "signed {} ({} tracks) → {}",
            input.display(),
            source.plan.tracks.len(),
            output.display()
        );
    }
    Ok(())
}

fn cmd_sign_segment(args: SignSegmentArgs) -> Result<()> {
    let (signer, track_manifest, wrapper_manifest) = args.signing.into_signer_and_manifests()?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    sign_segment_stream(
        &mut stdin,
        &mut stdout,
        &signer,
        &track_manifest,
        &wrapper_manifest,
    )
}

fn cmd_bench_sha256(args: BenchSha256Args) -> Result<()> {
    let BenchSha256Args { size, iterations, mode } = args;

    // Deterministic-but-non-trivial input so the optimizer can't fold
    // the hash to a constant. xorshift64 over a known seed.
    let mut buf = vec![0u8; size];
    let mut s: u64 = 0xdeadbeef_cafebabe;
    for chunk in buf.chunks_mut(8) {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let bytes = s.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }

    // One untimed warmup hash so wasm AOT / page-fault costs don't land
    // in the first iteration of the timed loop.
    let _ = sha256_once(&buf, mode);

    let start = Instant::now();
    let mut acc: u8 = 0;
    for _ in 0..iterations {
        let digest = sha256_once(&buf, mode);
        // Touch the digest so the compiler can't elide the call.
        acc ^= digest[0];
    }
    let elapsed = start.elapsed();

    let total_bytes = (size as u64) * (iterations as u64);
    let throughput_mb_s =
        (total_bytes as f64) / 1_048_576.0 / elapsed.as_secs_f64();
    let per_iter_us = elapsed.as_secs_f64() * 1e6 / (iterations as f64);

    println!(
        "mode={:?} size={} iterations={} elapsed={:?} per_iter={:.1}us throughput={:.1}MB/s sentinel={}",
        mode, size, iterations, elapsed, per_iter_us, throughput_mb_s, acc
    );
    Ok(())
}

fn sha256_once(data: &[u8], mode: Sha256Mode) -> [u8; 32] {
    match mode {
        Sha256Mode::Wasm => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(data);
            hasher.finalize().into()
        }
        Sha256Mode::Host => {
            let mut out = [0u8; 32];
            unsafe {
                crate::sign::host_sha256(
                    data.as_ptr() as u32,
                    data.len() as u32,
                    out.as_mut_ptr() as u32,
                );
            }
            out
        }
    }
}
