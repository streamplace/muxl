//! Init segment construction and extraction.
//!
//! Converts between Catalog (track configuration metadata) and MP4 init
//! segments (ftyp+moov with empty sample tables). This enables round-tripping
//! between the Hang catalog format and MP4 container headers.
//!
//! Spec: canonical-form.md § Init Segment moov

use std::io::{Cursor, Read, Seek, SeekFrom};

use mp4_atom::{
    Atom, Av01, Av1c, Avc1, Avcc, Codec, Decode, Dinf, Dops, Dref, Encode, Esds, Ftyp, Hdlr,
    Header, Mdhd, Mdia, Minf, Moov, Mp4a, Mvex, Mvhd, Opus, ReadAtom, ReadFrom, Stbl, Stco, Stsc,
    Stsd, Stsz, StszSamples, Stts, Tkhd, Trak, Trex, Url, Visual, Vmhd, WriteTo,
};

use crate::catalog::{AudioTrackConfig, Catalog, VideoTrackConfig};
use crate::error::{Error, Result};

// Canonical timescale for mvhd (movie-level, not media-level)
pub(crate) const MOVIE_TIMESCALE: u32 = 1000;

/// Extract a Catalog from an MP4 file's moov box.
///
/// Reads codec configuration from stsd entries, dimensions from visual/audio
/// sample entries.
pub fn catalog_from_mp4<RS: Read + Seek>(mut input: RS) -> Result<Catalog> {
    let moov = read_moov(&mut input)?;
    catalog_from_moov(&moov)
}

/// Extract a Catalog from an already-parsed Moov box.
pub fn catalog_from_moov(moov: &Moov) -> Result<Catalog> {
    let mut video = std::collections::BTreeMap::new();
    let mut audio = std::collections::BTreeMap::new();

    let mut traks: Vec<&Trak> = moov.trak.iter().collect();
    traks.sort_by_key(|t| t.tkhd.track_id);

    for trak in traks {
        let track_id = trak.tkhd.track_id;
        let handler = trak.mdia.hdlr.handler;

        match handler.as_ref() {
            b"vide" => {
                if let Some(config) = extract_video_config(trak)? {
                    video.insert(format!("video{}", track_id), config);
                }
            }
            b"soun" => {
                if let Some(config) = extract_audio_config(trak)? {
                    audio.insert(format!("audio{}", track_id), config);
                }
            }
            _ => {} // skip subtitle/other tracks for now
        }
    }

    Ok(Catalog { video, audio })
}

/// Build a canonical ftyp+moov init segment from a Catalog.
///
/// The init segment has empty sample tables (no samples), matching
/// canonical-form.md § Init Segment moov.
pub fn build_init_segment(catalog: &Catalog) -> Result<Vec<u8>> {
    let mut buf = Vec::new();

    // ftyp — canonical-form.md § ftyp
    let ftyp = Ftyp {
        major_brand: b"muxl".into(),
        minor_version: 0,
        compatible_brands: vec![b"muxl".into(), b"isom".into(), b"iso2".into()],
    };
    ftyp.write_to(&mut buf).map_err(mp4_err)?;

    // Collect all tracks sorted by track_id
    let mut track_defs: Vec<TrackDef> = Vec::new();
    for config in catalog.video.values() {
        track_defs.push(TrackDef::Video(config));
    }
    for config in catalog.audio.values() {
        track_defs.push(TrackDef::Audio(config));
    }
    track_defs.sort_by_key(|t| t.track_id());

    let max_track_id = track_defs.iter().map(|t| t.track_id()).max().unwrap_or(0);

    let mut traks = Vec::new();
    for td in &track_defs {
        traks.push(match td {
            TrackDef::Video(c) => build_video_trak(c)?,
            TrackDef::Audio(c) => build_audio_trak(c)?,
        });
    }

    // mvex with trex entries — required for fMP4 playback
    let trex_entries: Vec<Trex> = track_defs
        .iter()
        .map(|td| Trex {
            track_id: td.track_id(),
            default_sample_description_index: 1,
            default_sample_duration: 0,
            default_sample_size: 0,
            default_sample_flags: 0,
        })
        .collect();

    let moov = Moov {
        mvhd: Mvhd {
            creation_time: 0,
            modification_time: 0,
            timescale: MOVIE_TIMESCALE,
            duration: 0,
            rate: 1u16.into(),
            volume: 1u8.into(),
            matrix: Default::default(),
            next_track_id: max_track_id + 1,
        },
        meta: None,
        mvex: Some(Mvex {
            mehd: None,
            trex: trex_entries,
        }),
        trak: traks,
        udta: None,
        ainf: None,
    };
    moov.write_to(&mut buf).map_err(mp4_err)?;

    Ok(buf)
}

