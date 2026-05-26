//! CLI entry point for the `muxl-sign` binary.
//!
//! Consolidated CLI: every `muxl` subcommand is reachable here under the
//! same name (catalog, fmp4, mp4, segment, concat, hls), plus the
//! sign-specific subcommands `sign-per-track` and `sign-segment`. This
//! lets Streamplace ship a single `muxl-sign.wasm` that covers both the
//! unsigned-muxing path and the per-track signing path.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process;
use std::time::Instant;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use muxl::cli as muxl_cli;

use crate::{
    Result, SignerKey, SigningAlg, cert, sign_segment_stream, sign_transcode_segment,
    verify_segments,
};

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
    /// Stream-sign an fMP4 input on stdin: for each GoP emitted by the
    /// MUXL segmenter, produce one signed flat MP4 (per-track + wrapper)
    /// as a CBOR `signed-segment` event on stdout.
    SignSegment(SignSegmentArgs),
    /// Sign a transcoded canonical MUXL segment read from stdin, declaring
    /// the segment it was transcoded from (`--source`) as a `parentOf`
    /// ingredient. The output's C2PA manifest (`--manifest`) should carry a
    /// `c2pa.transcoded` action referencing the source (see
    /// `muxl_sign::TRANSCODE_INGREDIENT_LABEL`). Writes the signed segment to
    /// stdout. This is the provenance step a Livepeer orchestrator runs after
    /// transcoding a signed MUXL segment.
    SignTranscode(SignTranscodeArgs),
    /// Verify the C2PA/S2PA signatures on a signed MUXL wrapper read from
    /// stdin (bare .m4s stream, fMP4, or flat MP4). Each canonical segment
    /// is validated standalone as an `m4s` asset. Emits a JSON document
    /// `{"segments":[{track_id,manifest,cert,validation_results,validation_state}]}`
    /// on stdout — the per-track equivalent of the manifest+cert blob the
    /// host used to get from the iroh-streamplace c2pa binding.
    Verify,
    /// Microbenchmark: hash N×size bytes via either in-wasm sha2 or the
    /// `streamplace.host_sha256` import. Used to size the upper bound on
    /// what a host-SHA256 path could save before committing to a
    /// full-crate sha2 patch in c2pa-rs's hot path.
    BenchSha256(BenchSha256Args),

    /// Generate a fresh secp256k1 (ES256K) private key as PKCS#8 PEM.
    /// Pair with `gen-cert` to produce a signer for muxl-sign.
    GenKey(GenKeyArgs),
    /// Generate an S2PA self-signed leaf certificate (X.509 v3) for an
    /// existing secp256k1 PKCS#8 PEM private key. The cert's
    /// `commonName` is the DID identifying the signer — defaults to the
    /// `did:key` of the embedded public key.
    GenCert(GenCertArgs),

    // muxl subcommands, lifted verbatim. --------------------------------------
    /// Extract catalog (track config) from an MP4.
    Catalog(muxl_cli::CatalogArgs),
    /// Write a canonical MUXL fMP4 (or just its init segment with --init-only).
    Fmp4(muxl_cli::Fmp4Args),
    /// Write a canonical MUXL flat MP4 (faststart) from an input MP4.
    Mp4(muxl_cli::Mp4Args),
    /// Segment an fMP4 into per-GoP MUXL segments.
    Segment(muxl_cli::SegmentArgs),
    /// Wrap MUXL segments into a presentation MP4 (fMP4 or flat); "-" reads
    /// stdin / writes stdout. With --init-only, emit just the synthesized
    /// init segment (the inbound header-synthesis the host runs per segment).
    Wrap(muxl_cli::WrapArgs),
    /// Unwrap any MUXL wrapper (fMP4/flat/bare m4s) into its canonical segments.
    Unwrap(muxl_cli::UnwrapArgs),
    /// Print the BDASL CID of a whole file, or of each canonical segment.
    Cid(muxl_cli::CidArgs),
    /// Concatenate MUXL fMP4 files from stdin, emit CBOR events to stdout.
    Concat,
    /// Synthesize a multi-segment flat MP4 header from per-segment metadata.
    /// Reads a single CBOR document on stdin: `{catalog, segments: [...]}`.
    /// Emits the header bytes (ftyp + moov + mdat-envelope-header) on
    /// stdout. Streamplace's VOD-finalize pipeline pipes this in front of
    /// each segment's body bytes (typically via S3 multipart upload).
    SynthFlat,
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
struct SignSegmentArgs {
    #[command(flatten)]
    signing: SigningArgs,
}

