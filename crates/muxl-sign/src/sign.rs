//! Per-canonical-segment signing + wrapper signing.
//!
//! Each canonical segment (one track's fragments for one GoP, prefixed by
//! the muxl uuid + DRISL catalog) is treated as a standalone .m4s asset
//! and signed independently with c2pa-rs. Output is a multi-track flat MP4
//! whose body bytes are `[c2pa-uuid + muxl-uuid + moof+mdat]*` per
//! canonical segment. A wrapper c2pa-uuid at the file head signs the
//! synthesized ftyp+moov+mdat-envelope assembly.
//!
//! Signature semantics: the per-canonical-segment hash covers exactly the
//! canonical muxl bytes — `s2pa(muxl(data))`. The wrapper hash covers
//! everything in the assembly (auto-c2pa-default exclusions: /uuid c2pa,
//! /ftyp, /mfra). Extracting a canonical segment from the body yields a
//! self-verifying .m4s asset that doesn't depend on the surrounding flat
//! MP4 wrapper.
//!
//! Per-segment time: the streaming signer [`sign_segment_stream`] stamps each
//! GoP's signing time into its manifest's `cawg.metadata`/`dc:date` (RFC 3339
//! UTC, millisecond precision — `2019-09-22T18:22:57.000Z`), mutating an
//! existing `cawg.metadata` assertion or appending one. The time lives in
//! `dc:date` rather than a bespoke assertion for interop with other C2PA
//! tooling; the host reads it back as the segment's start time, and because
//! it's part of the signed claim every node sees the same value.

use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

use c2pa::{Builder, CallbackSigner, Signer as C2paSigner, SigningAlg};
use muxl::{Segmenter, SegmenterEvent};

use crate::cbor::SignedEvent;
use crate::error::{Error, Result};

/// Process-global c2pa-rs settings applied once before any sign call.
///
/// muxl-sign doesn't yet have a use case for X.509 trust verification —
/// our certs are issued via DID-based identity flows (Streamplace's
/// ES256K + did:key path), not chained to public CAs. So we disable
/// trust/OCSP/timestamp checks unconditionally. Callers that want
/// stricter settings can call `c2pa::settings::Settings::from_toml`
/// themselves before invoking any muxl-sign API — Once::call_once
/// ensures we won't stomp them.
const MUXL_SIGN_DEFAULTS_TOML: &str = r#"
version_major = 1
version_minor = 0

[verify]
verify_after_sign = false
verify_trust = false
verify_timestamp_trust = false
ocsp_fetch = false
remote_manifest_fetch = false
check_ingredient_trust = false
skip_ingredient_conflict_resolution = false
strict_v1_validation = false

[builder.thumbnail]
enabled = false
"#;

static SETTINGS_INIT: Once = Once::new();

pub(crate) fn init_default_settings() {
    SETTINGS_INIT.call_once(|| {
        c2pa::settings::Settings::from_toml(MUXL_SIGN_DEFAULTS_TOML)
            .expect("muxl-sign default settings TOML should always parse");
    });
}

/// Cert chain + signing backend used to drive c2pa-rs.
///
/// Two backends:
/// - [`SignBackend::Pem`] — sign in-process with a PEM-encoded private key.
///   Works in any target. Used by file-based CLI invocations and as the
///   default in WASM hosts that pass the streamer's key into the sandbox.
/// - [`SignBackend::Host`] — delegate signing to the embedding host via
///   the `streamplace.host_sign` wasm import. The private key never enters
///   the wasm sandbox; the host does the ECDSA work and returns raw r||s.
///   Required for hardware-backed signers (PKCS#11, TPM, EIP-712 wallets,
///   etc.) where the key is not extractable as PEM. Only usable when
///   compiled for `target_family = "wasm"`; native builds can construct
///   the variant but `build()` will error out.
///
/// An optional RFC 3161 TSA URL applies to either backend.
pub struct SignerKey {
    cert_chain: Vec<u8>,
    alg: SigningAlg,
    tsa_url: Option<String>,
    backend: SignBackend,
}