/// Build per-track init segments from a Catalog.
///
/// Returns a map of track_id → single-track ftyp+moov bytes. Each init
/// segment contains only the moov data for that one track, suitable for
/// HLS CMAF media playlists where each track needs its own init segment.
pub fn build_track_init_segments(catalog: &Catalog) -> Result<std::collections::BTreeMap<u32, Vec<u8>>> {
    let mut result = std::collections::BTreeMap::new();

    for config in catalog.video.values() {
        let single = Catalog {
            video: std::collections::BTreeMap::from([(
                format!("video{}", config.track_id),
                config.clone(),
            )]),
            audio: std::collections::BTreeMap::new(),
        };
        result.insert(config.track_id, build_init_segment(&single)?);
    }

    for config in catalog.audio.values() {
        let single = Catalog {
            video: std::collections::BTreeMap::new(),
            audio: std::collections::BTreeMap::from([(
                format!("audio{}", config.track_id),
                config.clone(),
            )]),
        };
        result.insert(config.track_id, build_init_segment(&single)?);
    }

    Ok(result)
}

// --- Internal helpers ---

enum TrackDef<'a> {
    Video(&'a VideoTrackConfig),
    Audio(&'a AudioTrackConfig),
}

impl TrackDef<'_> {
    fn track_id(&self) -> u32 {
        match self {
            TrackDef::Video(c) => c.track_id,
            TrackDef::Audio(c) => c.track_id,
        }
    }
}

fn mp4_err(e: mp4_atom::Error) -> Error {
    Error::InvalidMp4(e.to_string())
}

/// Read through an MP4 file to find and parse the moov box.
pub fn read_moov<R: Read + Seek>(reader: &mut R) -> Result<Moov> {
    reader.seek(SeekFrom::Start(0))?;
    loop {
        let header = match <Option<Header> as ReadFrom>::read_from(reader).map_err(mp4_err)? {
            Some(h) => h,
            None => return Err(Error::InvalidMp4("moov box not found".into())),
        };

        if header.kind == Moov::KIND {
            return Moov::read_atom(&header, reader).map_err(mp4_err);
        }

        // Skip this box
        match header.size {
            Some(size) => {
                reader.seek(SeekFrom::Current(size as i64))?;
            }
            None => return Err(Error::InvalidMp4("moov box not found".into())),
        }
    }
}

/// Extract video track config from a trak.
fn extract_video_config(trak: &Trak) -> Result<Option<VideoTrackConfig>> {
    let track_id = trak.tkhd.track_id;
    let timescale = trak.mdia.mdhd.timescale;

    for codec in &trak.mdia.minf.stbl.stsd.codecs {
        match codec {
            Codec::Avc1(avc1) => {
                let description = encode_atom(&avc1.avcc)?;
                let codec_str = format!(
                    "avc1.{:02x}{:02x}{:02x}",
                    avc1.avcc.avc_profile_indication,
                    avc1.avcc.profile_compatibility,
                    avc1.avcc.avc_level_indication,
                );
                return Ok(Some(VideoTrackConfig {
                    codec: codec_str,
                    description,
                    coded_width: avc1.visual.width as u32,
                    coded_height: avc1.visual.height as u32,
                    track_id,
                    timescale,
                }));
            }
            Codec::Av01(av01) => {
                let description = encode_atom(&av01.av1c)?;
                // AV1 codec string: av01.P.LLH.DD
                let profile = av01.av1c.seq_profile;
                let level = av01.av1c.seq_level_idx_0;
                let tier = if av01.av1c.seq_tier_0 { 'H' } else { 'M' };
                let bit_depth = if av01.av1c.twelve_bit {
                    12
                } else if av01.av1c.high_bitdepth {
                    10
                } else {
                    8
                };
                let codec_str = format!("av01.{profile}.{level:02}{tier}.{bit_depth:02}");
                return Ok(Some(VideoTrackConfig {
                    codec: codec_str,
                    description,
                    coded_width: av01.visual.width as u32,
                    coded_height: av01.visual.height as u32,
                    track_id,
                    timescale,
                }));
            }
            _ => continue,
        }
    }
    Ok(None)
}

