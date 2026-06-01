//! `muxl wrap` accepts multiple inputs, `tar`-style (output first, then one or
//! more inputs), and splices their canonical segments into one presentation
//! MP4 in argument order.
//!
//! Exercises the public CLI handler `muxl::cli::cmd_wrap` directly so the
//! arg-shape (output first, `inputs: Vec`) and the concatenation semantics are
//! both pinned: a future reorder of the positional args, or a regression in
//! clap's variadic parsing, breaks this test.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use muxl::cli::{cmd_segment, cmd_wrap, SegmentArgs, WrapArgs, WrapFormat};
use muxl::io::FileReadAt;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("samples/fixtures")
        .join(name)
}

/// Canonical fMP4 bytes for a fixture, via the public reader + fmp4 writer.
fn canonical_fmp4(name: &str) -> Vec<u8> {
    let input = FileReadAt::open(&fixture_path(name)).unwrap();
    let source = muxl::read(&input).unwrap();
    let mut out = Vec::new();
    muxl::fmp4::write(&source, &input, &mut out).unwrap();
    out
}

/// Split a fMP4 into its individual per-track canonical segments, in the
/// stream's storage order (per-GoP, track-id ascending) — the exact unit a
/// caller would have on disk as separate `.m4s` files.
fn segments_of(fmp4: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut segs = Vec::new();
    muxl::segment_fmp4(&mut Cursor::new(fmp4), |gop| {
        let mut tids: Vec<u32> = gop.tracks.keys().copied().collect();
        tids.sort();
        for tid in tids {
            segs.push((tid, gop.tracks[&tid].clone()));
        }
        Ok(())
    })
    .unwrap();
    segs
}

/// The core property: `wrap <out> <seg0> <seg1> … <segN>` recovers exactly the
/// input segments, verbatim and in argument order. This is the tar contract —
/// inputs are spliced together in the sequence given — combined with MUXL's
/// round-trip property (segment bytes survive the wrap unchanged).
#[test]
fn wrap_concatenates_multiple_m4s_in_argument_order() {
    let fmp4 = canonical_fmp4("h264-opus-frag.mp4");
    let segs = segments_of(&fmp4);
    assert!(
        segs.len() >= 3,
        "fixture must yield several segments to meaningfully test ordering (got {})",
        segs.len()
    );

    // Each canonical segment lands in its own `.m4s` file.
    let dir = tempfile::tempdir().unwrap();
    let inputs: Vec<PathBuf> = segs
        .iter()
        .enumerate()
        .map(|(i, (tid, data))| {
            let p = dir.path().join(format!("seg{i:03}_track{tid}.m4s"));
            std::fs::write(&p, data).unwrap();
            p
        })
        .collect();

    // tar-style invocation: output path first, then every input.
    let out = dir.path().join("wrapped.mp4");
    cmd_wrap(WrapArgs {
        output: out.clone(),
        inputs: inputs.clone(),
        format: WrapFormat::Fmp4,
        init_only: false,
    })
    .unwrap();

    // Unwrapping the result must recover the inputs 1:1, byte-verbatim, in order.
    let wrapped = std::fs::read(&out).unwrap();
    let recovered = muxl::reader::unwrap(&wrapped).unwrap();
    assert_eq!(
        recovered.len(),
        segs.len(),
        "every input segment must appear in the wrapped output"
    );
    for (i, seg) in recovered.iter().enumerate() {
        assert_eq!(seg.track_id, segs[i].0, "segment {i}: track-id / order mismatch");
        assert_eq!(
            seg.data,
            segs[i].1.as_slice(),
            "segment {i}: bytes must be carried through verbatim and in input order"
        );
    }
}

/// Wrapping the individual per-track `.m4s` files must produce byte-for-byte
/// the same presentation MP4 as wrapping the single whole fMP4 that contains
/// the same segments in the same order — multi-input is purely a splicing
/// convenience over the single-input path.
#[test]
fn wrap_multi_input_equals_wrapping_the_whole_fmp4() {
    let src = canonical_fmp4("h264-opus-frag.mp4");
    let segs = segments_of(&src);

    let dir = tempfile::tempdir().unwrap();

    // Reference fMP4: init + the same segments, track-id ascending then GoP
    // order — i.e. the storage layout `muxl segment --fmp4` emits.
    let catalog = muxl::segment_fmp4(&mut Cursor::new(&src), |_| Ok(())).unwrap();
    let mut whole = muxl::fmp4::init_segment(&catalog).unwrap();
    let mut by_track: std::collections::BTreeMap<u32, Vec<&Vec<u8>>> = Default::default();
    for (tid, data) in &segs {
        by_track.entry(*tid).or_default().push(data);
    }
    let mut ordered: Vec<(u32, &Vec<u8>)> = Vec::new();
    for (tid, datas) in &by_track {
        for data in datas {
            whole.extend_from_slice(data);
            ordered.push((*tid, data));
        }
    }
    let whole_path = dir.path().join("whole.fmp4");
    std::fs::write(&whole_path, &whole).unwrap();

    // Individual segments, written in that same storage order.
    let inputs: Vec<PathBuf> = ordered
        .iter()
        .enumerate()
        .map(|(i, (tid, data))| {
            let p = dir.path().join(format!("seg{i:03}_track{tid}.m4s"));
            std::fs::write(&p, data).unwrap();
            p
        })
        .collect();

    for format in [WrapFormat::Fmp4, WrapFormat::Flat] {
        let single = dir.path().join("single.mp4");
        cmd_wrap(WrapArgs {
            output: single.clone(),
            inputs: vec![whole_path.clone()],
            format,
            init_only: false,
        })
        .unwrap();

        let multi = dir.path().join("multi.mp4");
        cmd_wrap(WrapArgs {
            output: multi.clone(),
            inputs: inputs.clone(),
            format,
            init_only: false,
        })
        .unwrap();

        assert_eq!(
            std::fs::read(&single).unwrap(),
            std::fs::read(&multi).unwrap(),
            "wrapping the per-track .m4s files must match wrapping the whole fMP4 ({format:?})"
        );
    }
}

/// `segment --flat` must accept a flat (faststart) MP4 — which the streaming
/// modes can't — and emit a round-trippable MUXL flat (segments recoverable).
#[test]
fn segment_flat_accepts_flat_input_and_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("flat.mp4");
    cmd_segment(SegmentArgs {
        input: fixture_path("h264-aac.mp4").to_str().unwrap().to_string(),
        dir: None,
        fmp4: None,
        stdout: false,
        flat: Some(out.clone()),
        remap_track: vec![],
    })
    .unwrap();

    // The output is a MUXL flat: unwrap recovers the canonical segments.
    let bytes = std::fs::read(&out).unwrap();
    let segs = muxl::reader::unwrap(&bytes).unwrap();
    assert!(
        !segs.is_empty(),
        "segment --flat output should unwrap to canonical segments"
    );
    let tracks: std::collections::BTreeSet<u32> = segs.iter().map(|s| s.track_id).collect();
    assert!(tracks.len() >= 2, "expected video + audio tracks, got {tracks:?}");
}
