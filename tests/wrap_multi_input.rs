//! `muxl wrap` accepts multiple inputs, `tar`-style (output first, then one or
//! more inputs), and splices their canonical segments into one presentation
//! MP4 in argument order.
//!
//! Exercises the public CLI handler `muxl::cli::cmd_wrap` directly so the
//! arg-shape (positional `files`, output-first by default) and the
//! concatenation semantics are both pinned: a future reorder of the positional
//! args, or a regression in clap's variadic parsing, breaks this test.

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
        files: std::iter::once(out.clone()).chain(inputs.clone()).collect(),
        format: WrapFormat::Fmp4,
        flat: None,
        fmp4: None,
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

    // Reference fMP4: init + the canonical per-GoP segment stream (the layout
    // `muxl segment --fmp4` / `fmp4` emit — `segments_of` is already in that
    // order).
    let catalog = muxl::segment_fmp4(&mut Cursor::new(&src), |_| Ok(())).unwrap();
    let mut whole = muxl::fmp4::init_segment(&catalog).unwrap();
    for (_tid, data) in &segs {
        whole.extend_from_slice(data);
    }
    let whole_path = dir.path().join("whole.fmp4");
    std::fs::write(&whole_path, &whole).unwrap();

    // The same segments as individual files, in the same order.
    let inputs: Vec<PathBuf> = segs
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
            files: vec![single.clone(), whole_path.clone()],
            format,
            flat: None,
            fmp4: None,
            init_only: false,
        })
        .unwrap();

        let multi = dir.path().join("multi.mp4");
        cmd_wrap(WrapArgs {
            files: std::iter::once(multi.clone()).chain(inputs.clone()).collect(),
            format,
            flat: None,
            fmp4: None,
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

/// The `--flat <PATH>` / `--fmp4 <PATH>` shorthands name the output (and pick
/// the container) while leaving every positional as an input — so they must
/// produce byte-for-byte the same wrapper as the tar-style `--format` form with
/// the output spelled out first.
#[test]
fn wrap_output_flag_shorthand_matches_tar_style() {
    let src = canonical_fmp4("h264-opus-frag.mp4");
    let segs = segments_of(&src);
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

    // (shorthand setter, equivalent --format) pairs.
    let cases: [(fn(PathBuf) -> WrapArgs, WrapFormat); 2] = [
        (
            |out| WrapArgs {
                files: Vec::new(),
                format: WrapFormat::Fmp4,
                flat: Some(out),
                fmp4: None,
                init_only: false,
            },
            WrapFormat::Flat,
        ),
        (
            |out| WrapArgs {
                files: Vec::new(),
                format: WrapFormat::Fmp4,
                flat: None,
                fmp4: Some(out),
                init_only: false,
            },
            WrapFormat::Fmp4,
        ),
    ];

    for (make_shorthand, format) in cases {
        // tar-style: output first, then the inputs, with --format.
        let tar = dir.path().join("tar.mp4");
        cmd_wrap(WrapArgs {
            files: std::iter::once(tar.clone()).chain(inputs.clone()).collect(),
            format,
            flat: None,
            fmp4: None,
            init_only: false,
        })
        .unwrap();

        // Shorthand: positionals are all inputs; the flag names the output.
        let short = dir.path().join("short.mp4");
        let mut args = make_shorthand(short.clone());
        args.files = inputs.clone();
        cmd_wrap(args).unwrap();

        assert_eq!(
            std::fs::read(&tar).unwrap(),
            std::fs::read(&short).unwrap(),
            "--flat/--fmp4 shorthand must match the tar-style --format form ({format:?})"
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

/// Multi-track output must interleave by GoP, track_id-ascending —
/// [gop0 t1, gop0 t2, gop1 t1, gop1 t2, …] — per the spec (segments interleaved
/// by timestamp), not grouped per-track. Guards cmd_segment_fmp4's ordering.
#[test]
fn segment_fmp4_interleaves_per_gop_not_per_track() {
    let src = canonical_fmp4("h264-opus-frag.mp4");
    let dir = tempfile::tempdir().unwrap();
    let in_path = dir.path().join("in.fmp4");
    std::fs::write(&in_path, &src).unwrap();
    let out = dir.path().join("out.fmp4");
    cmd_segment(SegmentArgs {
        input: in_path.to_str().unwrap().to_string(),
        dir: None,
        fmp4: Some(out.clone()),
        stdout: false,
        flat: None,
        remap_track: vec![],
    })
    .unwrap();

    let bytes = std::fs::read(&out).unwrap();
    let tids: Vec<u32> = muxl::reader::unwrap(&bytes)
        .unwrap()
        .iter()
        .map(|s| s.track_id)
        .collect();
    assert!(tids.len() >= 4, "need >=2 GoPs x 2 tracks; got {tids:?}");
    // Per-GoP: the two tracks of GoP 0 are adjacent, then GoP 1 restarts at
    // track 0. Per-track grouping would instead repeat the first id.
    assert_ne!(tids[0], tids[1], "GoP 0's tracks must be adjacent (per-GoP), got {tids:?}");
    assert_eq!(tids[0], tids[2], "GoP 1 must restart at track 0 (per-GoP), got {tids:?}");
    assert_eq!(tids[1], tids[3], "per-GoP cycle expected, got {tids:?}");
}

/// `segment --fmp4` from a *file* reads with random access and so must accept a
/// flat (faststart) input — the capability that kept the `fmp4` command around.
/// The streaming `--fmp4` path (stdin/live) sees no moofs in a flat file and
/// would emit an empty init; the random-access path mints real segments.
#[test]
fn segment_fmp4_accepts_flat_input() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.fmp4");
    cmd_segment(SegmentArgs {
        input: fixture_path("h264-aac.mp4").to_str().unwrap().to_string(),
        dir: None,
        fmp4: Some(out.clone()),
        stdout: false,
        flat: None,
        remap_track: vec![],
    })
    .unwrap();

    // The output is a MUXL fMP4 whose canonical segments are recoverable.
    let bytes = std::fs::read(&out).unwrap();
    let segs = muxl::reader::unwrap(&bytes).unwrap();
    assert!(
        !segs.is_empty(),
        "segment --fmp4 of a flat MP4 must yield segments, not just an empty init"
    );
    let tracks: std::collections::BTreeSet<u32> = segs.iter().map(|s| s.track_id).collect();
    assert!(tracks.len() >= 2, "expected video + audio tracks, got {tracks:?}");
}

/// `segment --fmp4` from a file (random access — the path that replaced the old
/// `fmp4` command, and that the Go `Canonicalize` binding now invokes) must be
/// byte-identical to the streaming path the stdin/live route uses. A fragmented
/// input mints the same canonical fMP4 whether read from a seekable file or a
/// pipe — so the file and stdin routes can never silently diverge.
#[test]
fn segment_fmp4_file_matches_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let frag = fixture_path("h264-opus-frag.mp4");

    // (a) `segment --fmp4` from a file — random-access read.
    let via_segment = dir.path().join("segment.fmp4");
    cmd_segment(SegmentArgs {
        input: frag.to_str().unwrap().to_string(),
        dir: None,
        fmp4: Some(via_segment.clone()),
        stdout: false,
        flat: None,
        remap_track: vec![],
    })
    .unwrap();

    // (b) the streaming path — reassemble init + per-GoP segments exactly as
    // the stdin route (cmd_segment_fmp4_stream) does.
    let raw = std::fs::read(&frag).unwrap();
    let mut gops = Vec::new();
    let catalog = muxl::segment_fmp4(&mut Cursor::new(&raw), |gop| {
        gops.push(gop);
        Ok(())
    })
    .unwrap();
    let mut streamed = muxl::fmp4::init_segment(&catalog).unwrap();
    for gop in &gops {
        for data in gop.tracks.values() {
            streamed.extend_from_slice(data);
        }
    }

    let a = std::fs::read(&via_segment).unwrap();
    assert_eq!(
        a, streamed,
        "segment --fmp4 (file/random-access) must mint identical fMP4 bytes to the streaming path"
    );
}
