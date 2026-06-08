//! CLI entry point for the `muxl` binary — the single MUXL command-line tool.
//!
//! It bundles every muxing subcommand (catalog, segment, wrap,
//! unwrap, cid, hls), reusing the building blocks from [`muxl::cli`],
//! alongside the sign-specific subcommands (sign-segment, sign-transcode,
//! verify, inspect, gen-key, gen-cert, …). The binary lives in this crate
//! rather than the core `muxl` crate because signing pulls in c2pa; keeping it
//! here leaves the `muxl` library (and its wasm builds) free of that
//! dependency. Streamplace embeds the same artifact compiled to wasm
//! (`muxl.wasm`), covering both the unsigned-muxing and per-track signing
//! paths.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use muxl::cli as muxl_cli;

use crate::{
    Result, SignerKey, SigningAlg, cert, sign_segment_stream, sign_segment_stream_host,
    sign_transcode_segment, verify_segments,
};

#[derive(Parser)]
#[command(
    name = "muxl",
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
    /// Human-readable inspection of a MUXL segment file: per-track codec
    /// info plus signing info (signer DID, validation state, actions,
    /// ingredients) when a C2PA/S2PA manifest is attached. Colorized when
    /// stdout is a TTY.
    Inspect(InspectArgs),

    /// Generate a fresh secp256k1 (ES256K) private key as PKCS#8 PEM.
    /// Pair with `gen-cert` to produce a signer for muxl.
    GenKey(GenKeyArgs),
    /// Generate an S2PA self-signed leaf certificate (X.509 v3) for an
    /// existing secp256k1 PKCS#8 PEM private key. The cert's
    /// `commonName` is the DID identifying the signer — defaults to the
    /// `did:key` of the embedded public key.
    GenCert(GenCertArgs),

    // muxl subcommands, lifted verbatim. --------------------------------------
    /// Extract catalog (track config) from an MP4.
    Catalog(muxl_cli::CatalogArgs),
    /// Segment an MP4 (flat or fragmented) into MUXL segments — to a directory,
    /// a single fMP4 (--fmp4) or flat MP4 (--flat), or a CBOR event stream.
    Segment(muxl_cli::SegmentArgs),
    /// Wrap one or more MUXL wrappers into a presentation MP4 (fMP4 or flat),
    /// `tar`-style (output first, then inputs); "-" reads stdin / writes
    /// stdout. Or name the output with --flat <PATH> / --fmp4 <PATH>, in which
    /// case every positional is an input. With --init-only, emit just the
    /// synthesized init segment (the inbound header-synthesis the host runs
    /// per segment).
    Wrap(muxl_cli::WrapArgs),
    /// Unwrap any MUXL wrapper (fMP4/flat/bare m4s) into its canonical segments.
    Unwrap(muxl_cli::UnwrapArgs),
    /// Print the BDASL CID of a whole file, or of each canonical segment.
    Cid(muxl_cli::CidArgs),
    /// Generate HLS playback artifacts (CID-addressed blobs + optional playlists).
    Hls(muxl_cli::HlsArgs),
}

#[derive(clap::Args)]
#[command(group(
    ArgGroup::new("signing-key")
        .required(true)
        .args(["key", "host_sign"])
))]
#[command(group(
    // Manifests come from either static paths (CLI/file path) or per-segment
    // host fetches. `--host-manifest` excludes the static paths; without it the
    // two PATH args are required.
    ArgGroup::new("manifest-source")
        .required(true)
        .multiple(true)
        .args(["track_manifest", "host_manifest"])
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
    /// JSON manifest applied to each per-track signed asset. Required unless
    /// `--host-manifest` is set.
    #[arg(long, value_name = "PATH", conflicts_with = "host_manifest")]
    track_manifest: Option<PathBuf>,
    /// JSON manifest applied to the multi-track wrapper. Required unless
    /// `--host-manifest` is set.
    #[arg(long, value_name = "PATH", requires = "track_manifest")]
    wrapper_manifest: Option<PathBuf>,
    /// Fetch a fresh track + wrapper manifest from the wasm host (via the
    /// `muxl.host_get_manifest` import) before signing each GoP, instead of
    /// reading them from disk. Lets a long-lived streaming signer reflect
    /// mid-stream manifest updates — e.g. Streamplace's livestream record
    /// transitioning from pre-live to live. Only useful inside a wasm runtime
    /// that wires the import up; native runs error on the first segment.
    /// Mutually exclusive with `--track-manifest`/`--wrapper-manifest`.
    #[arg(long)]
    host_manifest: bool,
    /// Optional RFC 3161 timestamp authority URL.
    #[arg(long, value_name = "URL")]
    tsa_url: Option<String>,
}

/// What [`cmd_sign_segment`] gets from a parsed [`SigningArgs`]: a signer plus
/// the manifest source — either static strings or a directive to fetch from
/// the wasm host once per GoP.
enum ManifestSource {
    Static { track: String, wrapper: String },
    Host,
}