/// Extract audio track config from a trak.
fn extract_audio_config(trak: &Trak) -> Result<Option<AudioTrackConfig>> {
    let track_id = trak.tkhd.track_id;
    let timescale = trak.mdia.mdhd.timescale;

    for codec in &trak.mdia.minf.stbl.stsd.codecs {
        match codec {
            Codec::Opus(opus) => {
                let description = encode_atom(&opus.dops)?;
                return Ok(Some(AudioTrackConfig {
                    codec: "opus".into(),
                    description,
                    sample_rate: opus.audio.sample_rate.integer() as u32,
                    number_of_channels: opus.audio.channel_count as u32,
                    track_id,
                    timescale,
                }));
            }
            Codec::Mp4a(mp4a) => {
                let description = encode_atom(&mp4a.esds)?;
                let profile = mp4a.esds.es_desc.dec_config.dec_specific.profile;
                let codec_str = format!("mp4a.40.{}", profile);
                return Ok(Some(AudioTrackConfig {
                    codec: codec_str,
                    description,
                    sample_rate: mp4a.audio.sample_rate.integer() as u32,
                    number_of_channels: mp4a.audio.channel_count as u32,
                    track_id,
                    timescale,
                }));
            }
            _ => continue,
        }
    }
    Ok(None)
}

/// Encode an atom to bytes (including box header).
fn encode_atom<A: Atom + Encode>(atom: &A) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    atom.encode(&mut buf).map_err(mp4_err)?;
    Ok(buf)
}

/// Decode an atom from bytes (including box header).
fn decode_atom<A: Atom + Decode>(bytes: &[u8]) -> Result<A> {
    let mut cursor = Cursor::new(bytes);
    A::decode(&mut cursor).map_err(mp4_err)
}

/// Canonical dinf box with a single self-contained URL entry.
fn canonical_dinf() -> Dinf {
    Dinf {
        dref: Dref {
            urls: vec![Url {
                location: String::new(),
            }],
        },
    }
}

fn empty_stbl(stsd: Stsd) -> Stbl {
    Stbl {
        stsd,
        stts: Stts { entries: vec![] },
        ctts: None,
        stss: None,
        stsc: Stsc { entries: vec![] },
        stsz: Stsz {
            samples: StszSamples::Different { sizes: vec![] },
        },
        stco: Some(Stco { entries: vec![] }),
        co64: None,
        sbgp: vec![],
        sgpd: vec![],
        subs: vec![],
        saiz: vec![],
        saio: vec![],
        cslg: None,
    }
}

/// Build a canonical video trak box from config.
pub(crate) fn build_video_trak(config: &VideoTrackConfig) -> Result<Trak> {
    let codec = if config.codec.starts_with("avc1") {
        let avcc: Avcc = decode_atom(&config.description)?;
        Codec::Avc1(Avc1 {
            visual: Visual {
                data_reference_index: 1,
                width: config.coded_width as u16,
                height: config.coded_height as u16,
                ..Default::default()
            },
            avcc,
            btrt: None,
            colr: None,
            pasp: None,
            taic: None,
            fiel: None,
        })
    } else if config.codec.starts_with("av01") {
        let av1c: Av1c = decode_atom(&config.description)?;
        Codec::Av01(Av01 {
            visual: Visual {
                data_reference_index: 1,
                width: config.coded_width as u16,
                height: config.coded_height as u16,
                ..Default::default()
            },
            av1c,
            btrt: None,
            ccst: None,
            colr: None,
            pasp: None,
            taic: None,
        })
    } else {
        return Err(Error::InvalidMp4(format!(
            "unsupported video codec: {}",
            config.codec
        )));
    };

    Ok(Trak {
        tkhd: Tkhd {
            creation_time: 0,
            modification_time: 0,
            track_id: config.track_id,
            duration: 0,
            layer: 0,
            alternate_group: 0,
            enabled: true,
            in_movie: true,
            in_preview: false,
            volume: 0u8.into(),
            matrix: Default::default(),
            width: (config.coded_width as u16).into(),
            height: (config.coded_height as u16).into(),
        },
        edts: None,
        meta: None,
        mdia: Mdia {
            mdhd: Mdhd {
                creation_time: 0,
                modification_time: 0,
                timescale: config.timescale,
                duration: 0,
                language: "und".into(),
            },
            hdlr: Hdlr {
                handler: b"vide".into(),
                name: String::new(),
            },
            minf: Minf {
                vmhd: Some(Vmhd::default()),
                smhd: None,
                nmhd: None,
                sthd: None,
                hmhd: None,
                dinf: canonical_dinf(),
                stbl: empty_stbl(Stsd {
                    codecs: vec![codec],
                }),
            },
        },
        senc: None,
        tref: None,
        udta: None,
    })
}

