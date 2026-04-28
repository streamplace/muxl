//! End-to-end test for `sign_segment_stream`: feed an fMP4 fixture through
//! the streaming signer and confirm each per-GoP CBOR event decodes back
//! to a c2pa-readable signed flat MP4.

use std::io::{BufReader, Cursor};
use std::path::PathBuf;

use dasl::drisl::de::iter_from_reader;
use muxl_sign::{SignedEvent, SignerKey, SigningAlg, sign_segment_stream};

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

const TRACK_MANIFEST: &str = r#"{
    "title": "muxl-sign segment-stream track",
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
        TRACK_MANIFEST,
        WRAPPER_MANIFEST,
    )
    .expect("sign_segment_stream");

    assert!(!output.is_empty(), "stream produced no output");

    let reader = BufReader::new(Cursor::new(&output[..]));
    let events: Vec<SignedEvent> = iter_from_reader::<SignedEvent, _>(reader)
        .map(|r| r.expect("decode signed event"))
        .collect();

    assert!(
        events.len() >= 2,
        "fixture should produce at least 2 GOP segments, got {}",
        events.len()
    );

    for (i, event) in events.iter().enumerate() {
        let SignedEvent::SignedSegment { number, data } = event;
        assert_eq!(
            *number,
            (i + 1) as u32,
            "events should be numbered 1..N in order"
        );
        let reader = c2pa::Reader::from_stream("video/mp4", Cursor::new(data))
            .expect("c2pa::Reader on signed-segment data");
        let _ = reader
            .active_manifest()
            .expect("signed segment carries an active manifest");
    }
}
