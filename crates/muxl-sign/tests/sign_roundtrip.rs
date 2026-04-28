//! End-to-end test: sign a multi-track fixture per-track, then read the
//! wrapper back with `c2pa::Reader` and confirm the per-track ingredients
//! are present.

use std::io::Cursor;
use std::path::PathBuf;

use muxl::io::FileReadAt;
use muxl_sign::{SignerKey, SigningAlg, sign_per_track};

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

const TRACK_MANIFEST: &str = r#"{
    "title": "muxl-sign per-track test",
    "assertions": [
        {
            "label": "c2pa.actions",
            "data": {"actions": [{"action": "c2pa.created"}]}
        }
    ]
}"#;

const WRAPPER_MANIFEST: &str = r#"{
    "title": "muxl-sign wrapper test",
    "assertions": [
        {
            "label": "c2pa.actions",
            "data": {"actions": [{"action": "c2pa.created"}]}
        }
    ]
}"#;

#[test]
fn sign_per_track_roundtrip_h264_aac() {
    let input_path = repo_path("samples/fixtures/h264-aac.mp4");
    let input = FileReadAt::open(&input_path).expect("open fixture");
    let source = muxl::read(&input).expect("read source");
    let track_count = source.plan.tracks.len();
    assert!(track_count >= 2, "fixture must be multi-track");

    let signer = SignerKey::from_pem_files(
        repo_path("samples/test-keys/es256k-cert.pem"),
        repo_path("samples/test-keys/es256k-key.pem"),
        SigningAlg::Es256K,
    )
    .expect("load signer");

    let mut output: Vec<u8> = Vec::new();
    sign_per_track(
        &source,
        &input,
        &signer,
        TRACK_MANIFEST,
        WRAPPER_MANIFEST,
        &mut output,
    )
    .expect("sign_per_track");

    assert!(!output.is_empty(), "wrapper bytes produced");

    // Read the wrapper back through c2pa-rs and confirm the manifest shape.
    let reader = c2pa::Reader::from_stream("video/mp4", Cursor::new(&output))
        .expect("Reader::from_stream on signed wrapper");
    let active = reader.active_manifest().expect("wrapper has an active manifest");
    assert_eq!(
        active.ingredients().len(),
        track_count,
        "wrapper manifest should reference one ingredient per source track",
    );
    // One manifest per ingredient + one for the wrapper.
    assert_eq!(
        reader.manifests().len(),
        track_count + 1,
        "store should hold the wrapper manifest plus one per-track ingredient manifest",
    );
}
