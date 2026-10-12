//! Audio metadata is opt-in on source reads and authoritative in catalogs.
use std::collections::BTreeMap;
use std::io::Cursor;

use muxl::catalog::{Catalog, TrackMeta, TrackOptions};

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/samples/fixtures/h264-opus-frag.mp4"
    ))
    .unwrap()
}

fn options() -> TrackOptions {
    TrackOptions {
        remap: BTreeMap::from([(2, 9)]),
        track_meta: BTreeMap::from([(
            9,
            TrackMeta {
                alternate_group: 2,
                label: "Music: 音楽".into(),
            },
        )]),
        ..Default::default()
    }
}

fn check_catalog(catalog: &Catalog, id: u32, group: u32, label: &str) {
    let audio = catalog
        .audio_configs()
        .find(|a| a.track_id() == id)
        .unwrap();
    assert_eq!(audio.alternate_group, Some(group));
    assert_eq!(audio.label.as_deref(), Some(label));
}

fn source_catalog(bytes: &[u8]) -> Catalog {
    muxl::catalog::from_input_with_options(
        bytes,
        &TrackOptions {
            source_track_meta: true,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn explicit_metadata_reaches_every_reader_init_and_segment() {
    let bytes = fixture();
    let mut pull_segments = Vec::new();
    let catalog = muxl::segment_fmp4_with_options(&mut Cursor::new(&bytes), options(), |gop| {
        pull_segments.extend(gop.tracks.into_values());
        Ok(())
    })
    .unwrap();
    check_catalog(&catalog, 9, 2, "Music: 音楽");

    let mut push = muxl::Segmenter::with_options(options());
    let mut events = Vec::new();
    for chunk in bytes.chunks(113) {
        events.extend(push.feed(chunk).unwrap());
    }
    events.extend(push.flush().unwrap());
    let mut push_segments = Vec::new();
    for event in events {
        match event {
            muxl::SegmenterEvent::InitSegment { catalog: c, data } => {
                assert_eq!(c, catalog);
                assert_eq!(source_catalog(&data), catalog);
            }
            muxl::SegmenterEvent::Segment(gop) => push_segments.extend(gop.tracks.into_values()),
        }
    }
    assert_eq!(push_segments, pull_segments);
    for bytes in &pull_segments {
        let single = muxl::catalog::from_segment(bytes).unwrap();
        if single.audio.is_some() {
            check_catalog(&single, 9, 2, "Music: 音楽");
        }
    }

    let init = muxl::init::build_init_segment(&catalog).unwrap();
    let moov = muxl::init::read_moov(&mut Cursor::new(&init)).unwrap();
    let audio = moov.trak.iter().find(|t| t.tkhd.track_id == 9).unwrap();
    assert_eq!(audio.tkhd.alternate_group, 2);
    // Exactly one plain name box; no full-box fields or terminator.
    let payload = "Music: 音楽".as_bytes();
    let mut udta = Vec::new();
    udta.extend_from_slice(&((16 + payload.len()) as u32).to_be_bytes());
    udta.extend_from_slice(b"udta");
    udta.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    udta.extend_from_slice(b"name");
    udta.extend_from_slice(payload);
    assert!(init.windows(udta.len()).any(|w| w == udta));
    assert!(
        moov.trak
            .iter()
            .filter(|t| t.tkhd.track_id != 9)
            .all(|t| t.udta.is_none())
    );

    // Random-access canonicalization uses the same metadata after remapping.
    let source = muxl::flat::read_with_options(&bytes, &options()).unwrap();
    assert_eq!(source.catalog, catalog);
    let mut canonical = Vec::new();
    muxl::fmp4::write(&source, &bytes, &mut canonical).unwrap();
    assert_eq!(source_catalog(&canonical), catalog);

    // Every catalog-derived presentation path retains metadata without opt-in.
    let recovered = muxl::reader::unwrap(&canonical).unwrap();
    let aggregate = muxl::reader::aggregate_catalog(&recovered);
    assert_eq!(aggregate, catalog);
    let segments: Vec<&[u8]> = recovered.iter().map(|s| s.data).collect();
    let mut wrapped = Vec::new();
    muxl::present::write_fmp4(&aggregate, segments.iter().copied(), &mut wrapped).unwrap();
    assert_eq!(source_catalog(&wrapped), catalog);
    let mut flat = Vec::new();
    muxl::present::write_flat_from_m4s(&aggregate, &segments, &mut flat).unwrap();
    assert_eq!(source_catalog(&flat), catalog);
    let metas: Vec<_> = segments
        .iter()
        .map(|s| muxl::metafile::segment_metafile(s).unwrap())
        .collect();
    let header = muxl::metafile::synthesize_flat_header(&metas).unwrap();
    assert_eq!(source_catalog(&header.bytes), catalog);
    for event in muxl::reader::segment_events(&flat).unwrap() {
        if let muxl::cbor::CborEvent::Init {
            catalog: Some(c),
            data,
            ..
        } = event
        {
            check_catalog(&c, 9, 2, "Music: 音楽");
            assert_eq!(source_catalog(&data), c);
        }
    }
}

#[test]
fn ffmpeg_source_metadata_is_opt_in_and_roundtrips_through_wrap() {
    // Generated once by ffmpeg with two AAC sine tracks and
    // -metadata:s:a:1 title=Music -movflags +frag_keyframe+empty_moov.
    // Keep this alongside the other samples so tests need no ffmpeg binary.
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/samples/fixtures/aac-two-track-labeled-frag.mp4"
    ))
    .unwrap();
    let plain = muxl::catalog::from_input(&bytes).unwrap();
    assert!(
        plain
            .audio_configs()
            .all(|a| a.alternate_group.is_none() && a.label.is_none())
    );
    let source = source_catalog(&bytes);
    assert_eq!(source.audio_configs().count(), 2);
    // ffmpeg sets alternate_group=1 on both audio tracks.
    assert!(source.audio_configs().all(|a| a.alternate_group == Some(1)));
    assert_eq!(
        source
            .audio_configs()
            .find(|a| a.track_id() == 1)
            .unwrap()
            .label,
        None
    );
    check_catalog(&source, 2, 1, "Music");

    let opts = TrackOptions {
        source_track_meta: true,
        ..Default::default()
    };
    let mut segments = Vec::new();
    let catalog = muxl::segment_fmp4_with_options(&mut Cursor::new(&bytes), opts.clone(), |g| {
        segments.extend(g.tracks.into_values());
        Ok(())
    })
    .unwrap();
    assert_eq!(catalog, source);
    let mut wrapped = Vec::new();
    muxl::present::write_fmp4(&catalog, segments.iter().map(Vec::as_slice), &mut wrapped).unwrap();
    assert_eq!(source_catalog(&wrapped), source);
    let reminted = muxl::flat::read_with_options(&wrapped, &opts).unwrap();
    assert_eq!(reminted.catalog, source);

    let mut explicit = opts;
    explicit.track_meta.insert(2, TrackMeta::default());
    let cleared = muxl::flat::read_with_options(&bytes, &explicit).unwrap();
    let audio = cleared
        .catalog
        .audio_configs()
        .find(|a| a.track_id() == 2)
        .unwrap();
    assert_eq!(audio.alternate_group, None);
    assert_eq!(audio.label, None);
}

#[test]
fn metadata_wire_keys_and_invalid_tracks() {
    let bytes = fixture();
    let mut catalog = muxl::catalog::from_input(&bytes).unwrap();
    let json = muxl::catalog::to_hang_json(&catalog).unwrap();
    assert!(!json.contains("alternateGroup"));
    assert!(!json.contains("label"));
    for id in [1, 404] {
        assert!(
            catalog
                .set_track_meta(&BTreeMap::from([(id, TrackMeta::default())]))
                .is_err()
        );
    }
    assert!(
        catalog
            .set_track_meta(&BTreeMap::from([(
                2,
                TrackMeta {
                    alternate_group: 65536,
                    label: String::new()
                }
            )]))
            .is_err()
    );
    catalog
        .set_track_meta(&BTreeMap::from([(
            2,
            TrackMeta {
                alternate_group: 3,
                label: "Music".into(),
            },
        )]))
        .unwrap();
    let json = muxl::catalog::to_hang_json(&catalog).unwrap();
    assert!(json.contains("\"alternateGroup\": 3"));
    assert!(json.contains("\"label\": \"Music\""));
    assert_eq!(muxl::catalog::from_hang_json(&json).unwrap(), catalog);
    let cbor = muxl::catalog::to_drisl(&catalog).unwrap();
    assert!(cbor.windows(14).any(|w| w == b"alternateGroup"));
    assert!(cbor.windows(5).any(|w| w == b"label"));
    assert_eq!(muxl::catalog::from_drisl(&cbor).unwrap(), catalog);
}

#[test]
fn default_catalog_matches_v036_wire_bytes() {
    // Captured from the existing h264-opus-frag sample before these changes.
    let expected = concat!(
        "a265617564696fa16a72656e646974696f6e73a166617564696f32a565636f64",
        "6563646f70757369636f6e7461696e6572a3646b696e6464636d616667747261",
        "636b4964026974696d657363616c6519bb806a73616d706c655261746519bb80",
        "6b6465736372697074696f6e5300000013644f7073000101380000bb80000000",
        "706e756d6265724f664368616e6e656c730165766964656fa16a72656e646974",
        "696f6e73a166766964656f31a565636f6465636b617663312e34326330306469",
        "636f6e7461696e6572a3646b696e6464636d616667747261636b496401697469",
        "6d657363616c65193c006a636f64656457696474681901406b636f6465644865",
        "6967687418f06b6465736372697074696f6e582d0000002d617663430142c00d",
        "ffe100166742c00dda0507ec0440000003004000000f03c50aa801000468ce09",
        "c8",
    );
    let catalog = muxl::catalog::from_input(&fixture()).unwrap();
    assert_eq!(
        hex::encode(muxl::catalog::to_drisl(&catalog).unwrap()),
        expected
    );
}

#[test]
fn ignored_source_metadata_does_not_validate_utf8() {
    use mp4_atom::WriteTo;
    let bytes = fixture();
    let expected = muxl::catalog::from_input(&bytes).unwrap();
    let mut moov = muxl::init::read_moov(&mut Cursor::new(&bytes)).unwrap();
    moov.trak
        .iter_mut()
        .find(|t| t.tkhd.track_id == 2)
        .unwrap()
        .udta = Some(mp4_atom::Udta {
        name: Some(mp4_atom::TrackName { value: vec![0xff] }),
        ..Default::default()
    });
    let mut init = Vec::new();
    moov.write_to(&mut init).unwrap();
    assert_eq!(muxl::catalog::from_input(&init).unwrap(), expected);
    assert!(
        muxl::catalog::from_input_with_options(
            &init,
            &TrackOptions {
                source_track_meta: true,
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn zero_and_empty_metadata_are_unset_on_every_catalog_boundary() {
    use muxl::catalog::AudioConfig;
    let baseline = muxl::catalog::from_input(&fixture()).unwrap();
    let mut explicit = baseline.clone();
    for audio in explicit.audio_configs_mut() {
        audio.alternate_group = Some(0);
        audio.label = Some(String::new());
    }
    assert_eq!(
        muxl::catalog::to_drisl(&explicit).unwrap(),
        muxl::catalog::to_drisl(&baseline).unwrap()
    );
    assert_eq!(
        muxl::catalog::to_hang_json(&explicit).unwrap(),
        muxl::catalog::to_hang_json(&baseline).unwrap()
    );
    let init = muxl::init::build_init_segment(&explicit).unwrap();
    assert_eq!(init, muxl::init::build_init_segment(&baseline).unwrap());
    let moov = muxl::init::read_moov(&mut Cursor::new(init)).unwrap();
    assert!(
        moov.trak
            .iter()
            .all(|t| t.udta.is_none() && t.tkhd.alternate_group == 0)
    );

    // Produce explicit zero/empty wire fields without AudioConfig's omission
    // rules, exercising JSON and CBOR inputs from external callers.
    #[derive(serde::Serialize)]
    struct ExplicitAudio<'a> {
        #[serde(flatten)]
        config: &'a AudioConfig,
        #[serde(rename = "alternateGroup")]
        alternate_group: u32,
        label: &'a str,
    }
    let audio = baseline.audio_configs().next().unwrap();
    let wire = ExplicitAudio {
        config: audio,
        alternate_group: 0,
        label: "",
    };
    let json = serde_json::to_string(&wire).unwrap();
    let from_json: AudioConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(&from_json, audio);
    let mut cbor = Vec::new();
    dasl::drisl::to_writer(&mut cbor, &wire).unwrap();
    let from_cbor: AudioConfig = dasl::drisl::from_slice(&cbor).unwrap();
    assert_eq!(&from_cbor, audio);
}
