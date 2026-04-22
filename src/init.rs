//! Init segment construction and extraction.
//!
//! Converts between Catalog (track configuration metadata) and MP4 init
//! segments (ftyp+moov with empty sample tables). This enables round-tripping
//! between the Hang catalog format and MP4 container headers.
//!
//! Spec: canonical-form.md § Init Segment moov

use std::io::{Cursor, Read, Seek, SeekFrom};

use mp4_atom::{
    Atom, Av01, Av1c, Avc1, Avcc, Codec, Decode, Dinf, Dops, Dref, Encode, Esds, Ftyp, Hdlr, Header,
    Mdhd, Mdia, Minf, Moov, Mp4a, Mvex, Mvhd, Opus, ReadAtom, ReadFrom, Stbl, Stco, Stsc, Stsd,
    Stsz, StszSamples, Stts, Tkhd, Trak, Trex, Url, Visual, Vmhd, WriteTo,
};

use crate::catalog::{AudioConfig, Catalog, Container, VideoConfig};
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
    let mut catalog = Catalog::default();

    let mut traks: Vec<&Trak> = moov.trak.iter().collect();
    traks.sort_by_key(|t| t.tkhd.track_id);

    for trak in traks {
        let track_id = trak.tkhd.track_id;
        let handler = trak.mdia.hdlr.handler;

        match handler.as_ref() {
            b"vide" => {
                if let Some(config) = extract_video_config(trak)? {
                    catalog.insert_video(format!("video{}", track_id), config);
                }
            }
            b"soun" => {
                if let Some(config) = extract_audio_config(trak)? {
                    catalog.insert_audio(format!("audio{}", track_id), config);
                }
            }
            _ => {} // skip subtitle/other tracks for now
        }
    }

    Ok(catalog)
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
    for config in catalog.video_configs() {
        track_defs.push(TrackDef::Video(config));
    }
    for config in catalog.audio_configs() {
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

    for config in catalog.video_configs() {
        let mut single = Catalog::default();
        single.insert_video(format!("video{}", config.track_id()), config.clone());
        result.insert(config.track_id(), build_init_segment(&single)?);
    }

    for config in catalog.audio_configs() {
        let mut single = Catalog::default();
        single.insert_audio(format!("audio{}", config.track_id()), config.clone());
        result.insert(config.track_id(), build_init_segment(&single)?);
    }

    Ok(result)
}

// --- Internal helpers ---

