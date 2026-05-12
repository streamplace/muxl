//! End-to-end test for `sign_segment_stream`: feed an fMP4 fixture
//! through the streaming signer and confirm each per-GoP event carries
//! per-track signed canonical-segment bytes that c2pa::Reader validates
//! as standalone .m4s assets.
//!
//! Events are decoded via `serde_cbor::Value` rather than the typed
//! `SignedEvent`: dasl's serde-derived deserializer trips on
//! internally-tagged enum variants whose nested structs contain
//! `#[serde(with = "serde_bytes")]` fields (the Init event's catalog
//! goes through that path). The Go decoder consumed by Streamplace
//! doesn't have the issue — these tests just need to verify the wire
//! shape, not the typed round-trip.

use std::io::Cursor;
use std::path::PathBuf;

use muxl_sign::{SignerKey, SigningAlg, sign_segment_stream};
use serde_cbor::Value;

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

const SEGMENT_MANIFEST: &str = r#"{
    "title": "muxl-sign segment-stream segment",
    "assertions": [
        { "label": "c2pa.actions",
          "data": { "actions": [{ "action": "c2pa.created" }] } }
    ]
}"#;

const WRAPPER_MANIFEST: &str = r#"{
    "title": "muxl-sign segment-stream wrapper",
    "assertions": [
        { "label": "c2pa.actions",
          "data": { "actions": [{ "action": "c2pa.created" }] } }
    ]
}"#;

/// Walk a CBOR stream of independently-encoded values and return them
/// as a Vec<Value>. Stops at the first decode error.
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

#[test]
fn segment_stream_signs_each_gop() {
    let fmp4 = std::fs::read(repo_path("samples/fixtures/h264-opus-frag.mp4"))
        .expect("read fmp4 fixture");

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
    assert!(!output.is_empty(), "stream produced no output");

    let events = cbor_stream(&output);

    // First event should be Init; the rest signed-segment.
    assert_eq!(event_type(&events[0]), Some("init"));
    let segments: Vec<&Value> = events
        .iter()
        .filter(|e| event_type(e) == Some("signed-segment"))
        .collect();
    assert!(
        segments.len() >= 2,
        "fixture should produce at least 2 GOP segments, got {}",
        segments.len()
    );

    for (i, event) in segments.iter().enumerate() {
        // number
        let number = match get_text_key(event, "number") {
            Some(Value::Integer(n)) => *n as u32,
            _ => panic!("signed-segment {i} missing/non-int number"),
        };
        assert_eq!(number, (i + 1) as u32);

        // tracks: per-track signed canonical-segment bytes
        let tracks = match get_text_key(event, "tracks") {
            Some(Value::Map(m)) => m,
            _ => panic!("signed-segment {i} missing/non-map tracks"),
        };
        assert!(!tracks.is_empty(), "segment {number} should have tracks");

        // body_size matches sum of per-track byte lengths
        let body_size = match get_text_key(event, "body_size") {
            Some(Value::Integer(n)) => *n as u64,
            _ => panic!("signed-segment {i} missing/non-int body_size"),
        };
        let total: u64 = tracks
            .values()
            .map(|v| match v {
                Value::Bytes(b) => b.len() as u64,
                _ => 0,
            })
            .sum();
        assert_eq!(
            body_size, total,
            "body_size must equal sum of track byte lengths"
        );

        // Each per-track buffer must be a c2pa-signed .m4s asset.
        for (tid_key, buf_val) in tracks {
            let tid = match tid_key {
                Value::Text(s) => s.clone(),
                _ => panic!("track key must be text"),
            };
            let buf = match buf_val {
                Value::Bytes(b) => b,
                _ => panic!("track {tid} value must be byte string"),
            };
            let reader =
                c2pa::Reader::from_stream("m4s", Cursor::new(buf.as_slice()))
                    .unwrap_or_else(|e| {
                        panic!("c2pa::Reader on track {tid} signed canonical segment: {e}")
                    });
            let _ = reader
                .active_manifest()
                .unwrap_or_else(|| panic!("track {tid} carries an active manifest"));
        }
    }
}