enum SignBackend {
    Pem(Vec<u8>),
    Host,
}

impl SignerKey {
    /// Build from in-memory PEM byte slices. The cert chain may be a
    /// concatenation of multiple PEM-encoded certs (leaf first).
    pub fn from_pem_bytes(
        cert_chain: impl Into<Vec<u8>>,
        private_key: impl Into<Vec<u8>>,
        alg: SigningAlg,
    ) -> Self {
        SignerKey {
            cert_chain: cert_chain.into(),
            alg,
            tsa_url: None,
            backend: SignBackend::Pem(private_key.into()),
        }
    }

    /// Read PEM cert chain and PEM private key from filesystem paths.
    pub fn from_pem_files(
        cert_path: impl AsRef<Path>,
        key_path: impl AsRef<Path>,
        alg: SigningAlg,
    ) -> Result<Self> {
        Ok(SignerKey {
            cert_chain: std::fs::read(cert_path)?,
            alg,
            tsa_url: None,
            backend: SignBackend::Pem(std::fs::read(key_path)?),
        })
    }

    /// Build a host-callback signer from in-memory cert chain bytes.
    ///
    /// The wasm runtime must supply a `streamplace.host_sign` import; see
    /// [`host_sign`]'s contract.
    pub fn host_from_pem_bytes(cert_chain: impl Into<Vec<u8>>, alg: SigningAlg) -> Self {
        SignerKey {
            cert_chain: cert_chain.into(),
            alg,
            tsa_url: None,
            backend: SignBackend::Host,
        }
    }

    /// Build a host-callback signer, reading the cert chain from a file.
    pub fn host_from_pem_file(cert_path: impl AsRef<Path>, alg: SigningAlg) -> Result<Self> {
        Ok(SignerKey {
            cert_chain: std::fs::read(cert_path)?,
            alg,
            tsa_url: None,
            backend: SignBackend::Host,
        })
    }

    /// Set the RFC 3161 timestamp authority URL. Defaults to `None`.
    pub fn with_tsa_url(mut self, tsa_url: impl Into<String>) -> Self {
        self.tsa_url = Some(tsa_url.into());
        self
    }

    fn build(&self) -> Result<Box<dyn C2paSigner>> {
        match (&self.backend, self.alg) {
            (SignBackend::Host, _) => self.build_host_callback(),
            // The streamplace c2pa-rs fork validates ES256K but doesn't sign
            // it via the rust_native_crypto path — wire the signer through
            // CallbackSigner using `k256` so signing works in WASM too.
            (SignBackend::Pem(_), SigningAlg::Es256K) => self.build_es256k_callback(),
            (SignBackend::Pem(key), _) => Ok(c2pa::create_signer::from_keys(
                &self.cert_chain,
                key,
                self.alg,
                self.tsa_url.clone(),
            )?),
        }
    }

    fn build_es256k_callback(&self) -> Result<Box<dyn C2paSigner>> {
        use k256::ecdsa::SigningKey;
        use k256::ecdsa::signature::Signer;
        use k256::pkcs8::DecodePrivateKey;

        let private_key = match &self.backend {
            SignBackend::Pem(k) => k,
            SignBackend::Host => unreachable!("build_es256k_callback called for host backend"),
        };
        let pem_str = std::str::from_utf8(private_key).map_err(|_| {
            Error::C2pa(c2pa::Error::BadParam(
                "private key is not UTF-8 PEM".into(),
            ))
        })?;
        let secret_key = k256::SecretKey::from_pkcs8_pem(pem_str)
            .map_err(|e| Error::C2pa(c2pa::Error::BadParam(format!("bad ES256K key PEM: {e}"))))?;
        let signing_key = SigningKey::from(&secret_key);

        let mut signer = CallbackSigner::new(
            move |_ctx, data: &[u8]| -> std::result::Result<Vec<u8>, c2pa::Error> {
                // k256's deterministic ECDSA hashes with SHA-256 internally
                // and returns a fixed-length 64-byte (R || S) signature —
                // exactly the P1363 format c2pa expects for ES256K.
                let sig: k256::ecdsa::Signature = signing_key.sign(data);
                Ok(sig.to_bytes().to_vec())
            },
            SigningAlg::Es256K,
            self.cert_chain.clone(),
        );
        if let Some(url) = &self.tsa_url {
            signer = signer.set_tsa_url(url.clone());
        }
        Ok(Box::new(signer))
    }