impl SigningArgs {
    fn into_signer_and_manifests(self) -> Result<(SignerKey, ManifestSource)> {
        let SigningArgs {
            cert,
            key,
            host_sign,
            alg,
            track_manifest,
            wrapper_manifest,
            host_manifest,
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
        let manifests = if host_manifest {
            ManifestSource::Host
        } else {
            // ArgGroup guarantees --track-manifest is set when --host-manifest is not;
            // `requires` chains --wrapper-manifest to --track-manifest.
            let track = track_manifest
                .expect("clap ArgGroup guarantees --track-manifest when --host-manifest is absent");
            let wrapper = wrapper_manifest
                .expect("clap `requires` guarantees --wrapper-manifest with --track-manifest");
            ManifestSource::Static {
                track: fs::read_to_string(&track)?,
                wrapper: fs::read_to_string(&wrapper)?,
            }
        };
        Ok((signer, manifests))
    }
}

#[derive(clap::Args)]
struct InspectArgs {
    /// MUXL segment file to inspect (bare .m4s, fMP4, or flat MP4).
    input: PathBuf,
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
#[command(group(
    ArgGroup::new("cert-key")
        .required(true)
        .args(["key", "pubkey"])
))]
struct GenCertArgs {
    /// PKCS#8 PEM secp256k1 private key to self-sign with. Use `-` for stdin.
    /// Mutually exclusive with `--pubkey`.
    #[arg(long, value_name = "PATH")]
    key: Option<PathBuf>,
    /// Hex-encoded 65-byte uncompressed secp256k1 public key (0x04 || X || Y)
    /// to issue the cert for, signing the TBSCertificate via `--host-sign`
    /// rather than an in-process key. Mutually exclusive with `--key`.
    #[arg(long, value_name = "HEX")]
    pubkey: Option<String>,
    /// Sign the TBSCertificate via the wasm host (`muxl.host_sign`) so the
    /// private key never enters the sandbox. Required with `--pubkey`.
    #[arg(long)]
    host_sign: bool,
    /// DID for the cert's `commonName`. Defaults to the `did:key`
    /// identifier of the public key.
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
        Command::Inspect(args) => cmd_inspect(args),
        Command::GenKey(args) => cmd_gen_key(args),
        Command::GenCert(args) => cmd_gen_cert(args),
        // muxl subcommands delegate to muxl::cli::dispatch via its
        // matching enum variant — we just rebuild the muxl Command from
        // our payload and hand it off.
        Command::Catalog(args) => muxl_cli::cmd_catalog(args).map_err(Into::into),
        Command::Segment(args) => muxl_cli::cmd_segment(args).map_err(Into::into),
        Command::Wrap(args) => muxl_cli::cmd_wrap(args).map_err(Into::into),
        Command::Unwrap(args) => muxl_cli::cmd_unwrap(args).map_err(Into::into),
        Command::Cid(args) => muxl_cli::cmd_cid(args).map_err(Into::into),
        Command::Hls(args) => muxl_cli::cmd_hls(args).map_err(Into::into),
    };
    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn cmd_sign_segment(args: SignSegmentArgs) -> Result<()> {
    let (signer, manifests) = args.signing.into_signer_and_manifests()?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    match manifests {
        ManifestSource::Static { track, wrapper } => {
            sign_segment_stream(&mut stdin, &mut stdout, &signer, &track, &wrapper)
        }
        ManifestSource::Host => sign_segment_stream_host(&mut stdin, &mut stdout, &signer),
    }
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

fn cmd_inspect(args: InspectArgs) -> Result<()> {
    crate::inspect::inspect_file(&args.input)
}

fn cmd_verify() -> Result<()> {
    let mut buf = Vec::new();
    io::stdin().lock().read_to_end(&mut buf)?;
    let json = verify_segments(&buf)?;
    io::stdout().lock().write_all(json.as_bytes())?;
    Ok(())
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
    let der = if let Some(pubkey_hex) = args.pubkey.as_deref() {
        // Host-signed path: issue a cert for an external secp256k1 key,
        // signing the TBSCertificate via the wasm host (e.g. a Livepeer
        // orchestrator's Ethereum keystore). The private key never enters
        // this process — only its public key (for the SPKI/DID) does.
        if !args.host_sign {
            return Err(invalid_data("--pubkey requires --host-sign"));
        }
        let pubkey = decode_hex(pubkey_hex)?;
        cert::generate_cert_with_signer(
            &pubkey,
            args.did.as_deref(),
            args.organization.as_deref(),
            |tbs_der| crate::sign::host_sign_callback(tbs_der, SigningAlg::Es256K).map_err(invalid_data),
        )?
    } else {
        // In-process path: self-sign with a PEM private key.
        use k256::pkcs8::DecodePrivateKey;
        let key_path = args
            .key
            .expect("clap ArgGroup guarantees --key when --pubkey is absent");
        let key_pem = read_in(&key_path)?;
        let key_str = std::str::from_utf8(&key_pem)
            .map_err(|_| invalid_data("key file is not UTF-8 PEM"))?;
        let key = k256::SecretKey::from_pkcs8_pem(key_str)
            .map_err(|e| invalid_data(format!("parsing PKCS#8 PEM key: {e}")))?;
        cert::generate_cert(&key, args.did.as_deref(), args.organization.as_deref())?
    };

    let pem = cert::cert_to_pem(&der);
    write_out(&args.out, pem.as_bytes())?;
    if args.out.as_os_str() != "-" {
        eprintln!("wrote {}", args.out.display());
    }
    Ok(())
}

/// Build an InvalidData error from a message.
fn invalid_data(msg: impl Into<String>) -> crate::Error {
    crate::Error::from(muxl::Error::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        msg.into(),
    )))
}

/// Decode a hex string (optional `0x` prefix) into bytes.
fn decode_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() % 2 != 0 {
        return Err(invalid_data("hex string has an odd length"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| invalid_data(format!("bad hex: {e}")))
        })
        .collect()
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
