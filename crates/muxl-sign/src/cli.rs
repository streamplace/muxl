//! CLI entry point for the `muxl-sign` binary.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand, ValueEnum};

use crate::{Result, SignerKey, SigningAlg, sign_per_track, sign_segment_stream};

#[derive(Parser)]
#[command(
    name = "muxl-sign",
    about = "Per-track C2PA signing for MUXL flat MP4s",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Split a multi-track flat MP4 per-track, sign each, and combine
    /// into a wrapper signed flat MP4 whose manifest carries each
    /// per-track signed asset as a c2pa Ingredient.
    SignPerTrack(SignPerTrackArgs),
    /// Stream-sign an fMP4 input on stdin: for each GoP emitted by the
    /// MUXL segmenter, produce one signed flat MP4 (per-track + wrapper)
    /// as a CBOR `signed-segment` event on stdout.
    Segment(SegmentArgs),
}

#[derive(clap::Args)]
struct SigningArgs {
    /// PEM-encoded signing cert chain (leaf first).
    #[arg(long, value_name = "PATH")]
    cert: PathBuf,
    /// PEM-encoded private key matching `--cert`.
    #[arg(long, value_name = "PATH")]
    key: PathBuf,
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
            alg,
            track_manifest,
            wrapper_manifest,
            tsa_url,
        } = self;
        let mut signer = SignerKey::from_pem_files(&cert, &key, alg.into())?;
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
struct SegmentArgs {
    #[command(flatten)]
    signing: SigningArgs,
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
        Command::Segment(args) => cmd_segment(args),
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

    // FileReadAt uses pread(2) which isn't implemented for wasip1; read the
    // input into a Vec<u8> first so we can lean on the in-memory ReadAt impl
    // and stay portable across native + WASM builds.
    let input_bytes: Vec<u8> = fs::read(&input)?;
    let source = muxl::read(&input_bytes)?;

    let out_file = fs::File::create(&output)?;
    let mut out = BufWriter::new(out_file);
    sign_per_track(
        &source,
        &input_bytes,
        &signer,
        &track_manifest,
        &wrapper_manifest,
        &mut out,
    )?;
    out.flush()?;

    eprintln!(
        "signed {} ({} tracks) → {}",
        input.display(),
        source.plan.tracks.len(),
        output.display()
    );
    Ok(())
}

fn cmd_segment(args: SegmentArgs) -> Result<()> {
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
