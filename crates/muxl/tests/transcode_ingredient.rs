//! End-to-end test for [`muxl_sign::sign_transcode_segment`]: prove that a
//! transcoded output segment's signed manifest declares the segment it was
//! transcoded from as a `parentOf` ingredient, bound to a `c2pa.transcoded`
//! action — the C2PA-spec provenance link a Livepeer orchestrator would emit.
//!
//! Both segments come from the same fMP4 fixture (a stand-in for a real
//! transcode, which would change resolution): the "source" is signed first so
//! the parent ingredient carries its own provenance, and the "output" is a
//! bare unsigned canonical `.m4s` straight off muxl's `Segmenter`.

use std::io::Cursor;
use std::path::PathBuf;

use c2pa::assertions::Actions;
use c2pa::{Reader, Relationship, ValidationState};
use muxl_sign::{
    SignerKey, SigningAlg, TRANSCODE_INGREDIENT_LABEL, sign_segment_stream, sign_transcode_segment,
};
use serde_cbor::Value;

fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn signer() -> SignerKey {
    SignerKey::from_pem_files(
        repo_path("samples/test-keys/es256k-cert.pem"),
        repo_path("samples/test-keys/es256k-key.pem"),
        SigningAlg::Es256K,
    )
    .expect("load es256k signer")
}

const FIXTURE: &str = "samples/fixtures/h264-opus-frag.mp4";

/// Decode a stream of back-to-back CBOR values until the first decode error.
fn cbor_values(bytes: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
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

fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(m) => m.iter().find_map(|(k, val)| match k {
            Value::Text(k) if k == key => Some(val),
            _ => None,
        }),
        _ => None,
    }
}

/// Sign the fixture through the streaming signer and return the first signed
/// per-track canonical `.m4s` buffer — a fully S2PA-signed segment, used here
/// as the transcode *source* (parent ingredient with its own provenance).
fn signed_source_segment() -> Vec<u8> {
    const MANIFEST: &str = r#"{
        "title": "muxl-sign transcode source",
        "assertions": [
            { "label": "c2pa.actions",
              "data": { "actions": [{ "action": "c2pa.created" }] } }
        ]
    }"#;

    let fmp4 = std::fs::read(repo_path(FIXTURE)).expect("read fmp4 fixture");
    let mut output = Vec::new();
    sign_segment_stream(
        &mut Cursor::new(&fmp4),
        &mut output,
        &signer(),
        MANIFEST,
        MANIFEST,
    )
    .expect("sign_segment_stream");

    for ev in cbor_values(&output) {
        if map_get(&ev, "type") != Some(&Value::Text("signed-segment".into())) {
            continue;
        }
        if let Some(Value::Map(tracks)) = map_get(&ev, "tracks") {
            if let Some((_tid, Value::Bytes(b))) = tracks.iter().next() {
                return b.clone();
            }
        }
    }
    panic!("no signed-segment track bytes produced");
}

/// Segment the fixture into bare canonical `.m4s` segments and return the
/// first track's unsigned canonical bytes — the stand-in for the unsigned
/// output of a transcode.
fn unsigned_output_segment() -> Vec<u8> {
    let fmp4 = std::fs::read(repo_path(FIXTURE)).expect("read fmp4 fixture");
    let mut segmenter = muxl::Segmenter::new();
    let mut events = segmenter.feed(&fmp4).expect("segmenter feed");
    events.extend(segmenter.flush().expect("segmenter flush"));
    for ev in events {
        if let muxl::SegmenterEvent::Segment(gop) = ev {
            if let Some((_tid, bytes)) = gop.tracks.iter().next() {
                return bytes.clone();
            }
        }
    }
    panic!("no canonical segment produced");
}

#[test]
fn transcode_output_declares_source_as_parent_ingredient() {
    let source = signed_source_segment();
    let output = unsigned_output_segment();

    // The output manifest opens the source ingredient and records the
    // transcode, both referencing it by TRANSCODE_INGREDIENT_LABEL.
    let manifest = format!(
        r#"{{
            "title": "muxl-sign transcode output",
            "assertions": [
                {{ "label": "c2pa.actions", "data": {{ "actions": [
                    {{ "action": "c2pa.opened",
                       "parameters": {{ "org.cai.ingredientIds": ["{label}"] }} }},
                    {{ "action": "c2pa.transcoded",
                       "parameters": {{ "org.cai.ingredientIds": ["{label}"] }} }}
                ]}}}}
            ]
        }}"#,
        label = TRANSCODE_INGREDIENT_LABEL
    );

    let signed = sign_transcode_segment(&output, &source, &signer(), &manifest)
        .expect("sign_transcode_segment");

    // Re-read the signed output as a standalone m4s asset.
    let reader = Reader::from_stream("m4s", Cursor::new(&signed)).expect("c2pa reader");
    let active = reader.active_manifest().expect("active manifest");

    // 1) The output names exactly one parentOf ingredient (the source).
    let parents: Vec<_> = active
        .ingredients()
        .iter()
        .filter(|i| i.relationship() == &Relationship::ParentOf)
        .collect();
    assert_eq!(
        parents.len(),
        1,
        "expected exactly one parentOf ingredient, got {}",
        parents.len()
    );

    // 2) The manifest carries a c2pa.transcoded action.
    let actions: Actions = active
        .find_assertion(Actions::LABEL)
        .expect("c2pa.actions assertion present");
    assert!(
        actions
            .actions()
            .iter()
            .any(|a| a.action() == c2pa::assertions::c2pa_action::TRANSCODED),
        "manifest carries a c2pa.transcoded action; actions = {:?}",
        actions.actions().iter().map(|a| a.action()).collect::<Vec<_>>()
    );

    // 3) That action resolved its ingredient reference to the parent: the
    //    label bound to a real ingredient hashed URI at sign time. (A v2 claim
    //    errors at sign time if it can't resolve the id, so reaching here
    //    already implies the binding — this asserts the resolved shape.)
    let transcoded = actions
        .actions()
        .iter()
        .find(|a| a.action() == c2pa::assertions::c2pa_action::TRANSCODED)
        .expect("transcoded action");
    let action_json = serde_json::to_value(transcoded).expect("serialize transcoded action");
    let refs = action_json
        .get("parameters")
        .and_then(|p| p.get("ingredients"))
        .and_then(|r| r.as_array())
        .expect("transcoded action references its ingredient(s)");
    assert!(
        !refs.is_empty(),
        "transcoded action ingredient list must be non-empty"
    );

    // 4) The whole thing still validates (signature + hashes intact). Trust is
    //    intentionally disabled in muxl, so Valid (not Trusted) is expected.
    assert_ne!(
        reader.validation_state(),
        ValidationState::Invalid,
        "signed transcode output should validate; status = {:?}",
        reader.validation_status()
    );
}