    fn build_host_callback(&self) -> Result<Box<dyn C2paSigner>> {
        let alg = self.alg;
        let mut signer = CallbackSigner::new(
            move |_ctx, data: &[u8]| -> std::result::Result<Vec<u8>, c2pa::Error> {
                host_sign_callback(data, alg).map_err(|e| c2pa::Error::BadParam(e.to_string()))
            },
            self.alg,
            self.cert_chain.clone(),
        );
        if let Some(url) = &self.tsa_url {
            signer = signer.set_tsa_url(url.clone());
        }
        Ok(Box::new(signer))
    }
}

/// Host-supplied signing import. The wasm runtime is expected to provide
/// this function under module name `muxl`; signs `data` and writes
/// the raw r||s (or DER, per `alg`) signature into `[out_sig_ptr,
/// out_sig_max)`. Returns the signature length, or `u32::MAX` on error.
///
/// Only linked in wasm targets — native builds get a stub that always
/// returns the error sentinel so [`SignBackend::Host`] simply doesn't
/// work outside of a wasm runtime that wires it up.
#[cfg(target_family = "wasm")]
#[link(wasm_import_module = "muxl")]
unsafe extern "C" {
    fn host_sign(data_ptr: u32, data_len: u32, out_sig_ptr: u32, out_sig_max: u32) -> u32;
    /// Compute SHA-256 of `data` host-side, write the 32-byte digest at
    /// `out_ptr`. Used by `bench-sha256` to measure whether moving SHA-256
    /// out of wasm shrinks p99 sign latency before committing to a
    /// crate-level sha2 patch. Always available host-side (PEM-mode
    /// invocations may still call into it).
    pub(crate) fn host_sha256(data_ptr: u32, data_len: u32, out_ptr: u32);
}

#[cfg(not(target_family = "wasm"))]
unsafe fn host_sign(_: u32, _: u32, _: u32, _: u32) -> u32 {
    u32::MAX
}

#[cfg(not(target_family = "wasm"))]
pub(crate) unsafe fn host_sha256(_: u32, _: u32, _: u32) {}

/// Pre-allocated buffer for the host's signature. Sized for the largest
/// algorithm we expect (PS512 over RSA-4096 ≈ 512 bytes); ECDSA r||s for
/// ES256/ES256K/ES384/ES512 fits comfortably under this.
const HOST_SIG_BUF_LEN: usize = 1024;

pub(crate) fn host_sign_callback(data: &[u8], _alg: SigningAlg) -> std::result::Result<Vec<u8>, String> {
    let mut buf = vec![0u8; HOST_SIG_BUF_LEN];
    let n = unsafe {
        host_sign(
            data.as_ptr() as u32,
            data.len() as u32,
            buf.as_mut_ptr() as u32,
            buf.len() as u32,
        )
    };
    if n == u32::MAX {
        return Err("host_sign rejected the request".into());
    }
    let n = n as usize;
    if n > buf.len() {
        return Err(format!("host_sign returned bogus length {n}"));
    }
    buf.truncate(n);
    Ok(buf)
}