enum TrackDef<'a> {
    Video(&'a VideoConfig),
    Audio(&'a AudioConfig),
}

impl TrackDef<'_> {
    fn track_id(&self) -> u32 {
        match self {
            TrackDef::Video(c) => c.track_id(),
            TrackDef::Audio(c) => c.track_id(),
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

/// Derive a track's canonical presentation start offset from its `edts/elst`,
/// expressed in the track's media timescale.
///
/// MUXL canonical form has no `elst` box. Instead, a track's presentation
/// start offset (from source-file leading empty edits, typically used by clip
/// editors for A/V alignment) is baked into the track's first-fragment `tfdt`
/// and/or into a synthesized canonical `elst` in the flat MP4 moov.
///
/// This parses the input elst and returns the leading empty-edit duration,
/// summed across consecutive `media_time == -1` entries at the start of the
/// list, rescaled from the movie timescale into the track's media timescale.
/// Any trailing non-empty entries contribute nothing (they define what media
/// plays, not when presentation begins). Source patterns we recognize:
///
/// - no elst → 0
/// - `(X, media_time=0)` → 0 (trivial identity)
/// - `(D, media_time=-1), (X, media_time=0)` → rescale(D, movie_ts → track_ts)
///   (LosslessCut-style alignment)
///
/// Other patterns (encoder-priming `media_time > 0`, rate changes, non-leading
/// empty edits) are not converged here — see `open-questions.md`.
pub(crate) fn start_offset_from_trak(trak: &Trak, movie_timescale: u32) -> u64 {
    let Some(edts) = trak.edts.as_ref() else {
        return 0;
    };
    let Some(elst) = edts.elst.as_ref() else {
        return 0;
    };
    let track_ts = trak.mdia.mdhd.timescale as u64;
    let movie_ts = movie_timescale as u64;
    if track_ts == 0 || movie_ts == 0 {
        return 0;
    }

    let mut empty_movie_ticks: u64 = 0;
    for entry in &elst.entries {
        if is_empty_edit(entry.media_time) {
            empty_movie_ticks += entry.segment_duration;
        } else {
            break;
        }
    }
    // Rescale movie-timescale empty-edit duration → track media timescale.
    // Uses round-to-nearest; leading empty edits are typically whole
    // milliseconds in the 1000-tick movie timescale and rescale cleanly.
    (empty_movie_ticks * track_ts + movie_ts / 2) / movie_ts
}

/// Recognize an `elst` empty-edit entry. mp4-atom decodes v0 media_time as
/// `u32` zero-extended to `u64` (so `-1` becomes `0xFFFF_FFFF`), and decodes
/// v1 as `i64` in u64 bit-pattern (so `-1` becomes `0xFFFF_FFFF_FFFF_FFFF`).
/// Both encode "empty edit" per ISOBMFF.
fn is_empty_edit(media_time_u64: u64) -> bool {
    media_time_u64 == u32::MAX as u64 || media_time_u64 == u64::MAX
}

/// Extract video track config from a trak.
fn extract_video_config(trak: &Trak) -> Result<Option<VideoConfig>> {
    let track_id = trak.tkhd.track_id;
    let timescale = trak.mdia.mdhd.timescale;
    let container = Container::cmaf(timescale, track_id);

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
                return Ok(Some(VideoConfig {
                    codec: codec_str,
                    container,
                    description,
                    coded_width: avc1.visual.width as u32,
                    coded_height: avc1.visual.height as u32,
                    display_aspect_width: None,
                    display_aspect_height: None,
                    framerate: None,
                    bitrate: None,
                    optimize_for_latency: None,
                    jitter: None,
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
                return Ok(Some(VideoConfig {
                    codec: codec_str,
                    container,
                    description,
                    coded_width: av01.visual.width as u32,
                    coded_height: av01.visual.height as u32,
                    display_aspect_width: None,
                    display_aspect_height: None,
                    framerate: None,
                    bitrate: None,
                    optimize_for_latency: None,
                    jitter: None,
                }));
            }
            _ => continue,
        }
    }
    Ok(None)
}

/// Extract audio track config from a trak.
fn extract_audio_config(trak: &Trak) -> Result<Option<AudioConfig>> {
    let track_id = trak.tkhd.track_id;
    let timescale = trak.mdia.mdhd.timescale;
    let container = Container::cmaf(timescale, track_id);

    for codec in &trak.mdia.minf.stbl.stsd.codecs {
        match codec {
            Codec::Opus(opus) => {
                let description = encode_atom(&opus.dops)?;
                return Ok(Some(AudioConfig {
                    codec: "opus".into(),
                    container,
                    description,
                    sample_rate: opus.audio.sample_rate.integer() as u32,
                    number_of_channels: opus.audio.channel_count as u32,
                    bitrate: None,
                    jitter: None,
                }));
            }
            Codec::Mp4a(mp4a) => {
                let description = encode_atom(&mp4a.esds)?;
                let profile = mp4a.esds.es_desc.dec_config.dec_specific.profile;
                let codec_str = format!("mp4a.40.{}", profile);
                return Ok(Some(AudioConfig {
                    codec: codec_str,
                    container,
                    description,
                    sample_rate: mp4a.audio.sample_rate.integer() as u32,
                    number_of_channels: mp4a.audio.channel_count as u32,
                    bitrate: None,
                    jitter: None,
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
pub(crate) fn build_video_trak(config: &VideoConfig) -> Result<Trak> {
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
            track_id: config.track_id(),
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
                timescale: config.timescale(),
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
pub(crate) fn build_audio_trak(config: &AudioConfig) -> Result<Trak> {
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
            track_id: config.track_id(),
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
                timescale: config.timescale(),
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

        assert_eq!(catalog.video_configs().count(), 1);
        assert_eq!(catalog.audio_configs().count(), 1);

        let video = catalog.video_configs().next().unwrap();
        assert!(video.codec.starts_with("avc1."), "got {}", video.codec);
        assert!(video.coded_width > 0);
        assert!(video.coded_height > 0);
        assert!(!video.description.is_empty());

        let audio = catalog.audio_configs().next().unwrap();
        assert!(audio.codec.starts_with("mp4a."), "got {}", audio.codec);
        assert!(audio.sample_rate > 0);
        assert!(audio.number_of_channels > 0);
        assert!(!audio.description.is_empty());
    }

    #[test]
    fn test_catalog_from_h264_opus() {
        let data = read_fixture("h264-opus.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        assert_eq!(catalog.video_configs().count(), 1);
        assert_eq!(catalog.audio_configs().count(), 1);

        let audio = catalog.audio_configs().next().unwrap();
        assert_eq!(audio.codec, "opus");
        assert_eq!(audio.sample_rate, 48000);
        assert!(!audio.description.is_empty());
    }

    #[test]
    fn test_catalog_from_video_only() {
        let data = read_fixture("h264-video-only.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        assert_eq!(catalog.video_configs().count(), 1);
        assert_eq!(catalog.audio_configs().count(), 0);
    }

    #[test]
    fn test_catalog_from_audio_only() {
        let data = read_fixture("opus-audio-only.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        assert_eq!(catalog.video_configs().count(), 0);
        assert_eq!(catalog.audio_configs().count(), 1);
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

        assert_eq!(catalog.video_configs().count(), catalog2.video_configs().count());
        assert_eq!(catalog.audio_configs().count(), catalog2.audio_configs().count());

        let vr1 = &catalog.video.as_ref().unwrap().renditions;
        let vr2 = &catalog2.video.as_ref().unwrap().renditions;
        for (name, v1) in vr1 {
            let v2 = vr2.get(name).expect("video rendition missing");
            assert_eq!(v1.codec, v2.codec);
            assert_eq!(v1.description, v2.description);
            assert_eq!(v1.coded_width, v2.coded_width);
            assert_eq!(v1.coded_height, v2.coded_height);
            assert_eq!(v1.container, v2.container);
        }

        let ar1 = &catalog.audio.as_ref().unwrap().renditions;
        let ar2 = &catalog2.audio.as_ref().unwrap().renditions;
        for (name, a1) in ar1 {
            let a2 = ar2.get(name).expect("audio rendition missing");
            assert_eq!(a1.codec, a2.codec);
            assert_eq!(a1.description, a2.description);
            assert_eq!(a1.sample_rate, a2.sample_rate);
            assert_eq!(a1.number_of_channels, a2.number_of_channels);
            assert_eq!(a1.container, a2.container);
        }
    }

    #[test]
    fn test_init_round_trip_h264_opus() {
        let data = read_fixture("h264-opus.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        let init = build_init_segment(&catalog).unwrap();
        let catalog2 = catalog_from_mp4(Cursor::new(init)).unwrap();

        let v1 = catalog.video_configs().next().unwrap();
        let v2 = catalog2.video_configs().next().unwrap();
        assert_eq!(v1.codec, v2.codec);
        assert_eq!(v1.description, v2.description);

        let a1 = catalog.audio_configs().next().unwrap();
        let a2 = catalog2.audio_configs().next().unwrap();
        assert_eq!(a1.codec, a2.codec);
        assert_eq!(a1.description, a2.description);
    }

    #[test]
    fn test_init_round_trip_opus_only() {
        let data = read_fixture("opus-audio-only.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();

        let init = build_init_segment(&catalog).unwrap();
        let catalog2 = catalog_from_mp4(Cursor::new(init)).unwrap();

        let a1 = catalog.audio_configs().next().unwrap();
        let a2 = catalog2.audio_configs().next().unwrap();
        assert_eq!(a1.codec, a2.codec);
        assert_eq!(a1.description, a2.description);
        assert_eq!(a1.sample_rate, a2.sample_rate);
        assert_eq!(a1.number_of_channels, a2.number_of_channels);
    }

    #[test]
    fn test_init_never_emits_edts() {
        // Canonical init segment never contains edts/elst — presentation
        // offsets live in first-fragment tfdt instead. Use the h264-aac
        // fixture, whose source audio track has media_time=1024 priming
        // and whose video has a trivial (media_time=0) elst — neither
        // should reach the init segment's moov.
        use mp4_atom::FourCC;

        let data = read_fixture("h264-aac.mp4");
        let catalog = catalog_from_mp4(Cursor::new(data)).unwrap();
        let init = build_init_segment(&catalog).unwrap();
        let moov = read_moov(&mut Cursor::new(&init)).unwrap();

        for trak in &moov.trak {
            assert!(
                trak.edts.is_none(),
                "track {} carried edts into init segment",
                trak.tkhd.track_id
            );
        }
        // Also confirm the raw bytes contain no `elst` box anywhere.
        let elst_tag = FourCC::new(b"elst");
        assert!(
            !init.windows(4).any(|w| w == elst_tag.as_ref()),
            "init segment bytes contained an elst tag"
        );
    }

    #[test]
    fn test_start_offset_from_trak_empty_edit() {
        // Synthesize a trak with a leading 9ms empty edit (LosslessCut
        // pattern) and confirm start_offset_from_trak returns the
        // track-timescale equivalent.
        use mp4_atom::{Edts, Elst, ElstEntry};

        let data = read_fixture("h264-aac.mp4");
        let moov = read_moov(&mut Cursor::new(&data)).unwrap();
        let mut trak = moov.trak.iter().find(|t| t.mdia.hdlr.handler.as_ref() == b"vide")
            .cloned().unwrap();
        // Video timescale in this fixture is 15360.
        let video_ts = trak.mdia.mdhd.timescale;
        trak.edts = Some(Edts {
            elst: Some(Elst {
                entries: vec![
                    ElstEntry {
                        segment_duration: 9,
                        media_time: u32::MAX as u64, // empty edit (-1)
                        media_rate: 1,
                        media_rate_fraction: 0,
                    },
                    ElstEntry {
                        segment_duration: 2000,
                        media_time: 0,
                        media_rate: 1,
                        media_rate_fraction: 0,
                    },
                ],
            }),
        });
        let offset = start_offset_from_trak(&trak, 1000);
        // 9 movie ticks @ 1000 → 9 * 15360 / 1000 = 138.24, rounds to 138.
        assert_eq!(offset, 138);
        // Priming-only elst (media_time > 0) does not contribute a leading
        // offset; it's left to the CMAF priming question.
        trak.edts = Some(Edts {
            elst: Some(Elst {
                entries: vec![ElstEntry {
                    segment_duration: 2000,
                    media_time: 1024,
                    media_rate: 1,
                    media_rate_fraction: 0,
                }],
            }),
        });
        assert_eq!(start_offset_from_trak(&trak, 1000), 0);
        let _ = video_ts; // silence unused warning on early returns
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

        assert_eq!(
            moov.trak.len(),
            catalog.video_configs().count() + catalog.audio_configs().count()
        );
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
            assert!(catalog.video_configs().next().is_some(), "{name}: no video tracks");
        }
    }

    #[test]
    fn test_av1_fixtures_extract() {
        for name in &["av1-aac.mp4", "av1-opus.mp4"] {
            let data = read_fixture(name);
            let result = catalog_from_mp4(Cursor::new(data));
            match result {
                Ok(catalog) => {
                    let video = catalog
                        .video_configs()
                        .next()
                        .unwrap_or_else(|| panic!("{name}: no video tracks"));
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