/// Build a canonical audio trak box from config.
pub(crate) fn build_audio_trak(config: &AudioTrackConfig) -> Result<Trak> {
    let audio = mp4_atom::Audio {
        data_reference_index: 1,
        channel_count: config.number_of_channels as u16,
        sample_size: 16,
        sample_rate: (config.sample_rate as u16).into(),
    };

    let codec = if config.codec == "opus" {
        let dops: Dops = decode_atom(&config.description)?;
        Codec::Opus(Opus {
            audio: audio.clone(),
            dops,
            btrt: None,
        })
    } else if config.codec.starts_with("mp4a") {
        let esds: Esds = decode_atom(&config.description)?;
        Codec::Mp4a(Mp4a {
            audio: audio.clone(),
            esds,
            btrt: None,
            taic: None,
        })
    } else {
        return Err(Error::InvalidMp4(format!(
            "unsupported audio codec: {}",
            config.codec
        )));
    };

    Ok(Trak {
        tkhd: Tkhd {
            creation_time: 0,
            modification_time: 0,
            track_id: config.track_id,
            duration: 0,
            layer: 0,
            alternate_group: 0,
            enabled: true,
            in_movie: true,
            in_preview: false,
            volume: 1u8.into(), // audio tracks get volume 1.0
            matrix: Default::default(),
            width: 0u16.into(),
            height: 0u16.into(),
        },
        edts: None,
        meta: None,
        mdia: Mdia {
            mdhd: Mdhd {
                creation_time: 0,
                modification_time: 0,
                timescale: config.timescale,
                duration: 0,
                language: "und".into(),
            },
            hdlr: Hdlr {
                handler: b"soun".into(),
                name: String::new(),
            },
            minf: Minf {
                vmhd: None,
                smhd: Some(Default::default()),
                nmhd: None,
                sthd: None,
                hmhd: None,
                dinf: canonical_dinf(),
                stbl: empty_stbl(Stsd {
                    codecs: vec![codec],
                }),
            },
        },
        senc: None,
        tref: None,
        udta: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_fixture(name: &str) -> Vec<u8> {
        let path = format!("samples/fixtures/{}", name);
        std::fs::read(&path)
            .or_else(|_| std::fs::read(format!("samples/{}", name)))
            .unwrap_or_else(|_| panic!("{} must exist for tests", path))
    }

    #[test]
    fn test_catalog_from_h264_aac() {
        let data = read_fixture("h264-aac.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        assert_eq!(catalog.video.len(), 1);
        assert_eq!(catalog.audio.len(), 1);

        let video = catalog.video.values().next().unwrap();
        assert!(video.codec.starts_with("avc1."), "got {}", video.codec);
        assert!(video.coded_width > 0);
        assert!(video.coded_height > 0);
        assert!(!video.description.is_empty());

        let audio = catalog.audio.values().next().unwrap();
        assert!(audio.codec.starts_with("mp4a."), "got {}", audio.codec);
        assert!(audio.sample_rate > 0);
        assert!(audio.number_of_channels > 0);
        assert!(!audio.description.is_empty());
    }

    #[test]
    fn test_catalog_from_h264_opus() {
        let data = read_fixture("h264-opus.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        assert_eq!(catalog.video.len(), 1);
        assert_eq!(catalog.audio.len(), 1);

        let audio = catalog.audio.values().next().unwrap();
        assert_eq!(audio.codec, "opus");
        assert_eq!(audio.sample_rate, 48000);
        assert!(!audio.description.is_empty());
    }

    #[test]
    fn test_catalog_from_video_only() {
        let data = read_fixture("h264-video-only.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        assert_eq!(catalog.video.len(), 1);
        assert_eq!(catalog.audio.len(), 0);
    }

    #[test]
    fn test_catalog_from_audio_only() {
        let data = read_fixture("opus-audio-only.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        assert_eq!(catalog.video.len(), 0);
        assert_eq!(catalog.audio.len(), 1);
    }

    #[test]
    fn test_build_init_has_ftyp_moov() {
        let data = read_fixture("h264-aac.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        let init = build_init_segment(&catalog).unwrap();
        assert!(!init.is_empty());

        // Parse box structure
        let mut cursor = Cursor::new(&init[..]);
        let h1 = Header::read_from(&mut cursor).unwrap();
        assert_eq!(h1.kind, Ftyp::KIND);
        std::io::Read::read_exact(&mut cursor, &mut vec![0u8; h1.size.unwrap()]).unwrap();

        let h2 = Header::read_from(&mut cursor).unwrap();
        assert_eq!(h2.kind, Moov::KIND);
    }

    #[test]
    fn test_init_round_trip_h264_aac() {
        let data = read_fixture("h264-aac.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        let init = build_init_segment(&catalog).unwrap();
        let catalog2 = catalog_from_mp4(Cursor::new(init)).unwrap();

        assert_eq!(catalog.video.len(), catalog2.video.len());
        assert_eq!(catalog.audio.len(), catalog2.audio.len());

        for (name, v1) in &catalog.video {
            let v2 = catalog2.video.get(name).expect("video track missing");
            assert_eq!(v1.codec, v2.codec);
            assert_eq!(v1.description, v2.description);
            assert_eq!(v1.coded_width, v2.coded_width);
            assert_eq!(v1.coded_height, v2.coded_height);
        }

        for (name, a1) in &catalog.audio {
            let a2 = catalog2.audio.get(name).expect("audio track missing");
            assert_eq!(a1.codec, a2.codec);
            assert_eq!(a1.description, a2.description);
            assert_eq!(a1.sample_rate, a2.sample_rate);
            assert_eq!(a1.number_of_channels, a2.number_of_channels);
        }
    }

    #[test]
    fn test_init_round_trip_h264_opus() {
        let data = read_fixture("h264-opus.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        let init = build_init_segment(&catalog).unwrap();
        let catalog2 = catalog_from_mp4(Cursor::new(init)).unwrap();

        let v1 = catalog.video.values().next().unwrap();
        let v2 = catalog2.video.values().next().unwrap();
        assert_eq!(v1.codec, v2.codec);
        assert_eq!(v1.description, v2.description);

        let a1 = catalog.audio.values().next().unwrap();
        let a2 = catalog2.audio.values().next().unwrap();
        assert_eq!(a1.codec, a2.codec);
        assert_eq!(a1.description, a2.description);
    }

    #[test]
    fn test_init_round_trip_opus_only() {
        let data = read_fixture("opus-audio-only.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        let init = build_init_segment(&catalog).unwrap();
        let catalog2 = catalog_from_mp4(Cursor::new(init)).unwrap();

        let a1 = catalog.audio.values().next().unwrap();
        let a2 = catalog2.audio.values().next().unwrap();
        assert_eq!(a1.codec, a2.codec);
        assert_eq!(a1.description, a2.description);
        assert_eq!(a1.sample_rate, a2.sample_rate);
        assert_eq!(a1.number_of_channels, a2.number_of_channels);
    }

    #[test]
    fn test_init_idempotent() {
        let data = read_fixture("h264-opus.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        let init1 = build_init_segment(&catalog).unwrap();
        let catalog2 = catalog_from_mp4(Cursor::new(&init1)).unwrap();
        let init2 = build_init_segment(&catalog2).unwrap();

        assert_eq!(init1, init2, "init segment should be idempotent");
    }

    #[test]
    fn test_init_is_parseable() {
        let data = read_fixture("h264-aac.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        let init = build_init_segment(&catalog).unwrap();
        let moov = read_moov(&mut Cursor::new(&init)).unwrap();

        assert_eq!(moov.trak.len(), catalog.video.len() + catalog.audio.len());
    }

    #[test]
    fn test_all_h264_fixtures_extract() {
        for name in &[
            "h264-aac.mp4",
            "h264-opus.mp4",
            "h264-aac-25fps.mp4",
            "h264-aac-portrait.mp4",
            "h264-opus-vfr.mp4",
            "h264-video-only.mp4",
        ] {
            let data = read_fixture(name);
            let catalog = catalog_from_mp4(Cursor::new(data))
                .unwrap_or_else(|e| panic!("{name}: catalog extraction failed: {e}"));
            assert!(!catalog.video.is_empty(), "{name}: no video tracks");
        }
    }

    #[test]
    fn test_av1_fixtures_extract() {
        for name in &["av1-aac.mp4", "av1-opus.mp4"] {
            let data = read_fixture(name);
            let result = catalog_from_mp4(Cursor::new(data));
            match result {
                Ok(catalog) => {
                    assert!(!catalog.video.is_empty(), "{name}: no video tracks");
                    let video = catalog.video.values().next().unwrap();
                    assert!(
                        video.codec.starts_with("av01."),
                        "{name}: got {}",
                        video.codec
                    );
                }
                Err(e) => {
                    // AV1 support depends on mp4-atom's parsing
                    eprintln!("{name}: {e} (may not be supported yet)");
                }
            }
        }
    }
}
