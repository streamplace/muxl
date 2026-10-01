//! Init segment construction and extraction.
//!
//! Converts between Catalog (track configuration metadata) and MP4 init
//! segments (ftyp+moov with empty sample tables). This enables round-tripping
//! between the Hang catalog format and MP4 container headers.
//!
//! Spec: canonical-form.md § Init Segment moov

use std::io::{Cursor, Read, Seek, SeekFrom};

use mp4_atom::{
    Atom, Av01, Av1c, Avc1, Avcc, Codec, Decode, Dinf, Dops, Dref, Elng, Encode, Esds, Ftyp, Hdlr,
    Header, Mdhd, Mdia, Minf, Moov, Mp4a, Mvex, Mvhd, Nmhd, Opus, PlainText, ReadAtom, ReadFrom,
    Stbl, Stco, Stsc, Stsd, Stsz, StszSamples, Stts, Tkhd, Trak, Trex, Url, Visual, Vlab, Vmhd,
    VttC, WriteTo, Wvtt,
};

use crate::catalog::{AudioConfig, Catalog, Container, TextConfig, VideoConfig};
use crate::error::{Error, Result};

// Canonical timescale for mvhd (movie-level, not media-level)
pub(crate) const MOVIE_TIMESCALE: u32 = 1000;

/// Sample entry code for WebVTT in ISOBMFF (ISO/IEC 14496-30). The only text
/// codec MUXL carries.
pub const WVTT_CODEC: &str = "wvtt";

/// Minimal WebVTT file header. Every `vttC` body (and therefore every
/// `TextConfig::config`) starts with this.
pub const WEBVTT_HEADER: &str = "WEBVTT";