/// Stream-sign an fMP4 source: consume `input` (an fMP4 byte stream from
/// e.g. `muxl segment`'s emitter) and emit one CBOR-framed
/// `signed-segment` event per GoP on `output`.
///
/// Each event's `data` field is a complete signed flat MP4 (wrapper +
/// per-track ingredients) — the artifact Streamplace stores per GoP.
///
/// Output framing is one DRISL/CBOR value per event, written
/// back-to-back. Decoders read one value at a time until EOF.
pub fn sign_segment_stream<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    signer: &SignerKey,
    segment_manifest: &str,
    wrapper_manifest: &str,
) -> Result<()> {
    init_default_settings();
    let c2pa_signer = signer.build()?;
    // Parse the base segment manifest once. Each GoP is signed with a clone
    // that carries this segment's signing time in cawg.metadata/`dc:date`
    // (see stamp_segment_manifest), so every canonical .m4s holds its own
    // provenance date — the value Streamplace reads back as the segment's
    // StartTime.
    let segment_base: serde_json::Value = serde_json::from_str(segment_manifest)
        .map_err(|e| Error::C2pa(c2pa::Error::BadParam(format!("segment manifest JSON: {e}"))))?;
    let mut segmenter = Segmenter::new();
    let mut init_seen = false;
    let mut buf = [0u8; 64 * 1024];

    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for event in segmenter.feed(&buf[..n])? {
            handle_event(
                event,
                &mut init_seen,
                signer,
                &segment_base,
                wrapper_manifest,
                output,
                &*c2pa_signer,
            )?;
        }
    }
    for event in segmenter.flush()? {
        handle_event(
            event,
            &mut init_seen,
            signer,
            &segment_base,
            wrapper_manifest,
            output,
            &*c2pa_signer,
        )?;
    }
    Ok(())
}

/// Stamp the per-segment signing time into the base segment manifest's
/// `cawg.metadata` assertion as `dc:date`, returning the manifest JSON to
/// sign for this GoP.
///
/// We keep the time inside `cawg.metadata`/`dc:date` (rather than a bespoke
/// assertion) for interop with other C2PA tooling. If the base manifest
/// already carries a `cawg.metadata` assertion (Streamplace's does, with
/// creator/title) its `dc:date` is overwritten; otherwise a minimal
/// `cawg.metadata` assertion carrying just the dc context + date is appended.
/// `when` is an RFC 3339 UTC string (`2019-09-22T18:22:57.000Z`).
fn stamp_segment_manifest(base: &serde_json::Value, when: &str) -> String {
    use serde_json::{Value, json};
    let mut m = base.clone();
    if let Some(arr) = m.get_mut("assertions").and_then(Value::as_array_mut) {
        if let Some(a) = arr
            .iter_mut()
            .find(|a| a.get("label").and_then(Value::as_str) == Some("cawg.metadata"))
        {
            if let Some(data) = a.get_mut("data").and_then(Value::as_object_mut) {
                data.insert("dc:date".into(), Value::String(when.into()));
            } else if let Some(obj) = a.as_object_mut() {
                obj.insert("data".into(), json!({ "dc:date": when }));
            }
        } else {
            arr.push(json!({
                "label": "cawg.metadata",
                "data": { "@context": { "dc": "http://purl.org/dc/elements/1.1/" }, "dc:date": when }
            }));
        }
    } else if let Some(obj) = m.as_object_mut() {
        obj.insert(
            "assertions".into(),
            json!([{
                "label": "cawg.metadata",
                "data": { "@context": { "dc": "http://purl.org/dc/elements/1.1/" }, "dc:date": when }
            }]),
        );
    }
    m.to_string()
}

/// Current wall-clock as an RFC 3339 UTC string with millisecond precision
/// (`2019-09-22T18:22:57.000Z`). Streamplace's aqtime parses it via
/// RFC3339Nano and normalizes to exactly this layout.
fn now_rfc3339_utc() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    rfc3339_from_unix(d.as_secs() as i64, d.subsec_millis())
}

