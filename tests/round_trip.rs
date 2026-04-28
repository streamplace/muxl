//! End-to-end round-trip tests exercising fragments + catalog ↔ fMP4 ↔
//! flat MP4 in every direction we support.
//!
//! The public surface exercised here is the post-refactor one:
//!
//! - `muxl::read(input)` — auto-detecting reader, returns a `Source`.
//! - `muxl::fmp4::{read, write, init_segment}` — fMP4 wrapper I/O.
//! - `muxl::flat::{read, write}` — flat MP4 wrapper I/O.
//! - `muxl::catalog::{from_input, to_drisl, from_drisl, to_hang_json,
//!   from_hang_json}` — catalog-only and wire-form (de)serialization.
//!
//! A `Source` carries a `Catalog` (codec info) and a `Plan`
//! (per-sample offsets/sizes/durations — metadata only). The write
//! functions stream sample bytes from the original input on demand, so
//! the whole pipeline tolerates arbitrarily long sources.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use muxl::catalog::{Catalog, Container};
use muxl::io::FileReadAt;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("samples/fixtures")
        .join(name)
}

fn open_fixture(name: &str) -> FileReadAt {
    FileReadAt::open(&fixture_path(name))
        .unwrap_or_else(|e| panic!("{}: {e}", fixture_path(name).display()))
}

fn read_fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name))
        .unwrap_or_else(|e| panic!("{}: {e}", fixture_path(name).display()))
}

/// Produce a canonical MUXL flat MP4 from any fixture, streaming from
/// disk via `FileReadAt` — never loads the whole source into memory.
fn to_canonical_flat_on_disk(name: &str) -> Vec<u8> {
    let input = open_fixture(name);
    let source = muxl::read(&input).unwrap();
    let mut out = Vec::new();
    muxl::flat::write(&source, &input, &mut out).unwrap();
    out
}

/// Convert an in-memory flat MP4 buffer to canonical flat via the same
/// public path. Used when we need to re-process the output of a
/// previous stage.
fn to_canonical_flat(bytes: &[u8]) -> Vec<u8> {
    let buf = bytes.to_vec();
    let source = muxl::read(&buf).unwrap();
    let mut out = Vec::new();
    muxl::flat::write(&source, &buf, &mut out).unwrap();
    out
}

/// Convert a flat MP4 buffer to an fMP4 via the public `fmp4::write`
/// entry point. Uses a temp file so `flat_mp4_to_fmp4`'s ReadAt-backed
/// sample reads have something to seek into.
fn to_fmp4(bytes: &[u8]) -> Vec<u8> {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), bytes).unwrap();
    let input = FileReadAt::open(tmp.path()).unwrap();
    let source = muxl::read(&input).unwrap();
    let mut out: Vec<u8> = Vec::new();
    muxl::fmp4::write(&source, &input, &mut out).unwrap();
    out.flush().unwrap();
    out
}

/// Assert two catalogs describe equivalent tracks. Stable across every
/// wrapper conversion in the pipeline.
fn assert_catalogs_equivalent(a: &Catalog, b: &Catalog, context: &str) {
    let a_video: Vec<_> = a.video_configs().collect();
    let b_video: Vec<_> = b.video_configs().collect();
    let a_audio: Vec<_> = a.audio_configs().collect();
    let b_audio: Vec<_> = b.audio_configs().collect();

    assert_eq!(
        a_video.len(),
        b_video.len(),
        "{context}: video rendition count"
    );
    assert_eq!(
        a_audio.len(),
        b_audio.len(),
        "{context}: audio rendition count"
    );

    for (av, bv) in a_video.iter().zip(b_video.iter()) {
        assert_eq!(av.codec, bv.codec, "{context}: video codec");
        assert_eq!(av.description, bv.description, "{context}: video description");
        assert_eq!(av.coded_width, bv.coded_width, "{context}: codedWidth");
        assert_eq!(av.coded_height, bv.coded_height, "{context}: codedHeight");
        assert_eq!(av.container, bv.container, "{context}: video container");
    }
    for (aa, bb) in a_audio.iter().zip(b_audio.iter()) {
        assert_eq!(aa.codec, bb.codec, "{context}: audio codec");
        assert_eq!(aa.description, bb.description, "{context}: audio description");
        assert_eq!(aa.sample_rate, bb.sample_rate, "{context}: sampleRate");
        assert_eq!(
            aa.number_of_channels, bb.number_of_channels,
            "{context}: numberOfChannels"
        );
        assert_eq!(aa.container, bb.container, "{context}: audio container");
    }
}

