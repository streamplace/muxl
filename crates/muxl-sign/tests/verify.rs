//! End-to-end test for the `verify` path: sign an fMP4 fixture through the
//! streaming signer, reassemble the per-track signed buffers into a bare
//! `.m4s` stream (what Streamplace stores/transmits), and confirm
//! `verify_segments` unwraps + validates each one and emits the per-track
//! manifest+cert JSON the host consumes.

use std::io::Cursor;
use std::path::PathBuf;

use muxl_sign::{SignerKey, SigningAlg, sign_segment_stream, verify_segments};
use serde_cbor::Value;

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

const SEGMENT_MANIFEST: &str = r#"{
    "title": "muxl-sign verify segment",
    "assertions": [
        { "label": "c2pa.actions",
          "data": { "actions": [{ "action": "c2pa.created" }] } }
    ]
}"#;

const WRAPPER_MANIFEST: &str = r#"{
    "title": "muxl-sign verify wrapper",
    "assertions": [
        { "label": "c2pa.actions",
          "data": { "actions": [{ "action": "c2pa.created" }] } }
    ]
}"#;

fn cbor_stream(bytes: &[u8]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut cursor = Cursor::new(bytes);
    loop {
        let mut de = serde_cbor::Deserializer::from_reader(&mut cursor);
        match serde::de::Deserialize::deserialize(&mut de) {
            Ok(v) => out.push(v),
            Err(_) => break,
        }
    }
    out
}

fn event_type(v: &Value) -> Option<&str> {
    if let Value::Map(m) = v {
        for (k, val) in m {
            if let (Value::Text(k), Value::Text(s)) = (k, val) {
                if k == "type" {
                    return Some(s.as_str());
                }
            }
        }
    }
    None
}

fn get_text_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Map(m) = v {
        for (k, val) in m {
            if let Value::Text(k) = k {
                if k == key {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// Reassemble the per-track signed buffers from a signed-segment event
/// stream into one bare `.m4s` byte stream (concatenated canonical
/// segments) — the storage/wire form Streamplace feeds back to `verify`.
fn signed_m4s_stream(fixture: &str) -> Vec<u8> {
    let fmp4 = std::fs::read(repo_path(fixture)).expect("read fmp4 fixture");
    let signer = SignerKey::from_pem_files(
        repo_path("samples/test-keys/es256k-cert.pem"),
        repo_path("samples/test-keys/es256k-key.pem"),
        SigningAlg::Es256K,
    )
    .expect("load signer");

    let mut output: Vec<u8> = Vec::new();
    sign_segment_stream(
        &mut Cursor::new(&fmp4),
        &mut output,
        &signer,
        SEGMENT_MANIFEST,
        WRAPPER_MANIFEST,
    )
    .expect("sign_segment_stream");

    let mut m4s = Vec::new();
    for event in cbor_stream(&output)
        .iter()
        .filter(|e| event_type(e) == Some("signed-segment"))
    {
        if let Some(Value::Map(tracks)) = get_text_key(event, "tracks") {
            for (_tid, buf) in tracks {
                if let Value::Bytes(b) = buf {
                    m4s.extend_from_slice(b);
                }
            }
        }
    }
    assert!(!m4s.is_empty(), "no signed track bytes recovered");
    m4s
}

#[test]
fn verify_validates_unwrapped_segments() {
    let m4s = signed_m4s_stream("samples/fixtures/h264-opus-frag.mp4");

    let json = verify_segments(&m4s).expect("verify_segments");
    let v: serde_json::Value = serde_json::from_str(&json).expect("verify output is JSON");

    let segments = v["segments"].as_array().expect("segments array present");
    assert!(
        segments.len() >= 2,
        "expected at least video+audio segments, got {}",
        segments.len()
    );

    for (i, seg) in segments.iter().enumerate() {
        assert!(
            seg["track_id"].is_number(),
            "segment {i} carries a track_id"
        );
        assert!(
            seg["manifest"].is_object(),
            "segment {i} carries an active manifest object"
        );
        let cert = seg["cert"].as_str().unwrap_or("");
        assert!(
            cert.contains("BEGIN CERTIFICATE"),
            "segment {i} carries a PEM cert chain, got {cert:?}"
        );
        let pubkey = seg["signer_pubkey"].as_str().unwrap_or("");
        assert_eq!(
            pubkey.len(),
            130,
            "segment {i} signer_pubkey is 65-byte uncompressed SEC1 hex, got {pubkey:?}"
        );
        assert!(
            pubkey.starts_with("04"),
            "segment {i} signer_pubkey is uncompressed (0x04 prefix), got {pubkey:?}"
        );
        assert_ne!(
            seg["validation_state"].as_str(),
            Some("Invalid"),
            "segment {i} should validate (state was {:?})",
            seg["validation_state"]
        );
    }
}

#[test]
fn verify_rejects_garbage() {
    assert!(
        verify_segments(b"not an mp4 at all").is_err(),
        "verify must reject input with no canonical segments"
    );
}