/// Format `secs` since the Unix epoch (+ `millis`) as an RFC 3339 UTC string,
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`. Pure (no clock) so it's unit-testable.
fn rfc3339_from_unix(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// Civil (year, month, day) from days since 1970-01-01, via Howard Hinnant's
/// algorithm (proleptic Gregorian; handles leap years across the full range).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn handle_event<W: Write>(
    event: SegmenterEvent,
    init_seen: &mut bool,
    _signer: &SignerKey,
    segment_base: &serde_json::Value,
    _wrapper_manifest: &str,
    output: &mut W,
    c2pa_signer: &dyn C2paSigner,
) -> Result<()> {
    use muxl::cbor::{ByteString, CborEvent};
    match event {
        SegmenterEvent::InitSegment { catalog, data } => {
            *init_seen = true;
            // Build an Init event with the catalog + per-track init segments
            // so downstream consumers (Streamplace) have everything they need
            // to derive HLS playback artifacts without re-parsing.
            let track_inits: std::collections::BTreeMap<String, ByteString> =
                muxl::init::build_track_init_segments(&catalog)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(tid, bytes)| (tid.to_string(), ByteString(bytes)))
                    .collect();
            let event = SignedEvent::Init {
                data,
                catalog: Some(catalog),
                track_inits,
            };
            // Drop the auto-generated `Init` case from CborEvent — we re-emit
            // through SignedEvent ourselves so the wire type tag matches.
            let _ = CborEvent::from_event;
            dasl::drisl::to_writer(&mut *output, &event).map_err(|e| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                ))
            })?;
            output.flush()?;
        }
        SegmenterEvent::Segment(mut gop) => {
            if !*init_seen {
                return Err(Error::Muxl(muxl::Error::InvalidMp4(
                    "segment received before init segment".into(),
                )));
            }
            // Stamp this segment's signing time into cawg.metadata/`dc:date`
            // so the canonical .m4s carries its own provenance date (read back
            // as the segment StartTime), then sign each canonical segment (per
            // track) as a bare .m4s asset. Signing replaces the gop's per-track
            // buffers with the signed bytes and shifts per-sample offsets past
            // the leading c2pa-uuid prefix.
            let segment_manifest = stamp_segment_manifest(segment_base, &now_rfc3339_utc());
            let prefix_size = sign_gop_canonical_segments_in_place(
                &mut gop,
                &segment_manifest,
                c2pa_signer,
            )?;
            let number = gop.number;
            let track_count = gop.tracks.len();
            let signed_total: usize = gop.tracks.values().map(|v| v.len()).sum();

            let event = SignedEvent::signed_from_gop(gop);
            dasl::drisl::to_writer(&mut *output, &event).map_err(|e| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                ))
            })?;
            output.flush()?;
            eprintln!(
                "signed segment {number}: {track_count} canonical segments, \
                 {signed_total} bytes (c2pa prefix {prefix_size} bytes/track)"
            );
        }
    }
    Ok(())
}

/// Sign each per-track canonical segment in `gop` as a standalone .m4s
/// asset. Replaces `gop.tracks[tid]` with `[c2pa-uuid + muxl-uuid + frags]`
/// and shifts `gop.samples[tid].offsets_in_track` past the c2pa-uuid
/// prefix. `body_size` grows by `prefix_size * track_count`. Returns the
/// c2pa-uuid prefix size (asserted constant across all tracks).
fn sign_gop_canonical_segments_in_place(
    gop: &mut muxl::GopSegment,
    segment_manifest: &str,
    c2pa_signer: &dyn C2paSigner,
) -> Result<usize> {
    let mut prefix_size: Option<usize> = None;
    let mut signed_map: std::collections::BTreeMap<u32, Vec<u8>> =
        std::collections::BTreeMap::new();
    for (&tid, canonical) in &gop.tracks {
        let signed = sign_buf_as(canonical, segment_manifest, c2pa_signer, "m4s")?;
        let this_prefix = signed.len().checked_sub(canonical.len()).ok_or_else(|| {
            Error::Muxl(muxl::Error::InvalidMp4(
                "signed canonical segment shorter than unsigned".into(),
            ))
        })?;
        match prefix_size {
            None => prefix_size = Some(this_prefix),
            Some(p) if p == this_prefix => {}
            Some(p) => {
                return Err(Error::Muxl(muxl::Error::InvalidMp4(format!(
                    "per-segment c2pa-uuid size drift across tracks: \
                     expected {p}, got {this_prefix} for track {tid}"
                ))));
            }
        }
        signed_map.insert(tid, signed);
    }
    let prefix_size = prefix_size.unwrap_or(0);
    gop.tracks = signed_map;
    for samples in gop.samples.values_mut() {
        for off in &mut samples.offsets_in_track {
            *off += prefix_size as u64;
        }
    }
    gop.body_size += (prefix_size as u64) * (gop.tracks.len() as u64);
    Ok(prefix_size)
}

/// Sign a single in-memory MP4 buffer with a given manifest.
///
/// Helper for the per-track step. Wraps [`Builder::sign`] over
/// `Cursor`-backed buffers — c2pa-rs needs `Read+Seek` on input and
/// `Write+Read+Seek` on output, neither of which our caller's `&mut W:
/// Write` satisfies on its own.
fn sign_buf_as(
    input: &[u8],
    manifest: &str,
    signer: &dyn C2paSigner,
    format: &str,
) -> Result<Vec<u8>> {
    let mut builder = Builder::from_json(manifest)?;
    let mut source_cursor = Cursor::new(input);
    let mut output_buf: Vec<u8> = Vec::new();
    let mut dest_cursor = Cursor::new(&mut output_buf);
    builder.sign(signer, format, &mut source_cursor, &mut dest_cursor)?;
    Ok(output_buf)
}