#[derive(clap::Args)]
#[command(group(
    ArgGroup::new("transcode-signing-key")
        .required(true)
        .args(["key", "host_sign"])
))]
struct SignTranscodeArgs {
    /// PEM-encoded signing cert chain (leaf first).
    #[arg(long, value_name = "PATH")]
    cert: PathBuf,
    /// PEM-encoded private key matching `--cert`. Mutually exclusive with
    /// `--host-sign`.
    #[arg(long, value_name = "PATH")]
    key: Option<PathBuf>,
    /// Delegate signing to the wasm host via the `muxl.host_sign` import; the
    /// private key never enters the wasm sandbox. Mutually exclusive with
    /// `--key`.
    #[arg(long)]
    host_sign: bool,
    /// Signing algorithm. Defaults to ES256K (Streamplace's default).
    #[arg(long, value_enum, default_value_t = Alg::Es256K)]
    alg: Alg,
    /// The canonical MUXL `.m4s` segment that was transcoded — added to the
    /// output's manifest as a `parentOf` ingredient.
    #[arg(long, value_name = "PATH")]
    source: PathBuf,
    /// JSON C2PA manifest for the signed output; should carry a
    /// `c2pa.transcoded` action referencing the source ingredient.
    #[arg(long, value_name = "PATH")]
    manifest: PathBuf,
    /// Optional RFC 3161 timestamp authority URL.
    #[arg(long, value_name = "URL")]
    tsa_url: Option<String>,
}

#[derive(clap::Args)]
struct GenKeyArgs {
    /// Path to write the PKCS#8 PEM private key. Use `-` for stdout.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,
}

#[derive(clap::Args)]
struct GenCertArgs {
    /// PKCS#8 PEM secp256k1 private key. Use `-` for stdin.
    #[arg(long, value_name = "PATH")]
    key: PathBuf,
    /// DID for the cert's `commonName`. Defaults to the `did:key`
    /// identifier of the key's public key.
    #[arg(long, value_name = "DID")]
    did: Option<String>,
    /// Optional `organizationName` attribute alongside `commonName` in
    /// the DN. Included for compatibility with C2PA libraries that
    /// expect more than just a CN. Streamplace's production certs use
    /// "Streamplace" here.
    #[arg(long, value_name = "ORG")]
    organization: Option<String>,
    /// Path to write the PEM-encoded cert. Use `-` for stdout.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,
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
        Command::SignSegment(args) => cmd_sign_segment(args),
        Command::SignTranscode(args) => cmd_sign_transcode(args),
        Command::Verify => cmd_verify(),
        Command::BenchSha256(args) => cmd_bench_sha256(args),
        Command::GenKey(args) => cmd_gen_key(args),
        Command::GenCert(args) => cmd_gen_cert(args),
        // muxl subcommands delegate to muxl::cli::dispatch via its
        // matching enum variant — we just rebuild the muxl Command from
        // our payload and hand it off.
        Command::Catalog(args) => muxl_cli::cmd_catalog(args).map_err(Into::into),
        Command::Fmp4(args) => muxl_cli::cmd_fmp4(args).map_err(Into::into),
        Command::Mp4(args) => muxl_cli::cmd_mp4(args).map_err(Into::into),
        Command::Segment(args) => muxl_cli::cmd_segment(args).map_err(Into::into),
        Command::Wrap(args) => muxl_cli::cmd_wrap(args).map_err(Into::into),
        Command::Unwrap(args) => muxl_cli::cmd_unwrap(args).map_err(Into::into),
        Command::Cid(args) => muxl_cli::cmd_cid(args).map_err(Into::into),
        Command::Concat => muxl_cli::cmd_concat().map_err(Into::into),
        Command::SynthFlat => cmd_synth_flat(),
        Command::Hls(args) => muxl_cli::cmd_hls(args).map_err(Into::into),
    };
    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
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

fn cmd_sign_transcode(args: SignTranscodeArgs) -> Result<()> {
    let SignTranscodeArgs {
        cert,
        key,
        host_sign,
        alg,
        source,
        manifest,
        tsa_url,
    } = args;

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

    let source_segment = fs::read(&source)?;
    let manifest_json = fs::read_to_string(&manifest)?;

    // The transcoded output to sign arrives on stdin.
    let mut output_segment = Vec::new();
    io::stdin().lock().read_to_end(&mut output_segment)?;

    let signed = sign_transcode_segment(&output_segment, &source_segment, &signer, &manifest_json)?;
    io::stdout().lock().write_all(&signed)?;
    Ok(())
}

fn cmd_verify() -> Result<()> {
    let mut buf = Vec::new();
    io::stdin().lock().read_to_end(&mut buf)?;
    let json = verify_segments(&buf)?;
    io::stdout().lock().write_all(json.as_bytes())?;
    Ok(())
}