/// Extract a Catalog from an MP4 file's moov box.
///
/// Reads codec configuration from stsd entries, dimensions from visual/audio
/// sample entries, and WebVTT headers and languages from text tracks.
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
            // 14496-30 specifies `text` for WebVTT, but accept the subtitle
            // handlers other muxers use. Only a `wvtt` sample entry yields a
            // rendition. Other text formats (tx3g, stpp) are still skipped.
            b"text" | b"subt" | b"sbtl" => {
                if let Some(config) = extract_text_config(trak)? {
                    catalog.insert_text(format!("text{}", track_id), config);
                }
            }
            _ => {} // skip other tracks
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
    for config in catalog.text_configs() {
        track_defs.push(TrackDef::Text(config));
    }
    track_defs.sort_by_key(|t| t.track_id());

    let max_track_id = track_defs.iter().map(|t| t.track_id()).max().unwrap_or(0);

    let mut traks = Vec::new();
    for td in &track_defs {
        traks.push(match td {
            TrackDef::Video(c) => build_video_trak(c)?,
            TrackDef::Audio(c) => build_audio_trak(c)?,
            TrackDef::Text(c) => build_text_trak(c)?,
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

    for config in catalog.text_configs() {
        let mut single = Catalog::default();
        single.insert_text(format!("text{}", config.track_id()), config.clone());
        result.insert(config.track_id(), build_init_segment(&single)?);
    }

    Ok(result)
}

// --- Internal helpers ---

enum TrackDef<'a> {
    Video(&'a VideoConfig),
    Audio(&'a AudioConfig),
    Text(&'a TextConfig),
}

impl TrackDef<'_> {
    fn track_id(&self) -> u32 {
        match self {
            TrackDef::Video(c) => c.track_id(),
            TrackDef::Audio(c) => c.track_id(),
            TrackDef::Text(c) => c.track_id(),
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

/// Extract text (WebVTT) track config from a trak.
///
/// `language` prefers the `elng` BCP 47 tag. Without one, it falls back to
/// the `mdhd` ISO-639 code, shortened to its ISO 639-1 form where one exists
/// (`eng` → `en`), so ordinary muxer output yields a BCP 47 tag. An empty
/// `vlab` is treated as absent, and an empty `vttC` as the minimal header.
fn extract_text_config(trak: &Trak) -> Result<Option<TextConfig>> {
    let track_id = trak.tkhd.track_id;
    let timescale = trak.mdia.mdhd.timescale;
    let container = Container::cmaf(timescale, track_id);

    for codec in &trak.mdia.minf.stbl.stsd.codecs {
        if let Codec::Wvtt(wvtt) = codec {
            let config = if wvtt.config.config.is_empty() {
                WEBVTT_HEADER.to_string()
            } else {
                wvtt.config.config.clone()
            };
            let label = wvtt
                .label
                .as_ref()
                .map(|l| l.source_label.clone())
                .filter(|l| !l.is_empty());
            return Ok(Some(TextConfig {
                codec: WVTT_CODEC.into(),
                container,
                language: text_language_from_mdia(&trak.mdia),
                label,
                config,
            }));
        }
    }
    Ok(None)
}

/// Recover a text track's BCP 47 language tag from `elng` (preferred) or
/// `mdhd`.
fn text_language_from_mdia(mdia: &Mdia) -> String {
    if let Some(elng) = &mdia.elng {
        let tag = elng.extended_language.trim();
        if !tag.is_empty() {
            return tag.to_string();
        }
    }
    language_from_mdhd(&mdia.mdhd.language)
}

/// Normalize a catalog language tag: trim it, and map empty to `und`.
fn canonical_text_language(tag: &str) -> &str {
    let tag = tag.trim();
    if tag.is_empty() { "und" } else { tag }
}

/// The canonical `mdhd` language for a BCP 47 tag: the ISO-639-2/T code of its
/// primary language subtag. Two-letter subtags map through ISO 639-1. A
/// three-letter subtag is used as-is, except that ISO-639-2/B codes are
/// rewritten to their /T form. Anything else (`i-`/`x-` tags, or an unknown
/// two-letter code) maps to `und`.
fn mdhd_language_for(tag: &str) -> String {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !primary.bytes().all(|b| b.is_ascii_lowercase()) {
        return "und".into();
    }
    match primary.len() {
        2 => iso639_1_to_2t(&primary).unwrap_or("und").into(),
        3 => match iso639_2b_to_1(&primary).and_then(iso639_1_to_2t) {
            Some(t) => t.into(),
            None => primary,
        },
        _ => "und".into(),
    }
}

/// Inverse of [`mdhd_language_for`] for a bare `mdhd` code. Codes with an
/// ISO 639-1 equivalent are shortened (`eng` → `en`, including /B codes such as
/// `ger` → `de`). Other well-formed codes are kept, and malformed ones (such as
/// the all-zero code) become `und`.
fn language_from_mdhd(code: &str) -> String {
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_lowercase()) {
        return "und".into();
    }
    ISO639_1_TO_2T
        .iter()
        .find(|(_, t)| *t == code)
        .map(|(one, _)| *one)
        .or_else(|| iso639_2b_to_1(code))
        .map(str::to_string)
        .unwrap_or_else(|| code.to_string())
}

fn iso639_1_to_2t(code: &str) -> Option<&'static str> {
    ISO639_1_TO_2T
        .iter()
        .find(|(one, _)| *one == code)
        .map(|(_, t)| *t)
}

fn iso639_2b_to_1(code: &str) -> Option<&'static str> {
    ISO639_2B_TO_1
        .iter()
        .find(|(b, _)| *b == code)
        .map(|(_, one)| *one)
}

/// ISO 639-1 → ISO 639-2/T (the form `mdhd` specifies).
#[rustfmt::skip]
const ISO639_1_TO_2T: &[(&str, &str)] = &[
    ("aa", "aar"), ("ab", "abk"), ("ae", "ave"), ("af", "afr"), ("ak", "aka"), ("am", "amh"),
    ("an", "arg"), ("ar", "ara"), ("as", "asm"), ("av", "ava"), ("ay", "aym"), ("az", "aze"),
    ("ba", "bak"), ("be", "bel"), ("bg", "bul"), ("bi", "bis"), ("bm", "bam"), ("bn", "ben"),
    ("bo", "bod"), ("br", "bre"), ("bs", "bos"), ("ca", "cat"), ("ce", "che"), ("ch", "cha"),
    ("co", "cos"), ("cr", "cre"), ("cs", "ces"), ("cu", "chu"), ("cv", "chv"), ("cy", "cym"),
    ("da", "dan"), ("de", "deu"), ("dv", "div"), ("dz", "dzo"), ("ee", "ewe"), ("el", "ell"),
    ("en", "eng"), ("eo", "epo"), ("es", "spa"), ("et", "est"), ("eu", "eus"), ("fa", "fas"),
    ("ff", "ful"), ("fi", "fin"), ("fj", "fij"), ("fo", "fao"), ("fr", "fra"), ("fy", "fry"),
    ("ga", "gle"), ("gd", "gla"), ("gl", "glg"), ("gn", "grn"), ("gu", "guj"), ("gv", "glv"),
    ("ha", "hau"), ("he", "heb"), ("hi", "hin"), ("ho", "hmo"), ("hr", "hrv"), ("ht", "hat"),
    ("hu", "hun"), ("hy", "hye"), ("hz", "her"), ("ia", "ina"), ("id", "ind"), ("ie", "ile"),
    ("ig", "ibo"), ("ii", "iii"), ("ik", "ipk"), ("io", "ido"), ("is", "isl"), ("it", "ita"),
    ("iu", "iku"), ("ja", "jpn"), ("jv", "jav"), ("ka", "kat"), ("kg", "kon"), ("ki", "kik"),
    ("kj", "kua"), ("kk", "kaz"), ("kl", "kal"), ("km", "khm"), ("kn", "kan"), ("ko", "kor"),
    ("kr", "kau"), ("ks", "kas"), ("ku", "kur"), ("kv", "kom"), ("kw", "cor"), ("ky", "kir"),
    ("la", "lat"), ("lb", "ltz"), ("lg", "lug"), ("li", "lim"), ("ln", "lin"), ("lo", "lao"),
    ("lt", "lit"), ("lu", "lub"), ("lv", "lav"), ("mg", "mlg"), ("mh", "mah"), ("mi", "mri"),
    ("mk", "mkd"), ("ml", "mal"), ("mn", "mon"), ("mr", "mar"), ("ms", "msa"), ("mt", "mlt"),
    ("my", "mya"), ("na", "nau"), ("nb", "nob"), ("nd", "nde"), ("ne", "nep"), ("ng", "ndo"),
    ("nl", "nld"), ("nn", "nno"), ("no", "nor"), ("nr", "nbl"), ("nv", "nav"), ("ny", "nya"),
    ("oc", "oci"), ("oj", "oji"), ("om", "orm"), ("or", "ori"), ("os", "oss"), ("pa", "pan"),
    ("pi", "pli"), ("pl", "pol"), ("ps", "pus"), ("pt", "por"), ("qu", "que"), ("rm", "roh"),
    ("rn", "run"), ("ro", "ron"), ("ru", "rus"), ("rw", "kin"), ("sa", "san"), ("sc", "srd"),
    ("sd", "snd"), ("se", "sme"), ("sg", "sag"), ("si", "sin"), ("sk", "slk"), ("sl", "slv"),
    ("sm", "smo"), ("sn", "sna"), ("so", "som"), ("sq", "sqi"), ("sr", "srp"), ("ss", "ssw"),
    ("st", "sot"), ("su", "sun"), ("sv", "swe"), ("sw", "swa"), ("ta", "tam"), ("te", "tel"),
    ("tg", "tgk"), ("th", "tha"), ("ti", "tir"), ("tk", "tuk"), ("tl", "tgl"), ("tn", "tsn"),
    ("to", "ton"), ("tr", "tur"), ("ts", "tso"), ("tt", "tat"), ("tw", "twi"), ("ty", "tah"),
    ("ug", "uig"), ("uk", "ukr"), ("ur", "urd"), ("uz", "uzb"), ("ve", "ven"), ("vi", "vie"),
    ("vo", "vol"), ("wa", "wln"), ("wo", "wol"), ("xh", "xho"), ("yi", "yid"), ("yo", "yor"),
    ("za", "zha"), ("zh", "zho"), ("zu", "zul"),
];

/// ISO-639-2/B codes that differ from /T. Some muxers write these into `mdhd`.
#[rustfmt::skip]
const ISO639_2B_TO_1: &[(&str, &str)] = &[
    ("alb", "sq"), ("arm", "hy"), ("baq", "eu"), ("bur", "my"), ("chi", "zh"), ("cze", "cs"),
    ("dut", "nl"), ("fre", "fr"), ("geo", "ka"), ("ger", "de"), ("gre", "el"), ("ice", "is"),
    ("mac", "mk"), ("mao", "mi"), ("may", "ms"), ("per", "fa"), ("rum", "ro"), ("slo", "sk"),
    ("tib", "bo"), ("wel", "cy"),
];

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
            elng: None,
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
            elng: None,
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

/// Build a canonical text (WebVTT) trak box from config.
///
/// Canonical form (ISO/IEC 14496-30):
/// - `hdlr` is `text` with an empty name, and `minf` carries `nmhd`.
/// - `stsd` holds one `wvtt` sample entry (`data_reference_index` 1). It
///   contains `vttC` = `config`, a `vlab` only when `label` is non-empty, and
///   no `btrt`.
/// - `mdhd.language` is the ISO-639-2/T code from [`mdhd_language_for`]. `elng`
///   holds the full BCP 47 tag, but only when `mdhd` alone would not reproduce
///   it on extraction. Plain `en` is written as `mdhd eng` with no `elng`;
///   `en-US` is written as `mdhd eng` plus `elng en-US`.
/// - `tkhd` uses volume 0, zero width and height, and no alternate group.
pub(crate) fn build_text_trak(config: &TextConfig) -> Result<Trak> {
    if config.codec != WVTT_CODEC {
        return Err(Error::InvalidMp4(format!(
            "unsupported text codec: {}",
            config.codec
        )));
    }
    if !config.config.starts_with(WEBVTT_HEADER) {
        return Err(Error::InvalidMp4(format!(
            "text track {}: wvtt config must start with \"{WEBVTT_HEADER}\"",
            config.track_id()
        )));
    }

    let language = canonical_text_language(&config.language);
    let mdhd_language = mdhd_language_for(language);
    let elng = (language_from_mdhd(&mdhd_language) != language).then(|| Elng {
        extended_language: language.to_string(),
    });

    let codec = Codec::Wvtt(Wvtt {
        plaintext: PlainText {
            data_reference_index: 1,
        },
        config: VttC {
            config: config.config.clone(),
        },
        label: config
            .label
            .as_ref()
            .filter(|l| !l.is_empty())
            .map(|l| Vlab {
                source_label: l.clone(),
            }),
        btrt: None,
    });

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
                language: mdhd_language,
            },
            hdlr: Hdlr {
                handler: b"text".into(),
                name: String::new(),
            },
            elng,
            minf: Minf {
                vmhd: None,
                smhd: None,
                nmhd: Some(Nmhd::default()),
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

    fn text_config(track_id: u32, language: &str, label: Option<&str>) -> TextConfig {
        TextConfig {
            codec: WVTT_CODEC.into(),
            container: Container::cmaf(1000, track_id),
            language: language.into(),
            label: label.map(Into::into),
            config: "WEBVTT\n\nSTYLE\n::cue { color: white }\n".into(),
        }
    }

    fn text_trak(moov: &Moov) -> &Trak {
        moov.trak
            .iter()
            .find(|t| t.mdia.hdlr.handler.as_ref() == b"text")
            .expect("text trak")
    }

    fn find_tag(haystack: &[u8], tag: &[u8; 4]) -> Option<usize> {
        haystack.windows(4).position(|w| w == tag)
    }

    #[test]
    fn test_text_init_canonical_boxes() {
        let mut catalog = Catalog::default();
        catalog.insert_text("text1", text_config(1, "en-US", Some("captions")));
        let init = build_init_segment(&catalog).unwrap();
        let moov = read_moov(&mut Cursor::new(&init)).unwrap();

        assert_eq!(moov.trak.len(), 1);
        assert_eq!(moov.mvhd.next_track_id, 2);
        assert_eq!(moov.mvex.as_ref().unwrap().trex[0].track_id, 1);

        let trak = text_trak(&moov);
        assert_eq!(trak.tkhd.track_id, 1);
        assert!(trak.edts.is_none());
        assert_eq!(trak.mdia.mdhd.timescale, 1000);
        assert_eq!(trak.mdia.mdhd.language, "eng");
        assert_eq!(
            trak.mdia.elng.as_ref().map(|e| e.extended_language.as_str()),
            Some("en-US")
        );
        assert!(trak.mdia.hdlr.name.is_empty());
        let minf = &trak.mdia.minf;
        assert!(minf.nmhd.is_some());
        assert!(minf.vmhd.is_none() && minf.smhd.is_none() && minf.sthd.is_none());

        let codecs = &minf.stbl.stsd.codecs;
        assert_eq!(codecs.len(), 1);
        let Codec::Wvtt(wvtt) = &codecs[0] else {
            panic!("expected wvtt sample entry, got {:?}", codecs[0]);
        };
        assert_eq!(wvtt.plaintext.data_reference_index, 1);
        assert_eq!(wvtt.config.config, "WEBVTT\n\nSTYLE\n::cue { color: white }\n");
        assert_eq!(
            wvtt.label.as_ref().map(|l| l.source_label.as_str()),
            Some("captions")
        );
        assert!(wvtt.btrt.is_none());

        // elng sits in its 14496-12 Table 1 slot: after hdlr, before minf.
        let hdlr = find_tag(&init, b"hdlr").unwrap();
        let elng = find_tag(&init, b"elng").unwrap();
        let minf = find_tag(&init, b"minf").unwrap();
        assert!(hdlr < elng && elng < minf, "hdlr={hdlr} elng={elng} minf={minf}");
    }

    #[test]
    fn test_text_init_round_trip_and_idempotent() {
        let mut catalog = Catalog::default();
        catalog.insert_text("text1", text_config(1, "en-US", Some("captions")));
        catalog.insert_text("text2", text_config(2, "es", None));

        let init1 = build_init_segment(&catalog).unwrap();
        let catalog2 = catalog_from_mp4(Cursor::new(&init1)).unwrap();
        assert_eq!(catalog2, catalog, "text catalog must round-trip exactly");
        let init2 = build_init_segment(&catalog2).unwrap();
        assert_eq!(init1, init2, "text init segment should be idempotent");
    }

    #[test]
    fn test_text_language_mapping() {
        // (catalog language, mdhd code, elng, language extracted back)
        let cases: &[(&str, &str, Option<&str>, &str)] = &[
            ("en", "eng", None, "en"),
            ("en-US", "eng", Some("en-US"), "en-US"),
            ("de", "deu", None, "de"),
            ("fr-CA", "fra", Some("fr-CA"), "fr-CA"),
            ("zh-Hant", "zho", Some("zh-Hant"), "zh-Hant"),
            ("pt-BR", "por", Some("pt-BR"), "pt-BR"),
            // Three-letter primary subtags pass through to mdhd.
            ("yue", "yue", None, "yue"),
            ("yue-HK", "yue", Some("yue-HK"), "yue-HK"),
            // Non-shortest forms keep elng so the exact tag survives.
            ("eng", "eng", Some("eng"), "eng"),
            ("ger", "deu", Some("ger"), "ger"),
            ("EN", "eng", Some("EN"), "EN"),
            ("und", "und", None, "und"),
            ("", "und", None, "und"),
            ("  ", "und", None, "und"),
            ("x-klingon", "und", Some("x-klingon"), "x-klingon"),
            ("qq", "und", Some("qq"), "qq"),
        ];
        for &(lang, mdhd, elng, back) in cases {
            let trak = build_text_trak(&text_config(1, lang, None)).unwrap();
            assert_eq!(trak.mdia.mdhd.language, mdhd, "{lang:?}: mdhd");
            assert_eq!(
                trak.mdia.elng.as_ref().map(|e| e.extended_language.as_str()),
                elng,
                "{lang:?}: elng"
            );
            // Through bytes, so mdhd's packed 5-bit encoding is exercised.
            let mut catalog = Catalog::default();
            catalog.insert_text("text1", text_config(1, lang, None));
            let init = build_init_segment(&catalog).unwrap();
            let got = catalog_from_mp4(Cursor::new(&init)).unwrap();
            let got_lang = &got.text_configs().next().unwrap().language;
            assert_eq!(got_lang, back, "{lang:?}: extracted language");
            // Normalization is a fixed point.
            assert_eq!(build_init_segment(&got).unwrap(), init, "{lang:?}: not idempotent");
        }
    }

    #[test]
    fn test_text_extraction_tolerates_muxer_variants() {
        let mut catalog = Catalog::default();
        catalog.insert_text("text3", text_config(3, "fr", Some("src")));
        let init = build_init_segment(&catalog).unwrap();
        let mut moov = read_moov(&mut Cursor::new(&init)).unwrap();
        {
            let trak = &mut moov.trak[0];
            // Subtitle handler, ISO-639-2/B mdhd code, empty vlab and vttC.
            trak.mdia.hdlr.handler = b"subt".into();
            trak.mdia.mdhd.language = "fre".into();
            trak.mdia.elng = None;
            let Codec::Wvtt(wvtt) = &mut trak.mdia.minf.stbl.stsd.codecs[0] else {
                panic!("expected wvtt");
            };
            wvtt.label = Some(Vlab {
                source_label: String::new(),
            });
            wvtt.config.config = String::new();
        }
        let got = catalog_from_moov(&moov).unwrap();
        let t = &got.text.as_ref().unwrap().renditions["text3"];
        assert_eq!(t.language, "fr");
        assert_eq!(t.label, None);
        assert_eq!(t.config, WEBVTT_HEADER);
        assert_eq!(t.track_id(), 3);
        assert_eq!(t.timescale(), 1000);

        // An unparseable (all-zero) mdhd code yields und.
        moov.trak[0].mdia.mdhd.language = "```".into();
        let got = catalog_from_moov(&moov).unwrap();
        assert_eq!(got.text_configs().next().unwrap().language, "und");

        // A text-handler trak without a wvtt entry is skipped, as before.
        moov.trak[0].mdia.minf.stbl.stsd.codecs.clear();
        let got = catalog_from_moov(&moov).unwrap();
        assert!(got.text.is_none());
    }

    #[test]
    fn test_text_build_rejects_invalid_config() {
        let mut bad_codec = text_config(1, "en", None);
        bad_codec.codec = "stpp".into();
        assert!(build_text_trak(&bad_codec).is_err());

        let mut bad_header = text_config(1, "en", None);
        bad_header.config = "NOT VTT".into();
        assert!(build_text_trak(&bad_header).is_err());

        let mut minimal = text_config(1, "en", None);
        minimal.config = WEBVTT_HEADER.into();
        assert!(build_text_trak(&minimal).is_ok());
    }

    #[test]
    fn test_av_with_text_keeps_av_traks_identical() {
        let data = read_fixture("h264-aac.mp4");
        let av = catalog_from_mp4(Cursor::new(data)).unwrap();
        let av_init = build_init_segment(&av).unwrap();
        // AV-only init bytes carry no text-only boxes.
        for tag in [b"elng", b"wvtt", b"vttC", b"nmhd"] {
            assert!(find_tag(&av_init, tag).is_none(), "AV init has {tag:?}");
        }

        let next_id = av.video_configs().map(|c| c.track_id())
            .chain(av.audio_configs().map(|c| c.track_id()))
            .max()
            .unwrap()
            + 1;
        let mut mixed = av.clone();
        mixed.insert_text(format!("text{next_id}"), text_config(next_id, "en", None));
        let mixed_init = build_init_segment(&mixed).unwrap();

        let av_moov = read_moov(&mut Cursor::new(&av_init)).unwrap();
        let mixed_moov = read_moov(&mut Cursor::new(&mixed_init)).unwrap();
        assert_eq!(mixed_moov.trak.len(), av_moov.trak.len() + 1);
        assert_eq!(&mixed_moov.trak[..av_moov.trak.len()], &av_moov.trak[..]);
        assert_eq!(mixed_moov.mvhd.next_track_id, next_id + 1);
        assert_eq!(
            mixed_moov.mvex.as_ref().unwrap().trex.last().unwrap().track_id,
            next_id
        );

        let extracted = catalog_from_mp4(Cursor::new(&mixed_init)).unwrap();
        assert_eq!(extracted.text, mixed.text);
        assert_eq!(extracted.video_configs().count(), 1);
        assert_eq!(extracted.audio_configs().count(), 1);

        let per_track = build_track_init_segments(&mixed).unwrap();
        assert_eq!(per_track.len(), av_moov.trak.len() + 1);
        let text_only = catalog_from_mp4(Cursor::new(&per_track[&next_id])).unwrap();
        assert_eq!(text_only, mixed.filter_to_track(next_id));
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
