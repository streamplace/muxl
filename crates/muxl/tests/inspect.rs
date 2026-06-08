//! End-to-end test for `muxl inspect`: sign an fMP4 fixture through the
//! streaming signer, reassemble the per-track signed buffers into a bare
//! `.m4s` stream (what Streamplace stores/transmits), then confirm both the
//! `--json` report and the `--manifests` dump surface the per-segment codec
//! and signing facts.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use muxl_sign::inspect::{InspectOptions, inspect_to};
use muxl_sign::{SignerKey, SigningAlg, sign_segment_stream};
use serde_cbor::Value;

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

const SEGMENT_MANIFEST: &str = r#"{
    "title": "muxl inspect segment",
    "assertions": [
        { "label": "c2pa.actions",
          "data": { "actions": [{ "action": "c2pa.created" }] } }
    ]
}"#;

const WRAPPER_MANIFEST: &str = r#"{
    "title": "muxl inspect wrapper",
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
    let Value::Map(m) = v else { return None };
    for (k, val) in m {
        if let (Value::Text(k), Value::Text(s)) = (k, val) {
            if k == "type" {
                return Some(s.as_str());
            }
        }
    }
    None
}

fn get_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Map(m) = v else { return None };
    for (k, val) in m {
        if let Value::Text(k) = k {
            if k == key {
                return Some(val);
            }
        }
    }
    None
}

/// Reassemble the per-track signed buffers from a signed-segment event stream
/// into one bare `.m4s` byte stream — the storage/wire form Streamplace
/// inspects.
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
        if let Some(Value::Map(tracks)) = get_key(event, "tracks") {
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

fn inspect_json(bytes: &[u8], opts: InspectOptions) -> serde_json::Value {
    let mut out = Vec::new();
    inspect_to(bytes, Path::new("signed.m4s"), &mut out, false, opts).expect("inspect_to");
    serde_json::from_slice(&out).expect("inspect --json emits valid JSON")
}

#[test]
fn inspect_json_reports_codec_and_signing() {
    let m4s = signed_m4s_stream("samples/fixtures/h264-opus-frag.mp4");
    let v = inspect_json(&m4s, InspectOptions { json: true, manifests: false });

    let segments = v["segments"].as_array().expect("segments array");
    assert!(
        segments.len() >= 2,
        "expected at least video+audio segments, got {}",
        segments.len()
    );
    assert_eq!(v["segment_count"].as_u64(), Some(segments.len() as u64));

    // At least one segment carries the H.264 video config (with its friendly
    // label), and every segment is signed (not "unsigned").
    let has_h264 = segments.iter().any(|s| {
        s["video"].as_array().is_some_and(|vs| {
            vs.iter().any(|c| {
                c["codec"].as_str().is_some_and(|x| x.starts_with("avc1."))
                    && c["friendly"].as_str() == Some("H.264 / AVC")
            })
        })
    });
    assert!(has_h264, "video segment reports its avc1 codec + friendly label");

    for (i, seg) in segments.iter().enumerate() {
        let sig = &seg["signing"];
        let state = sig["state"].as_str().unwrap_or("");
        assert!(
            matches!(state, "trusted" | "valid"),
            "segment {i} should be signed + validate, got state {state:?}"
        );
        let pubkey = sig["pubkey"].as_str().unwrap_or("");
        assert_eq!(pubkey.len(), 130, "segment {i} pubkey is full 65-byte SEC1 hex");
        assert!(pubkey.starts_with("04"), "segment {i} pubkey is uncompressed");
        // The signer line is the leaf cert's CN (S2PA puts the signer DID here;
        // the test cert uses a plain CN).
        assert_eq!(sig["signer"].as_str(), Some("muxl test signer"));
        let actions: Vec<&str> = sig["actions"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        assert!(
            actions.contains(&"c2pa.created"),
            "segment {i} surfaces its c2pa.created action, got {actions:?}"
        );
        // Without --manifests, the full store is omitted.
        assert!(seg.get("manifests").is_none(), "segment {i} omits the store");
    }
}

#[test]
fn inspect_manifests_embeds_the_store_in_json() {
    let m4s = signed_m4s_stream("samples/fixtures/h264-opus-frag.mp4");
    let v = inspect_json(&m4s, InspectOptions { json: true, manifests: true });

    for (i, seg) in v["segments"].as_array().unwrap().iter().enumerate() {
        let store = &seg["manifests"];
        assert!(
            store.is_object(),
            "segment {i} embeds its C2PA manifest store with --manifests"
        );
        // The c2pa Reader JSON always carries a manifests map.
        assert!(
            store.get("manifests").is_some_and(|m| m.is_object()),
            "segment {i} store has a manifests map, got {store}"
        );
    }
}

#[test]
fn inspect_human_renders_signed_block_and_manifest_dump() {
    let m4s = signed_m4s_stream("samples/fixtures/h264-opus-frag.mp4");
    let mut out = Vec::new();
    inspect_to(
        &m4s,
        Path::new("signed.m4s"),
        &mut out,
        false,
        InspectOptions { json: false, manifests: true },
    )
    .expect("inspect_to human");
    let text = String::from_utf8(out).expect("human output is utf-8");

    assert!(text.contains("── segment 1/"), "prints per-segment headers");
    assert!(text.contains("video:"), "prints the codec block");
    assert!(text.contains("✓"), "prints a validation badge for the signed segment");
    assert!(text.contains("signer:"), "prints the signer DID line");
    assert!(text.contains("manifest store:"), "prints the manifest dump header");
}