/// Label binding the transcode source segment — added as a `parentOf`
/// ingredient by [`sign_transcode_segment`] — to the actions that reference
/// it. A manifest passed to [`sign_transcode_segment`] declares the link by
/// listing this string in an action's `org.cai.ingredientIds` parameter:
///
/// ```json
/// { "label": "c2pa.actions", "data": { "actions": [
///     { "action": "c2pa.opened",     "parameters": { "org.cai.ingredientIds": ["muxl.source"] } },
///     { "action": "c2pa.transcoded", "parameters": { "org.cai.ingredientIds": ["muxl.source"] } }
/// ]}}
/// ```
///
/// At sign time c2pa-rs resolves the label to the ingredient's hashed URI
/// (see [`Builder`]'s ingredient/action mapping), so the signed claim names
/// the exact source bytes the output was derived from.
pub const TRANSCODE_INGREDIENT_LABEL: &str = "muxl.source";

/// Sign one transcoded canonical MUXL segment, declaring the segment it was
/// transcoded from as a `parentOf` ingredient.
///
/// `output` is the unsigned canonical MUXL `.m4s` produced by transcoding +
/// re-segmenting; `source` is the canonical MUXL `.m4s` that was transcoded
/// (typically itself S2PA-signed, so the provenance chain stays unbroken).
/// `segment_manifest` is a C2PA manifest JSON that should carry a
/// `c2pa.actions` assertion whose `c2pa.transcoded` action references the
/// source via [`TRANSCODE_INGREDIENT_LABEL`] (see that constant's docs).
///
/// Returns the signed output — `[c2pa-uuid][muxl-uuid][moof][mdat]…` — which
/// verifies standalone as an `m4s` asset and whose manifest names `source`
/// as its parent. This is the provenance step a Livepeer orchestrator runs
/// after transcoding a signed MUXL segment: the output carries, in its own
/// signed claim, the identity of the exact input it came from.
///
/// Per-segment time is stamped into `cawg.metadata`/`dc:date` exactly as in
/// [`sign_segment_stream`], so each signed `.m4s` holds its own provenance
/// date.
pub fn sign_transcode_segment(
    output: &[u8],
    source: &[u8],
    signer: &SignerKey,
    segment_manifest: &str,
) -> Result<Vec<u8>> {
    init_default_settings();
    let c2pa_signer = signer.build()?;
    let base: serde_json::Value = serde_json::from_str(segment_manifest)
        .map_err(|e| Error::C2pa(c2pa::Error::BadParam(format!("segment manifest JSON: {e}"))))?;
    let manifest = stamp_segment_manifest(&base, &now_rfc3339_utc());
    sign_buf_as_transcode(output, source, &manifest, &*c2pa_signer, "m4s")
}

