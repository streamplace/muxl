//! Per-track signing + ingredient combine.
//!
//! Splits a multi-track [`muxl::Source`] into per-track flat MP4s, signs
//! each one independently with c2pa-rs, and combines the signed per-track
//! assets as `Ingredient`s in a wrapper signed flat MP4. The result is a
//! multi-track flat MP4 whose top-level manifest covers cross-track
//! claims and whose per-track ingredient manifests verify each track in
//! isolation — losing a track only invalidates that ingredient, not the
//! wrapper signature or the surviving tracks.

use std::io::{Cursor, Write};
use std::path::Path;

use c2pa::{Builder, CallbackSigner, Signer as C2paSigner, SigningAlg};
use muxl::Source;
use muxl::io::ReadAt;

use crate::error::{Error, Result};

/// PEM-format cert chain + private key bundle, used to drive c2pa-rs.
///
/// Holds the bytes of the (possibly multi-cert) PEM signing chain and the
/// matching private key, plus the chosen [`SigningAlg`] (typically
/// `Es256K` for Streamplace's ES256K + DID issuance flow). An optional
/// timestamp authority URL can be set for RFC 3161 timestamps; leave it
/// unset for tests.
pub struct SignerKey {
    cert_chain: Vec<u8>,
    private_key: Vec<u8>,
    alg: SigningAlg,
    tsa_url: Option<String>,
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
            private_key: private_key.into(),
            alg,
            tsa_url: None,
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
            private_key: std::fs::read(key_path)?,
            alg,
            tsa_url: None,
        })
    }

    /// Set the RFC 3161 timestamp authority URL. Defaults to `None`.
    pub fn with_tsa_url(mut self, tsa_url: impl Into<String>) -> Self {
        self.tsa_url = Some(tsa_url.into());
        self
    }

    fn build(&self) -> Result<Box<dyn C2paSigner>> {
        match self.alg {
            // The streamplace c2pa-rs fork validates ES256K but doesn't sign
            // it via the rust_native_crypto path — wire the signer through
            // CallbackSigner using `k256` so signing works in WASM too.
            SigningAlg::Es256K => self.build_es256k_callback(),
            _ => Ok(c2pa::create_signer::from_keys(
                &self.cert_chain,
                &self.private_key,
                self.alg,
                self.tsa_url.clone(),
            )?),
        }
    }

    fn build_es256k_callback(&self) -> Result<Box<dyn C2paSigner>> {
        use k256::ecdsa::SigningKey;
        use k256::ecdsa::signature::Signer;
        use k256::pkcs8::DecodePrivateKey;

        let pem_str = std::str::from_utf8(&self.private_key).map_err(|_| {
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
}

/// Sign a multi-track [`Source`] per-track and combine.
///
/// Steps, in order:
/// 1. For each track in `source.plan.tracks`, write a single-track flat MP4
///    (via [`Source::filter_to_track`] + [`muxl::flat::write`]) and sign it
///    with `track_manifest` to produce a per-track signed asset.
/// 2. Write the multi-track flat MP4 of the original source.
/// 3. Build a wrapper [`Builder`] from `wrapper_manifest`, attach each
///    per-track signed asset as a c2pa `Ingredient`, sign, and write the
///    wrapper bytes to `output`.
///
/// The same `track_manifest` JSON is used for every track in v1; per-track
/// templating can come later if needed.
pub fn sign_per_track<R, W>(
    source: &Source,
    input: &R,
    signer: &SignerKey,
    track_manifest: &str,
    wrapper_manifest: &str,
    output: &mut W,
) -> Result<()>
where
    R: ReadAt + ?Sized,
    W: Write,
{
    let c2pa_signer = signer.build()?;

    // 1. Per-track sign — emit + sign one flat MP4 per track.
    let mut signed_tracks: Vec<(u32, Vec<u8>)> = Vec::with_capacity(source.plan.tracks.len());
    for track in &source.plan.tracks {
        let single = source.filter_to_track(track.track_id).ok_or_else(|| {
            Error::Muxl(muxl::Error::InvalidMp4(format!(
                "track {} disappeared during filter",
                track.track_id
            )))
        })?;
        let mut track_buf = Vec::new();
        muxl::flat::write(&single, input, &mut track_buf)?;
        let signed = sign_buf(&track_buf, track_manifest, &*c2pa_signer)?;
        signed_tracks.push((track.track_id, signed));
    }

    // 2. Wrapper flat MP4 — covers all tracks together.
    let mut wrapper_buf = Vec::new();
    muxl::flat::write(source, input, &mut wrapper_buf)?;

    // 3. Wrapper sign with per-track signed assets as ingredients.
    let mut builder = Builder::from_json(wrapper_manifest)?;
    for (track_id, signed_bytes) in &signed_tracks {
        let ingredient_json = format!(
            r#"{{"title": "track-{}", "relationship": "componentOf"}}"#,
            track_id
        );
        let mut ingredient_cursor = Cursor::new(signed_bytes.as_slice());
        builder.add_ingredient_from_stream(ingredient_json, "video/mp4", &mut ingredient_cursor)?;
    }

    let mut source_cursor = Cursor::new(wrapper_buf);
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

/// Sign a single in-memory MP4 buffer with a given manifest.
///
/// Helper for the per-track step. Wraps [`Builder::sign`] over
/// `Cursor`-backed buffers — c2pa-rs needs `Read+Seek` on input and
/// `Write+Read+Seek` on output, neither of which our caller's `&mut W:
/// Write` satisfies on its own.
fn sign_buf(input: &[u8], manifest: &str, signer: &dyn C2paSigner) -> Result<Vec<u8>> {
    let mut builder = Builder::from_json(manifest)?;
    let mut source_cursor = Cursor::new(input);
    let mut output_buf: Vec<u8> = Vec::new();
    let mut dest_cursor = Cursor::new(&mut output_buf);
    builder.sign(signer, "video/mp4", &mut source_cursor, &mut dest_cursor)?;
    Ok(output_buf)
}
