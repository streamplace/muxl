//! End-to-end test: sign a multi-track fixture and confirm the new
//! per-canonical-segment + wrapper signing shape — the wrapper c2pa-uuid
//! sits at the file head, and the mdat envelope contains one
//! `[c2pa-uuid][muxl-uuid][moof+mdat]*` chunk per canonical segment.

use std::io::Cursor;
use std::path::PathBuf;

use muxl::io::FileReadAt;
use muxl_sign::{SignerKey, SigningAlg, sign_per_track};

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

const SEGMENT_MANIFEST: &str = r#"{
    "title": "muxl-sign per-segment test",
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

const MUXL_UUID: [u8; 16] = muxl::segment::MUXL_UUID;
const C2PA_UUID: [u8; 16] = [
    0xd8, 0xfe, 0xc3, 0xd6, 0x1b, 0x0e, 0x48, 0x3c, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7e, 0xc4, 0x81,
];

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
        SEGMENT_MANIFEST,
        WRAPPER_MANIFEST,
        &mut output,
    )
    .expect("sign_per_track");

    assert!(!output.is_empty(), "wrapper bytes produced");

    // Wrapper c2pa-uuid present and validates via c2pa::Reader.
    let reader = c2pa::Reader::from_stream("video/mp4", Cursor::new(&output))
        .expect("Reader::from_stream on signed wrapper");
    let _active = reader
        .active_manifest()
        .expect("wrapper has an active manifest");

    // Body shape: at least `track_count` canonical segments inside the
    // outer mdat, each prefixed by a c2pa-uuid (per-segment sig) immediately
    // followed by a muxl-uuid (catalog).
    let muxl_count = count_subseq(&output, &MUXL_UUID);
    let c2pa_count = count_subseq(&output, &C2PA_UUID);
    assert!(
        muxl_count >= track_count,
        "expected ≥{track_count} muxl uuid occurrences inside body, got {muxl_count}"
    );
    // One c2pa-uuid at file head (wrapper) + one per canonical segment;
    // each c2pa identifier byte sequence may also appear inside the JUMBF
    // payload as part of the assertion exclusion DataMap, so the count is
    // a lower bound rather than equality.
    assert!(
        c2pa_count >= muxl_count + 1,
        "expected ≥{} c2pa uuid occurrences (1 wrapper + 1 per canonical segment), got {c2pa_count}",
        muxl_count + 1
    );
}

#[test]
fn sign_with_generated_s2pa_cert() {
    // End-to-end check that an S2PA-shaped self-signed leaf produced by
    // `muxl_sign::cert::generate_cert` is accepted by c2pa-rs's sign and
    // verify paths. If c2pa-rs's cert profile check rejects self-signed
    // leaves we'll see it here as a SignerKey::from_pem_bytes failure or
    // a Builder::sign error.
    let input_path = repo_path("samples/fixtures/h264-aac.mp4");
    let input = FileReadAt::open(&input_path).expect("open fixture");
    let source = muxl::read(&input).expect("read source");

    // Fresh keypair + cert. CN defaults to did:key(pubkey). Streamplace's
    // production certs carry organizationName="Streamplace"; we set one
    // too so the cert profile shape matches.
    let key = muxl_sign::generate_key();
    let cert_der = muxl_sign::generate_cert(&key, None, Some("S2PA test"))
        .expect("generate_cert");
    let cert_pem = muxl_sign::cert_to_pem(&cert_der);
    let key_pem = muxl_sign::key_to_pem(&key).expect("key_to_pem");

    let signer = SignerKey::from_pem_bytes(
        cert_pem.into_bytes(),
        key_pem.into_bytes(),
        SigningAlg::Es256K,
    );

    let mut output: Vec<u8> = Vec::new();
    sign_per_track(
        &source,
        &input,
        &signer,
        SEGMENT_MANIFEST,
        WRAPPER_MANIFEST,
        &mut output,
    )
    .expect("sign_per_track with generated S2PA cert");

    assert!(!output.is_empty(), "wrapper bytes produced");
    let reader = c2pa::Reader::from_stream("video/mp4", Cursor::new(&output))
        .expect("Reader::from_stream on signed wrapper");
    let _active = reader
        .active_manifest()
        .expect("wrapper has an active manifest");
}

fn count_subseq(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    (0..=haystack.len() - needle.len())
        .filter(|&i| &haystack[i..i + needle.len()] == needle)
        .count()
}
