//! Phase A probe: feed a bare canonical-segment byte sequence
//! (muxl-uuid + moof+mdat) into c2pa Builder as a CMAF segment and
//! confirm the output is `[c2pa-uuid][muxl-uuid][moof+mdat]` and that
//! `c2pa::Reader` validates it back.

use std::io::Cursor;
use std::path::PathBuf;

use c2pa::{Builder, CallbackSigner, Reader, SigningAlg};

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

const MANIFEST: &str = r#"{
    "title": "m4s probe",
    "assertions": [
        {"label": "c2pa.actions", "data": {"actions": [{"action": "c2pa.created"}]}}
    ]
}"#;

const MUXL_UUID: [u8; 16] = muxl::segment::MUXL_UUID;
const C2PA_UUID: [u8; 16] = [
    0xd8, 0xfe, 0xc3, 0xd6, 0x1b, 0x0e, 0x48, 0x3c, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7e, 0xc4, 0x81,
];

/// Smallest plausible canonical segment: muxl uuid box + one moof+mdat.
fn bare_canonical_segment() -> Vec<u8> {
    let mut out = Vec::new();

    // uuid box: 4-byte size, "uuid", 16-byte uuid, payload (stub).
    let drisl_stub = [0u8, 0, 0, 0];
    let uuid_size = 8 + 16 + drisl_stub.len();
    out.extend_from_slice(&(uuid_size as u32).to_be_bytes());
    out.extend_from_slice(b"uuid");
    out.extend_from_slice(&MUXL_UUID);
    out.extend_from_slice(&drisl_stub);

    // moof box with a tiny mfhd inside.
    let moof_payload = [
        0u8, 0, 0, 16, b'm', b'f', b'h', b'd', 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    let moof_size = 8 + moof_payload.len();
    out.extend_from_slice(&(moof_size as u32).to_be_bytes());
    out.extend_from_slice(b"moof");
    out.extend_from_slice(&moof_payload);

    // mdat box with 8 bytes of arbitrary payload.
    let mdat_payload = [0xAAu8; 8];
    let mdat_size = 8 + mdat_payload.len();
    out.extend_from_slice(&(mdat_size as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&mdat_payload);

    out
}

fn es256k_signer() -> CallbackSigner {
    use k256::ecdsa::{Signature, SigningKey, signature::Signer};
    use k256::pkcs8::DecodePrivateKey;

    let key_pem = std::fs::read(repo_path("samples/test-keys/es256k-key.pem"))
        .expect("read es256k-key.pem");
    let cert_pem = std::fs::read(repo_path("samples/test-keys/es256k-cert.pem"))
        .expect("read es256k-cert.pem");

    let pem_str = std::str::from_utf8(&key_pem).unwrap();
    let signing_key = SigningKey::from_pkcs8_pem(pem_str).expect("parse k256 key");

    let cb = move |_ctx: *const (), data: &[u8]| -> std::result::Result<Vec<u8>, c2pa::Error> {
        let sig: Signature = signing_key.sign(data);
        Ok(sig.to_bytes().to_vec())
    };
    CallbackSigner::new(cb, SigningAlg::Es256K, cert_pem)
}

#[test]
fn sign_bare_canonical_segment_as_m4s() {
    let canon = bare_canonical_segment();
    // Sanity: input starts with a muxl uuid box.
    assert_eq!(&canon[4..8], b"uuid", "input starts with uuid box");
    assert_eq!(canon[8..24], MUXL_UUID, "input uuid is the muxl one");

    let signer = es256k_signer();
    let mut builder = Builder::from_json(MANIFEST).expect("Builder::from_json");

    let mut source = Cursor::new(&canon);
    let mut output: Vec<u8> = Vec::new();
    let mut dest = Cursor::new(&mut output);

    // Sign as m4s — the patched fork should accept this format.
    builder
        .sign(&signer, "m4s", &mut source, &mut dest)
        .expect("Builder::sign on bare m4s");

    assert!(!output.is_empty(), "non-empty signed output");

    // Output shape: [c2pa-uuid box][muxl-uuid box][moof][mdat]
    assert_eq!(&output[4..8], b"uuid", "first box is a uuid box");
    assert_eq!(output[8..24], C2PA_UUID, "first uuid is the c2pa one");

    let c2pa_size = u32::from_be_bytes(output[0..4].try_into().unwrap()) as usize;
    assert!(c2pa_size < output.len(), "c2pa uuid size {c2pa_size} < total {}", output.len());

    // Bytes after the c2pa uuid box should match the original canonical segment verbatim.
    assert_eq!(
        &output[c2pa_size..],
        canon.as_slice(),
        "canonical segment bytes survive signing verbatim"
    );

    // The muxl uuid box should be the first thing after the c2pa uuid box.
    assert_eq!(&output[c2pa_size + 4..c2pa_size + 8], b"uuid");
    assert_eq!(output[c2pa_size + 8..c2pa_size + 24], MUXL_UUID);

    // Verify the signature reads back cleanly.
    let reader = Reader::from_stream("m4s", Cursor::new(&output))
        .expect("Reader::from_stream on signed m4s");
    assert!(
        reader.active_manifest().is_some(),
        "signed m4s has an active manifest",
    );
}
