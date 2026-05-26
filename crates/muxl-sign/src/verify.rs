//! Verify signed canonical MUXL segments with c2pa-rs, in-process.
//!
//! Streamplace previously verified provenance by calling the
//! `iroh-streamplace` uniffi binding's `get_manifest_and_cert` over a flat
//! signed MP4. With the canonical `.m4s` segment as the stored and
//! transmitted unit, verification moves here: [`unwrap`] the wrapper into
//! its per-track signed segments and run c2pa-rs's [`Reader`] over each one
//! as an `m4s` asset — the exact format it was signed as
//! (`sign::sign_buf_as(.., "m4s")`). A single `muxl-sign.wasm` now covers
//! segment / sign / wrap / verify, so the c2pa dependency leaves the host.
//!
//! [`unwrap`]: muxl::reader::unwrap
//! [`Reader`]: c2pa::Reader

use std::io::Cursor;

use c2pa::Reader;
use serde_json::json;
use x509_cert::Certificate;

use crate::error::{Error, Result};
use crate::sign::init_default_settings;

/// Verify every canonical segment in a signed MUXL wrapper. `bytes` may be a
/// bare `.m4s` stream, a MUXL fMP4, or a flat MP4 — [`muxl::reader::unwrap`]
/// fast-forwards over the framing and splits on segment boundaries, keeping
/// each segment's leading c2pa-uuid box attached.
///
/// Returns a JSON document, one entry per track, each mirroring the old
/// `get_manifest_and_cert` shape so the host's assertion- and cert-parsing
/// applies unchanged:
///
/// ```json
/// {"segments": [
///   {"track_id": 1,
///    "manifest": { .. }, "cert": "<pem chain>",
///    "validation_results": { .. }, "validation_state": ".."},
///   ..
/// ]}
/// ```
///
/// Cryptographic *failures* (a tampered segment) surface in each entry's
/// `validation_results` / `validation_state` for the caller to enforce —
/// matching how the host already inspects `validation_results.failure`. This
/// function errors only on structural problems: nothing to unwrap, an
/// unreadable segment, or a segment carrying no signed manifest.
pub fn verify_segments(bytes: &[u8]) -> Result<String> {
    init_default_settings();
    let segments = muxl::reader::unwrap(bytes)?;
    if segments.is_empty() {
        return Err(Error::Muxl(muxl::Error::InvalidMp4(
            "no canonical segments to verify".into(),
        )));
    }

    let mut out = Vec::with_capacity(segments.len());
    for seg in &segments {
        // Each segment was signed standalone as an "m4s" asset, so it
        // verifies standalone — no synthesized init/moov is involved (the
        // hash covers only s2pa(muxl(data)), never a header).
        let reader = Reader::from_stream("m4s", Cursor::new(seg.data))?;
        let manifest = reader.active_manifest().ok_or_else(|| {
            Error::Muxl(muxl::Error::InvalidMp4(format!(
                "canonical segment for track {} has no active c2pa manifest",
                seg.track_id
            )))
        })?;
        let cert_chain = manifest
            .signature_info()
            .map(|si| si.cert_chain().to_string())
            .ok_or_else(|| {
                Error::Muxl(muxl::Error::InvalidMp4(format!(
                    "track {} manifest carries no signature info",
                    seg.track_id
                )))
            })?;
        out.push(json!({
            "track_id": seg.track_id,
            "manifest": manifest,
            "cert": cert_chain,
            "signer_pubkey": leaf_pubkey_hex(&cert_chain),
            "validation_results": reader.validation_results(),
            "validation_state": reader.validation_state(),
        }));
    }

    Ok(json!({ "segments": out }).to_string())
}

/// Extract the leaf cert's public key (uncompressed SEC1, lowercase hex) from a
/// PEM cert chain. This lets a host bind the S2PA signer to an external
/// identity — e.g. recover a Livepeer orchestrator's Ethereum address from the
/// secp256k1 key, confirming the signer is the orchestrator it selected.
/// Returns `None` if the chain doesn't parse.
fn leaf_pubkey_hex(cert_chain_pem: &str) -> Option<String> {
    // load_pem_chain tolerates one-or-many concatenated PEM certs (and
    // trailing whitespace), unlike the strict single-doc from_pem.
    let chain = Certificate::load_pem_chain(cert_chain_pem.as_bytes()).ok()?;
    let leaf = chain.first()?;
    let bytes = leaf
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes();
    Some(hex_lower(bytes))
}

fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}