/// Read a catalog from any MP4 bytes using the public auto-detect path.
fn catalog_of(bytes: &[u8]) -> Catalog {
    muxl::catalog::from_input(&bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------------------
// 1. flat MP4 self-idempotence
// ---------------------------------------------------------------------------

/// `flat::write(read(flat::write(read(x)))) == flat::write(read(x))` —
/// the canonical flat form is a fixed point.
#[test]
fn canonical_flat_is_idempotent() {
    for fixture in &[
        "h264-aac.mp4",
        "h264-opus.mp4",
        "opus-audio-only.mp4",
        "h264-opus-frag.mp4",
    ] {
        let first = to_canonical_flat_on_disk(fixture);
        let second = to_canonical_flat(&first);
        assert_eq!(first, second, "{fixture}: canonical flat not idempotent");
    }
}

// ---------------------------------------------------------------------------
// 2. flat → fMP4 → flat round trip
// ---------------------------------------------------------------------------

/// Canonical flat → fMP4 → back to canonical flat produces byte-identical
/// output. fMP4 and flat are alternative wrappers over the same fragments.
#[test]
fn flat_to_fmp4_to_flat_round_trip() {
    for fixture in &["h264-aac.mp4", "h264-opus.mp4", "opus-audio-only.mp4"] {
        let canonical_flat = to_canonical_flat_on_disk(fixture);
        let fmp4 = to_fmp4(&canonical_flat);
        let back_to_flat = to_canonical_flat(&fmp4);
        assert_eq!(
            canonical_flat, back_to_flat,
            "{fixture}: flat → fMP4 → flat not byte-identical"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. fMP4 → flat → fMP4 round trip
// ---------------------------------------------------------------------------

/// Start from a fragmented source, canonicalize to flat, back to fMP4.
/// Compare against a second pass to confirm the fMP4 writer is
/// deterministic across the cycle.
#[test]
fn fmp4_to_flat_to_fmp4_round_trip() {
    let src = read_fixture("h264-opus-frag.mp4");
    let canonical_flat = to_canonical_flat(&src);
    let fmp4_a = to_fmp4(&canonical_flat);
    let flat_again = to_canonical_flat(&fmp4_a);
    let fmp4_b = to_fmp4(&flat_again);
    assert_eq!(fmp4_a, fmp4_b, "fMP4 writer not deterministic across round-trip");
    assert_eq!(
        canonical_flat, flat_again,
        "flat form diverged after fMP4 detour"
    );
}

// ---------------------------------------------------------------------------
// 4. catalog equivalence across all three forms
// ---------------------------------------------------------------------------

/// Catalog extracted from source, canonical flat, and derived fMP4 all
/// describe the same tracks.
#[test]
fn catalog_stable_across_forms() {
    for fixture in &["h264-aac.mp4", "h264-opus.mp4", "opus-audio-only.mp4"] {
        let src = read_fixture(fixture);
        let canonical_flat = to_canonical_flat(&src);
        let fmp4 = to_fmp4(&canonical_flat);

        assert_catalogs_equivalent(&catalog_of(&src), &catalog_of(&canonical_flat), fixture);
        assert_catalogs_equivalent(&catalog_of(&canonical_flat), &catalog_of(&fmp4), fixture);
    }
}

// ---------------------------------------------------------------------------
// 5. catalog wire round trips — DRISL and Hang JSON
// ---------------------------------------------------------------------------

/// Catalog → DRISL → decoded catalog → built init segment → extracted
/// catalog all describe the same tracks. This is the "fragments +
/// catalog out-of-band → reconstruct CMAF init" path.
#[test]
fn catalog_drisl_transport_round_trip() {
    for fixture in &["h264-aac.mp4", "h264-opus.mp4", "opus-audio-only.mp4"] {
        let src = read_fixture(fixture);
        let catalog = catalog_of(&src);

        let drisl = muxl::catalog::to_drisl(&catalog).unwrap();
        let decoded = muxl::catalog::from_drisl(&drisl).unwrap();
        assert_eq!(catalog, decoded, "{fixture}: DRISL round-trip");

        let init = muxl::fmp4::init_segment(&decoded).unwrap();
        let init_catalog = catalog_of(&init);
        assert_catalogs_equivalent(&catalog, &init_catalog, fixture);
    }
}

/// Same as above but through Hang-shaped JSON.
#[test]
fn catalog_hang_json_transport_round_trip() {
    for fixture in &["h264-aac.mp4", "h264-opus.mp4", "opus-audio-only.mp4"] {
        let src = read_fixture(fixture);
        let catalog = catalog_of(&src);

        let json = muxl::catalog::to_hang_json(&catalog).unwrap();
        let decoded = muxl::catalog::from_hang_json(&json).unwrap();
        assert_eq!(catalog, decoded, "{fixture}: Hang JSON round-trip");

        let init = muxl::fmp4::init_segment(&decoded).unwrap();
        let init_catalog = catalog_of(&init);
        assert_catalogs_equivalent(&catalog, &init_catalog, fixture);
    }
}

// ---------------------------------------------------------------------------
// 6. container field invariant
// ---------------------------------------------------------------------------

/// MUXL-produced tracks always carry `container: cmaf(timescale, trackId)`.
/// Legacy only exists for consuming non-MUXL Hang catalogs.
#[test]
fn muxl_outputs_always_have_cmaf_container() {
    for fixture in &["h264-aac.mp4", "h264-opus.mp4", "opus-audio-only.mp4"] {
        let src = read_fixture(fixture);
        let catalog = catalog_of(&src);
        for v in catalog.video_configs() {
            assert!(
                matches!(v.container, Container::Cmaf { .. }),
                "{fixture}: video container"
            );
            assert!(v.track_id() > 0, "{fixture}: video track_id must be set");
            assert!(v.timescale() > 0, "{fixture}: video timescale must be set");
        }
        for a in catalog.audio_configs() {
            assert!(
                matches!(a.container, Container::Cmaf { .. }),
                "{fixture}: audio container"
            );
            assert!(a.track_id() > 0, "{fixture}: audio track_id must be set");
            assert!(a.timescale() > 0, "{fixture}: audio timescale must be set");
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Source carries a materialized plan
// ---------------------------------------------------------------------------

/// A `Source` from a real file has per-track sample plans with
/// monotonically increasing decode times implied by the sample durations.
/// Exercises the `muxl::read` → `source.plan.tracks` path.
#[test]
fn source_plan_covers_every_track() {
    let input = open_fixture("h264-aac.mp4");
    let source = muxl::read(&input).unwrap();
    assert!(
        !source.plan.tracks.is_empty(),
        "source plan should have tracks"
    );
    for track in &source.plan.tracks {
        assert!(!track.samples.is_empty(), "track {} has no samples", track.track_id);
        assert!(track.timescale > 0, "track {} has zero timescale", track.track_id);
    }
}

// ---------------------------------------------------------------------------
// 8. Source::filter_to_track produces a single-track Source
// ---------------------------------------------------------------------------

/// Filtering a multi-track `Source` to a single track yields a `Source`
/// whose catalog and plan each describe exactly that one track. The
/// filtered source can drive `flat::write` to produce a per-track flat MP4
/// that itself round-trips back to a single-track `Source`.
#[test]
fn filter_to_track_produces_single_track_source() {
    let input = open_fixture("h264-aac.mp4");
    let source = muxl::read(&input).unwrap();

    let track_ids: Vec<u32> = source.plan.tracks.iter().map(|t| t.track_id).collect();
    assert!(track_ids.len() >= 2, "fixture must be multi-track");

    for &tid in &track_ids {
        let filtered = source
            .filter_to_track(tid)
            .unwrap_or_else(|| panic!("track {tid} should exist"));
        assert_eq!(filtered.plan.tracks.len(), 1);
        assert_eq!(filtered.plan.tracks[0].track_id, tid);
        let renditions =
            filtered.catalog.video_configs().count() + filtered.catalog.audio_configs().count();
        assert_eq!(renditions, 1, "filtered catalog should have exactly one rendition");

        let mut buf = Vec::new();
        muxl::flat::write(&filtered, &input, &mut buf)
            .expect("flat::write should succeed for a single-track source");

        let round_tripped = muxl::read(&buf).unwrap();
        assert_eq!(round_tripped.plan.tracks.len(), 1);
        assert_eq!(round_tripped.plan.tracks[0].track_id, tid);
    }

    assert!(source.filter_to_track(99_999).is_none());
}
