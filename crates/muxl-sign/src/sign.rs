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

use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::Once;

use c2pa::{Builder, CallbackSigner, Signer as C2paSigner, SigningAlg};
use muxl::{Segmenter, SegmenterEvent, Source};
use muxl::io::ReadAt;

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

fn init_default_settings() {
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

fn host_sign_callback(data: &[u8], _alg: SigningAlg) -> std::result::Result<Vec<u8>, String> {
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

/// Sign a multi-track [`Source`] by signing each canonical segment as a
/// standalone .m4s asset, then signing the assembled flat MP4 wrapper.
///
/// Output shape: `ftyp + c2pa-uuid (wrapper) + moov + mdat-envelope {
/// [c2pa-uuid (segment) + muxl-uuid + moof+mdat]* }`.
///
/// `segment_manifest` is applied to every canonical segment.
/// `wrapper_manifest` is applied to the assembled flat MP4 at the file
/// level. Both manifests use the same signer.
pub fn sign_per_track<R, W>(
    source: &Source,
    input: &R,
    signer: &SignerKey,
    segment_manifest: &str,
    wrapper_manifest: &str,
    output: &mut W,
) -> Result<()>
where
    R: ReadAt + ?Sized,
    W: Write,
{
    init_default_settings();
    let c2pa_signer = signer.build()?;

    // 1. Build the unsigned canonical flat MP4. Body bytes are
    //    `[muxl-uuid + moof+mdat]*` per canonical segment in time-slice
    //    order, with co64 entries in moov pointing at sample payload
    //    inside each fragment's inner mdat.
    let mut unsigned: Vec<u8> = Vec::new();
    muxl::flat::write(source, input, &mut unsigned)?;

    // 2. Sign each canonical segment in the body as .m4s. Returns the
    //    assembled flat MP4 with signed segments and shifted co64 offsets,
    //    but no wrapper-level c2pa-uuid yet.
    let with_signed_segments = sign_canonical_segments_in_body(
        &unsigned,
        segment_manifest,
        &*c2pa_signer,
    )?;

    // 3. Sign the wrapper at the file level. c2pa-rs inserts its uuid box
    //    after ftyp; the default BMFF v3 hash assertion excludes
    //    /uuid(c2pa) + /ftyp + /mfra and covers the rest (moov +
    //    mdat-envelope, including the per-segment c2pa-uuids inside as
    //    inert bytes — segment signatures stand on their own).
    let mut builder = Builder::from_json(wrapper_manifest)?;
    let mut source_cursor = Cursor::new(with_signed_segments);
    let mut output_buf: Vec<u8> = Vec::new();
    let mut dest_cursor = Cursor::new(&mut output_buf);
    builder.sign(
        &*c2pa_signer,
        "video/mp4",
        &mut source_cursor,
        &mut dest_cursor,
    )?;

    output.write_all(&output_buf)?;
    Ok(())
}

/// Layout of a parsed top-level flat MP4 (ftyp + moov + mdat-envelope).
struct FlatMp4Layout {
    ftyp_end: usize,
    moov_start: usize,
    moov_end: usize,
    mdat_envelope_start: usize,
    /// `mdat_envelope_start + 16` (we always use the 64-bit largesize form).
    mdat_payload_start: usize,
    mdat_payload_end: usize,
}

/// Walk top-level boxes and identify ftyp, moov, and mdat envelope spans.
fn parse_flat_mp4_layout(data: &[u8]) -> Result<FlatMp4Layout> {
    let mut off = 0usize;
    let mut ftyp_end: Option<usize> = None;
    let mut moov_range: Option<(usize, usize)> = None;
    let mut mdat: Option<(usize, usize, usize)> = None; // (env_start, payload_start, env_end)

    while off + 8 <= data.len() {
        let size_field =
            u32::from_be_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        let kind = &data[off + 4..off + 8];
        let (box_size, header_size) = if size_field == 1 {
            if off + 16 > data.len() {
                return Err(Error::Muxl(muxl::Error::InvalidMp4(
                    "truncated largesize box header".into(),
                )));
            }
            let largesize = u64::from_be_bytes(
                data[off + 8..off + 16].try_into().unwrap(),
            ) as usize;
            (largesize, 16)
        } else if size_field == 0 {
            // size=0 means "to end of file"; treat as remainder.
            (data.len() - off, 8)
        } else {
            (size_field, 8)
        };
        if off + box_size > data.len() {
            return Err(Error::Muxl(muxl::Error::InvalidMp4(format!(
                "box {:?} extends past asset",
                std::str::from_utf8(kind).unwrap_or("?")
            ))));
        }
        match kind {
            b"ftyp" => ftyp_end = Some(off + box_size),
            b"moov" => moov_range = Some((off, off + box_size)),
            b"mdat" => mdat = Some((off, off + header_size, off + box_size)),
            _ => {}
        }
        off += box_size;
    }

    let ftyp_end = ftyp_end.ok_or_else(|| {
        Error::Muxl(muxl::Error::InvalidMp4("no ftyp in flat MP4".into()))
    })?;
    let (moov_start, moov_end) = moov_range.ok_or_else(|| {
        Error::Muxl(muxl::Error::InvalidMp4("no moov in flat MP4".into()))
    })?;
    let (mdat_envelope_start, mdat_payload_start, mdat_payload_end) =
        mdat.ok_or_else(|| {
            Error::Muxl(muxl::Error::InvalidMp4("no mdat in flat MP4".into()))
        })?;
    Ok(FlatMp4Layout {
        ftyp_end,
        moov_start,
        moov_end,
        mdat_envelope_start,
        mdat_payload_start,
        mdat_payload_end,
    })
}

/// Walk top-level boxes inside the mdat envelope payload and return the
/// byte ranges of each canonical segment. A canonical segment runs from a
/// muxl-uuid box (identified by [`muxl::segment::MUXL_UUID`]) to the start
/// of the next muxl-uuid (or end of payload).
fn locate_muxl_canonical_segments(
    mdat_payload: &[u8],
) -> Result<Vec<std::ops::Range<usize>>> {
    let mut chunks: Vec<std::ops::Range<usize>> = Vec::new();
    let mut current_start: Option<usize> = None;
    let mut off = 0usize;

    while off + 8 <= mdat_payload.len() {
        let size_field =
            u32::from_be_bytes(mdat_payload[off..off + 4].try_into().unwrap())
                as usize;
        let kind = &mdat_payload[off + 4..off + 8];
        let (box_size, header_size) = if size_field == 1 {
            if off + 16 > mdat_payload.len() {
                return Err(Error::Muxl(muxl::Error::InvalidMp4(
                    "truncated largesize box header inside mdat".into(),
                )));
            }
            let largesize = u64::from_be_bytes(
                mdat_payload[off + 8..off + 16].try_into().unwrap(),
            ) as usize;
            (largesize, 16)
        } else if size_field == 0 {
            (mdat_payload.len() - off, 8)
        } else {
            (size_field, 8)
        };
        if off + box_size > mdat_payload.len() {
            return Err(Error::Muxl(muxl::Error::InvalidMp4(format!(
                "box inside mdat extends past payload (off={off}, size={box_size})"
            ))));
        }
        if kind == b"uuid"
            && box_size >= header_size + 16
            && mdat_payload[off + header_size..off + header_size + 16]
                == muxl::segment::MUXL_UUID
        {
            if let Some(start) = current_start {
                chunks.push(start..off);
            }
            current_start = Some(off);
        }
        off += box_size;
    }
    if let Some(start) = current_start {
        chunks.push(start..mdat_payload.len());
    }
    Ok(chunks)
}

/// Sign each canonical segment in `unsigned`'s mdat envelope as .m4s and
/// return the reassembled flat MP4 with shifted co64 entries. The output
/// has no wrapper-level c2pa-uuid (caller wraps separately).
fn sign_canonical_segments_in_body(
    unsigned: &[u8],
    segment_manifest: &str,
    c2pa_signer: &dyn C2paSigner,
) -> Result<Vec<u8>> {
    use mp4_atom::{Header, Moov, ReadAtom, ReadFrom, WriteTo};

    let layout = parse_flat_mp4_layout(unsigned)?;
    let mdat_payload = &unsigned[layout.mdat_payload_start..layout.mdat_payload_end];
    let chunks = locate_muxl_canonical_segments(mdat_payload)?;
    if chunks.is_empty() {
        return Err(Error::Muxl(muxl::Error::InvalidMp4(
            "no canonical segments found in mdat envelope".into(),
        )));
    }

    // Sign each canonical segment as .m4s.
    let mut signed_chunks: Vec<Vec<u8>> = Vec::with_capacity(chunks.len());
    let mut prefix_size: Option<usize> = None;
    for chunk_range in &chunks {
        let chunk_bytes = &mdat_payload[chunk_range.clone()];
        let signed = sign_buf_as(chunk_bytes, segment_manifest, c2pa_signer, "m4s")?;
        let this_prefix = signed
            .len()
            .checked_sub(chunk_bytes.len())
            .ok_or_else(|| {
                Error::Muxl(muxl::Error::InvalidMp4(
                    "signed canonical segment shorter than unsigned".into(),
                ))
            })?;
        match prefix_size {
            None => prefix_size = Some(this_prefix),
            Some(p) if p == this_prefix => {}
            Some(p) => {
                return Err(Error::Muxl(muxl::Error::InvalidMp4(format!(
                    "per-segment c2pa-uuid size drift: expected {p}, got {this_prefix}"
                ))));
            }
        }
        signed_chunks.push(signed);
    }
    let prefix_size = prefix_size.unwrap_or(0);

    // Shift co64 entries in moov to account for the c2pa-uuid prefix
    // prepended to each canonical segment. For each entry at byte offset
    // O_unsigned in the unsigned file, the new offset is
    // O_unsigned + chunks_before(O_unsigned) * prefix_size, where
    // chunks_before(O) counts canonical-segment chunks whose start (in
    // unsigned file coordinates) is <= O.
    let chunk_starts_unsigned: Vec<u64> = chunks
        .iter()
        .map(|r| (layout.mdat_payload_start + r.start) as u64)
        .collect();
    let entry_shift_for = |off: u64| -> u64 {
        let chunks_before =
            chunk_starts_unsigned.iter().filter(|&&s| s <= off).count() as u64;
        chunks_before * prefix_size as u64
    };

    let moov_bytes = &unsigned[layout.moov_start..layout.moov_end];
    let mut cursor = Cursor::new(moov_bytes);
    let header = <Option<Header> as ReadFrom>::read_from(&mut cursor)
        .map_err(|e| Error::Muxl(muxl::Error::InvalidMp4(e.to_string())))?
        .ok_or_else(|| {
            Error::Muxl(muxl::Error::InvalidMp4("empty moov header".into()))
        })?;
    let mut moov = Moov::read_atom(&header, &mut cursor)
        .map_err(|e| Error::Muxl(muxl::Error::InvalidMp4(e.to_string())))?;

    for trak in &mut moov.trak {
        if let Some(co64) = &mut trak.mdia.minf.stbl.co64 {
            for entry in &mut co64.entries {
                *entry += entry_shift_for(*entry);
            }
        }
    }

    let mut new_moov_buf: Vec<u8> = Vec::new();
    moov.write_to(&mut new_moov_buf)
        .map_err(|e| Error::Muxl(muxl::Error::InvalidMp4(e.to_string())))?;
    if new_moov_buf.len() != moov_bytes.len() {
        return Err(Error::Muxl(muxl::Error::InvalidMp4(format!(
            "moov size changed during co64 shift: {} -> {}",
            moov_bytes.len(),
            new_moov_buf.len()
        ))));
    }

    // Assemble: ftyp + new_moov + new mdat envelope { signed canonical segments }.
    let new_mdat_payload_size: u64 =
        signed_chunks.iter().map(|s| s.len() as u64).sum();
    let new_largesize = 16 + new_mdat_payload_size;
    let total_capacity = layout.ftyp_end
        + new_moov_buf.len()
        + 16
        + new_mdat_payload_size as usize;

    let mut out: Vec<u8> = Vec::with_capacity(total_capacity);
    out.extend_from_slice(&unsigned[..layout.ftyp_end]);
    out.extend_from_slice(&new_moov_buf);
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&new_largesize.to_be_bytes());
    for signed in &signed_chunks {
        out.extend_from_slice(signed);
    }
    let _ = layout.mdat_envelope_start; // (kept in struct for documentation)
    Ok(out)
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
                segment_manifest,
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
            segment_manifest,
            wrapper_manifest,
            output,
            &*c2pa_signer,
        )?;
    }
    Ok(())
}

fn handle_event<W: Write>(
    event: SegmenterEvent,
    init_seen: &mut bool,
    _signer: &SignerKey,
    segment_manifest: &str,
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
            // Sign each canonical segment (per track) in this GoP as a bare
            // .m4s asset. Replace the gop's per-track buffers with the
            // signed bytes and shift per-sample offsets past the leading
            // c2pa-uuid prefix.
            let prefix_size = sign_gop_canonical_segments_in_place(
                &mut gop,
                segment_manifest,
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