/// Sign `output` as a standalone asset that declares `source` as a
/// `parentOf` ingredient. Sibling of [`sign_buf_as`] with the ingredient
/// step added: c2pa-rs hashes `source` (and pulls in its manifest, if it's
/// signed) and records it as the parent, then the manifest's actions bind to
/// it via [`TRANSCODE_INGREDIENT_LABEL`].
fn sign_buf_as_transcode(
    output: &[u8],
    source: &[u8],
    manifest: &str,
    signer: &dyn C2paSigner,
    format: &str,
) -> Result<Vec<u8>> {
    let mut builder = Builder::from_json(manifest)?;

    // `source` is the parent ingredient: `output` is a transcode *of* it. The
    // label lets the manifest's actions reference this ingredient by id.
    let ingredient_json = format!(
        r#"{{"title":"source segment","relationship":"parentOf","label":"{TRANSCODE_INGREDIENT_LABEL}"}}"#
    );
    builder.add_ingredient_from_stream(ingredient_json, format, &mut Cursor::new(source))?;

    let mut asset = Cursor::new(output);
    let mut signed: Vec<u8> = Vec::new();
    builder.sign(signer, format, &mut asset, &mut Cursor::new(&mut signed))?;
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_from_unix(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_from_unix(0, 123), "1970-01-01T00:00:00.123Z");
        // Unix billennium.
        assert_eq!(rfc3339_from_unix(1_000_000_000, 0), "2001-09-09T01:46:40.000Z");
        // Leap day.
        assert_eq!(rfc3339_from_unix(1_582_934_400, 0), "2020-02-29T00:00:00.000Z");
    }

    #[test]
    fn stamp_mutates_existing_cawg_metadata() {
        let base = json!({
            "title": "stream",
            "assertions": [
                { "label": "c2pa.actions", "data": { "actions": [] } },
                { "label": "cawg.metadata", "data": {
                    "@context": { "dc": "http://purl.org/dc/elements/1.1/" },
                    "dc:creator": "did:example", "dc:title": "t",
                    "dc:date": "1970-01-01T00:00:00.000Z"
                }}
            ]
        });
        let out: serde_json::Value =
            serde_json::from_str(&stamp_segment_manifest(&base, "2020-02-29T00:00:00.000Z")).unwrap();
        let cawg: Vec<_> = out["assertions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["label"] == "cawg.metadata")
            .collect();
        assert_eq!(cawg.len(), 1, "must not duplicate cawg.metadata");
        assert_eq!(cawg[0]["data"]["dc:date"], "2020-02-29T00:00:00.000Z");
        // Creator/title preserved.
        assert_eq!(cawg[0]["data"]["dc:creator"], "did:example");
        assert_eq!(cawg[0]["data"]["dc:title"], "t");
    }

    #[test]
    fn stamp_appends_when_cawg_absent() {
        let base = json!({ "assertions": [ { "label": "c2pa.actions", "data": {} } ] });
        let out: serde_json::Value =
            serde_json::from_str(&stamp_segment_manifest(&base, "2001-09-09T01:46:40.000Z")).unwrap();
        let cawg = out["assertions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["label"] == "cawg.metadata")
            .expect("a cawg.metadata assertion should have been appended");
        assert_eq!(cawg["data"]["dc:date"], "2001-09-09T01:46:40.000Z");
        assert!(cawg["data"]["@context"]["dc"].is_string());
    }

    #[test]
    fn stamp_creates_assertions_when_missing() {
        let base = json!({ "title": "x" });
        let out: serde_json::Value =
            serde_json::from_str(&stamp_segment_manifest(&base, "1970-01-01T00:00:00.000Z")).unwrap();
        assert_eq!(out["assertions"].as_array().unwrap().len(), 1);
        assert_eq!(out["assertions"][0]["label"], "cawg.metadata");
    }
}