/// Wire shape for `muxl-sign synth-flat`'s stdin: a single CBOR document
/// carrying the catalog and a sequence of per-segment metadata. The
/// fields mirror [`muxl::SegmentMetadata`] one-to-one.
#[derive(serde::Deserialize)]
struct SynthFlatInput {
    catalog: muxl::catalog::Catalog,
    segments: Vec<SynthFlatSegment>,
}

#[derive(serde::Deserialize)]
struct SynthFlatSegment {
    track_byte_sizes: std::collections::BTreeMap<String, u64>,
    samples: std::collections::BTreeMap<String, SynthFlatTrackSamples>,
    #[serde(default)]
    first_decode_times: std::collections::BTreeMap<String, u64>,
}

#[derive(serde::Deserialize)]
struct SynthFlatTrackSamples {
    durations: Vec<u32>,
    sizes: Vec<u32>,
    #[serde(default)]
    cts_offsets: Vec<i32>,
    #[serde(default)]
    sync_indices: Vec<u32>,
    offsets: Vec<u64>,
}

fn cmd_synth_flat() -> Result<()> {
    let mut buf = Vec::new();
    io::stdin().lock().read_to_end(&mut buf)?;
    let input: SynthFlatInput = dasl::drisl::from_slice(&buf).map_err(|e| {
        crate::Error::from(muxl::Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("decoding synth-flat input: {e}"),
        )))
    })?;

    let mut segments: Vec<muxl::SegmentMetadata> = Vec::with_capacity(input.segments.len());
    for s in input.segments {
        let track_byte_sizes = parse_track_keyed_map(s.track_byte_sizes)?;
        let first_decode_times = parse_track_keyed_map(s.first_decode_times)?;
        let mut samples = std::collections::BTreeMap::new();
        for (k, v) in s.samples {
            let tid = parse_track_id(&k)?;
            let n = v.durations.len();
            let cts_offsets = if v.cts_offsets.is_empty() {
                vec![0; n]
            } else {
                v.cts_offsets
            };
            samples.insert(
                tid,
                muxl::segment::TrackSamples {
                    durations: v.durations,
                    sizes: v.sizes,
                    cts_offsets,
                    sync_indices: v.sync_indices,
                    offsets_in_track: v.offsets,
                },
            );
        }
        segments.push(muxl::SegmentMetadata {
            track_byte_sizes,
            samples,
            first_decode_times,
        });
    }

    let header = muxl::build_synth_flat_header(&input.catalog, &segments)?;
    io::stdout().lock().write_all(&header)?;
    Ok(())
}

fn parse_track_id(k: &str) -> Result<u32> {
    k.parse().map_err(|_| {
        crate::Error::from(muxl::Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("non-numeric track id {k:?}"),
        )))
    })
}

fn parse_track_keyed_map<V>(
    m: std::collections::BTreeMap<String, V>,
) -> Result<std::collections::BTreeMap<u32, V>> {
    let mut out = std::collections::BTreeMap::new();
    for (k, v) in m {
        out.insert(parse_track_id(&k)?, v);
    }
    Ok(out)
}

fn cmd_gen_key(args: GenKeyArgs) -> Result<()> {
    let key = cert::generate_key();
    let pem = cert::key_to_pem(&key)?;
    write_out(&args.out, pem.as_bytes())?;
    if args.out.as_os_str() != "-" {
        let did = cert::did_key_for(&key.public_key());
        eprintln!("wrote {} ({})", args.out.display(), did);
    }
    Ok(())
}

fn cmd_gen_cert(args: GenCertArgs) -> Result<()> {
    use k256::pkcs8::DecodePrivateKey;
    let key_pem = read_in(&args.key)?;
    let key_str = std::str::from_utf8(&key_pem).map_err(|_| {
        crate::Error::from(muxl::Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "key file is not UTF-8 PEM",
        )))
    })?;
    let key = k256::SecretKey::from_pkcs8_pem(key_str).map_err(|e| {
        crate::Error::from(muxl::Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parsing PKCS#8 PEM key: {e}"),
        )))
    })?;
    let der = cert::generate_cert(
        &key,
        args.did.as_deref(),
        args.organization.as_deref(),
    )?;
    let pem = cert::cert_to_pem(&der);
    write_out(&args.out, pem.as_bytes())?;
    if args.out.as_os_str() != "-" {
        let did = args
            .did
            .clone()
            .unwrap_or_else(|| cert::did_key_for(&key.public_key()));
        eprintln!("wrote {} (CN={})", args.out.display(), did);
    }
    Ok(())
}

fn read_in(path: &PathBuf) -> Result<Vec<u8>> {
    if path.as_os_str() == "-" {
        let mut buf = Vec::new();
        io::stdin().lock().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(fs::read(path)?)
    }
}

fn write_out(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    if path.as_os_str() == "-" {
        io::stdout().lock().write_all(bytes)?;
    } else {
        fs::write(path, bytes)?;
    }
    Ok(())
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
