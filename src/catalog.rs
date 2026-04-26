//! Catalog types for track configuration metadata.
//!
//! These types describe the codec configuration needed to initialize a
//! decoder for each track. The in-memory shape mirrors Hang's catalog
//! schema (https://doc.moq.dev/concept/layer/hang) — same field names,
//! same per-track-type renditions map, same nested `container`
//! discriminator — so a MUXL catalog can round-trip through Hang's zod
//! validators without restructuring.
//!
//! Two serialization targets:
//!
//! - **DRISL (CBOR)** — the canonical / content-addressed form. Byte
//!   fields stay binary, producing deterministic output. Catalog CIDs are
//!   BLAKE3 of these bytes. See [`to_drisl`] / [`from_drisl`].
//! - **Hang JSON** — the human-readable / web-interop form. Byte fields
//!   are hex-encoded (Hang's current convention, with a stated plan to
//!   flip to base64). See [`to_hang_json`] / [`from_hang_json`].
//!
//! Both forms are produced from the same Rust struct. A custom serde
//! helper (`description_codec`) picks the encoding per call site using
//! `Serializer::is_human_readable()` — CBOR returns false, JSON true.
//!
//! Spec: `canonical-form.md § Init Segment moov` for how these map back
//! into ISOBMFF boxes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Top-level catalog. Carries both track-type groups. Missing groups
/// serialize as absent fields (not as empty objects), so an audio-only
/// stream produces `{"audio": {...}}` with no `video` key.
///
/// Hang's top-level catalog also carries `location`, `user`, `chat`,
/// `capabilities`, `preview` etc. MUXL doesn't emit or interpret those
/// but unknown fields are ignored on deserialize, so a Hang catalog with
/// extras still round-trips through MUXL lossily-but-safely (the extras
/// are dropped).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub video: Option<Video>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub audio: Option<Audio>,
}

/// Video group. `renditions` is a name→config map (not an array) so
/// JSON-Merge-Patch can swap individual renditions without touching others.
/// `display`, `rotation`, `flip` are rendering-time overrides shared
/// across all renditions — distinct from per-rendition
/// `displayAspectWidth/Height`, which change the decoder init.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Video {
    pub renditions: BTreeMap<String, VideoConfig>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub display: Option<Display>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rotation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub flip: Option<bool>,
}

/// Audio group. Same shape rule as `Video`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Audio {
    pub renditions: BTreeMap<String, AudioConfig>,
}

/// Render target for a video track, independent of the coded pixel
/// dimensions. Both fields are required when the object is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Display {
    pub width: u32,
    pub height: u32,
}

/// Per-rendition video configuration. Mirrors Hang's `VideoConfig`.
///
/// `codec` and `container` are required. `codedWidth`/`codedHeight` are
/// required in MUXL (we always extract them from the source), even though
/// Hang marks them optional — it lets decoders without a description
/// re-derive dimensions from in-band SPS/PPS. Our Rust type keeps them
/// required because every path here produces them.
///
/// `description` holds the codec-specific decoder-config bytes
/// (`avcC` for H.264, `av1C` for AV1, `dOps` for Opus, `esds` for AAC).
/// Empty = field omitted on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoConfig {
    pub codec: String,
    pub container: Container,
    #[serde(with = "description_codec", skip_serializing_if = "Vec::is_empty", default)]
    pub description: Vec<u8>,
    pub coded_width: u32,
    pub coded_height: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub display_aspect_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub display_aspect_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub framerate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bitrate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub optimize_for_latency: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub jitter: Option<u64>,
}

/// Per-rendition audio configuration. Mirrors Hang's `AudioConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioConfig {
    pub codec: String,
    pub container: Container,
    #[serde(with = "description_codec", skip_serializing_if = "Vec::is_empty", default)]
    pub description: Vec<u8>,
    pub sample_rate: u32,
    pub number_of_channels: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bitrate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub jitter: Option<u64>,
}

/// Container / transport framing. MUXL only produces CMAF, but the
/// Legacy variant round-trips through Hang catalogs unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Container {
    /// Hang's default transport: raw codec frames with QUIC-VarInt
    /// timestamps in microseconds. MUXL never produces this.
    Legacy,
    /// Fragmented MP4: frames are complete `moof+mdat` pairs.
    #[serde(rename_all = "camelCase")]
    Cmaf {
        /// Media timescale — ticks per second in the `trun` sample durations
        /// and `tfdt` base decode times.
        timescale: u32,
        /// Track ID used in the `moof.tfhd.track_id` field.
        track_id: u32,
    },
}

impl Default for Container {
    fn default() -> Self {
        Container::Legacy
    }
}

impl Container {
    /// Shortcut for the common case: build a CMAF container.
    pub fn cmaf(timescale: u32, track_id: u32) -> Self {
        Container::Cmaf {
            timescale,
            track_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Convenience accessors
// ---------------------------------------------------------------------------

impl VideoConfig {
    /// Track ID from the container. Returns 0 for the (non-MUXL) Legacy variant.
    pub fn track_id(&self) -> u32 {
        match self.container {
            Container::Cmaf { track_id, .. } => track_id,
            Container::Legacy => 0,
        }
    }

    /// Media timescale from the container. Returns 0 for Legacy.
    pub fn timescale(&self) -> u32 {
        match self.container {
            Container::Cmaf { timescale, .. } => timescale,
            Container::Legacy => 0,
        }
    }
}

impl AudioConfig {
    pub fn track_id(&self) -> u32 {
        match self.container {
            Container::Cmaf { track_id, .. } => track_id,
            Container::Legacy => 0,
        }
    }

    pub fn timescale(&self) -> u32 {
        match self.container {
            Container::Cmaf { timescale, .. } => timescale,
            Container::Legacy => 0,
        }
    }
}

impl Catalog {
    /// Iterate all video rendition configs, regardless of rendition name.
    pub fn video_configs(&self) -> impl Iterator<Item = &VideoConfig> {
        self.video.iter().flat_map(|v| v.renditions.values())
    }

    /// Iterate all audio rendition configs.
    pub fn audio_configs(&self) -> impl Iterator<Item = &AudioConfig> {
        self.audio.iter().flat_map(|a| a.renditions.values())
    }

    /// Mutable iterator over video rendition configs.
    pub fn video_configs_mut(&mut self) -> impl Iterator<Item = &mut VideoConfig> {
        self.video.iter_mut().flat_map(|v| v.renditions.values_mut())
    }

    /// Mutable iterator over audio rendition configs.
    pub fn audio_configs_mut(&mut self) -> impl Iterator<Item = &mut AudioConfig> {
        self.audio.iter_mut().flat_map(|a| a.renditions.values_mut())
    }

    /// Insert a video rendition, creating the `Video` wrapper if missing.
    pub fn insert_video(&mut self, name: impl Into<String>, config: VideoConfig) {
        let video = self.video.get_or_insert_with(|| Video {
            renditions: BTreeMap::new(),
            display: None,
            rotation: None,
            flip: None,
        });
        video.renditions.insert(name.into(), config);
    }

    /// Insert an audio rendition, creating the `Audio` wrapper if missing.
    pub fn insert_audio(&mut self, name: impl Into<String>, config: AudioConfig) {
        let audio = self.audio.get_or_insert_with(|| Audio {
            renditions: BTreeMap::new(),
        });
        audio.renditions.insert(name.into(), config);
    }

    /// Return a new catalog containing only the rendition whose `track_id`
    /// matches. The matching rendition's wrapping `Video` or `Audio` is
    /// retained (with its other-rendition entries stripped); the opposite
    /// track-type wrapper is dropped entirely. If no rendition matches,
    /// both wrappers are `None`.
    pub fn filter_to_track(&self, track_id: u32) -> Catalog {
        let video = self.video.as_ref().and_then(|v| {
            let renditions: BTreeMap<String, VideoConfig> = v
                .renditions
                .iter()
                .filter(|(_, c)| c.track_id() == track_id)
                .map(|(k, c)| (k.clone(), c.clone()))
                .collect();
            if renditions.is_empty() {
                None
            } else {
                Some(Video {
                    renditions,
                    display: v.display,
                    rotation: v.rotation,
                    flip: v.flip,
                })
            }
        });
        let audio = self.audio.as_ref().and_then(|a| {
            let renditions: BTreeMap<String, AudioConfig> = a
                .renditions
                .iter()
                .filter(|(_, c)| c.track_id() == track_id)
                .map(|(k, c)| (k.clone(), c.clone()))
                .collect();
            if renditions.is_empty() {
                None
            } else {
                Some(Audio { renditions })
            }
        });
        Catalog { video, audio }
    }
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

/// Extract a catalog from any supported MP4 wrapper (fMP4 or flat MP4),
/// reading only the `moov` box. Cheaper than [`crate::read`] when you
/// only need codec info — no per-sample plan is built.
pub fn from_input<R: crate::io::ReadAt + ?Sized>(input: &R) -> Result<Catalog> {
    let mut cursor = crate::io::ReadAtCursor::new(input).map_err(Error::Io)?;
    let moov = crate::init::read_moov(&mut cursor)?;
    crate::init::catalog_from_moov(&moov)
}

/// Encode the catalog as DRISL (deterministic CBOR). This is the
/// canonical / content-addressed form — the bytes are BLAKE3-hashed to
/// produce the catalog's CID. Binary fields (`description`) stay binary.
pub fn to_drisl(catalog: &Catalog) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    dasl::drisl::to_writer(&mut buf, catalog).map_err(drisl_err)?;
    Ok(buf)
}

/// Decode a catalog from DRISL bytes.
pub fn from_drisl(bytes: &[u8]) -> Result<Catalog> {
    dasl::drisl::from_slice(bytes).map_err(drisl_err)
}

/// Encode the catalog as Hang-shaped JSON (camelCase field names,
/// `description` as a hex string). Pretty-printed for humans; for
/// wire transport prefer [`to_drisl`].
///
/// Hang currently uses hex for `description` with a "TODO use base64"
/// note in their schema; MUXL tracks their current choice.
pub fn to_hang_json(catalog: &Catalog) -> Result<String> {
    serde_json::to_string_pretty(catalog).map_err(json_err)
}

/// Decode a catalog from Hang-shaped JSON. Extra top-level Hang fields
/// (`location`, `user`, `chat`, etc.) that MUXL doesn't use are ignored.
pub fn from_hang_json(json: &str) -> Result<Catalog> {
    serde_json::from_str(json).map_err(json_err)
}

fn drisl_err(e: impl std::fmt::Display) -> Error {
    Error::InvalidMp4(format!("drisl: {e}"))
}

fn json_err(e: impl std::fmt::Display) -> Error {
    Error::InvalidMp4(format!("json: {e}"))
}

// ---------------------------------------------------------------------------
// description codec: bytes in CBOR, hex string in JSON
// ---------------------------------------------------------------------------

/// Serde helper that serializes a `Vec<u8>` as a CBOR byte string for
/// binary formats (CBOR/DRISL) and as a hex string for human-readable
/// formats (JSON), using `Serializer::is_human_readable()` to pick.
///
/// Applied via `#[serde(with = "description_codec")]` on the
/// `description` fields of [`VideoConfig`] and [`AudioConfig`]. Paired
/// with `skip_serializing_if = "Vec::is_empty"` so a catalog with no
/// description omits the field entirely on the wire (matching Hang's
/// `description: z.optional(z.string())`).
mod description_codec {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&hex::encode(bytes))
        } else {
            s.serialize_bytes(bytes)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            hex::decode(&s).map_err(D::Error::custom)
        } else {
            // Accept either a byte string or a sequence of u8 (serde's Vec<u8>
            // default) to be lenient with CBOR encoders that don't use bstr.
            let buf = serde_bytes::ByteBuf::deserialize(d)?;
            Ok(buf.into_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_catalog() -> Catalog {
        let mut c = Catalog::default();
        c.insert_video(
            "video1",
            VideoConfig {
                codec: "avc1.64001f".into(),
                container: Container::cmaf(15360, 1),
                description: b"\x01\x64\x00\x1f\xff\xe1".to_vec(),
                coded_width: 1920,
                coded_height: 1080,
                display_aspect_width: None,
                display_aspect_height: None,
                framerate: Some(30.0),
                bitrate: None,
                optimize_for_latency: None,
                jitter: None,
            },
        );
        c.insert_audio(
            "audio1",
            AudioConfig {
                codec: "mp4a.40.2".into(),
                container: Container::cmaf(48000, 2),
                description: b"\x12\x10".to_vec(),
                sample_rate: 48000,
                number_of_channels: 2,
                bitrate: None,
                jitter: None,
            },
        );
        c
    }

    #[test]
    fn drisl_round_trip_preserves_catalog() {
        let original = sample_catalog();
        let bytes = to_drisl(&original).unwrap();
        let decoded = from_drisl(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn drisl_is_deterministic() {
        let original = sample_catalog();
        let a = to_drisl(&original).unwrap();
        let b = to_drisl(&original).unwrap();
        assert_eq!(a, b, "DRISL must produce byte-identical output on re-encode");
    }

    #[test]
    fn hang_json_round_trip_preserves_catalog() {
        let original = sample_catalog();
        let json = to_hang_json(&original).unwrap();
        let decoded = from_hang_json(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn hang_json_uses_hang_field_names() {
        // Spot-check that the output carries Hang's camelCase field names
        // and hex-encoded description — the whole point of this path.
        let catalog = sample_catalog();
        let json = to_hang_json(&catalog).unwrap();
        assert!(json.contains("\"codedWidth\""), "got: {json}");
        assert!(json.contains("\"codedHeight\""));
        assert!(json.contains("\"numberOfChannels\""));
        assert!(json.contains("\"sampleRate\""));
        assert!(json.contains("\"renditions\""));
        // container discriminator
        assert!(json.contains("\"kind\": \"cmaf\""));
        assert!(json.contains("\"trackId\""));
        // description hex (0164001fffe1 lowercased)
        assert!(json.contains("\"0164001fffe1\""));
        // Absent optionals are elided (not serialized as null)
        assert!(!json.contains("\"bitrate\""));
        assert!(!json.contains("\"jitter\""));
    }

    #[test]
    fn hang_json_ignores_unknown_top_level_fields() {
        // Hang catalogs carry extras like `location`, `user`, `chat`. MUXL
        // doesn't use them but shouldn't error on their presence.
        let json = r#"{
            "video": { "renditions": {} },
            "user": { "name": "alice" },
            "capabilities": {}
        }"#;
        let catalog = from_hang_json(json).unwrap();
        assert!(catalog.video.is_some());
        assert!(catalog.audio.is_none());
    }

    #[test]
    fn missing_description_round_trips() {
        // description is optional on the wire (empty Vec → absent). Make
        // sure a catalog with no description survives both paths.
        let mut c = Catalog::default();
        c.insert_audio(
            "audio1",
            AudioConfig {
                codec: "opus".into(),
                container: Container::cmaf(48000, 1),
                description: Vec::new(),
                sample_rate: 48000,
                number_of_channels: 2,
                bitrate: None,
                jitter: None,
            },
        );
        let drisl = to_drisl(&c).unwrap();
        assert_eq!(from_drisl(&drisl).unwrap(), c);
        let json = to_hang_json(&c).unwrap();
        assert!(!json.contains("\"description\""));
        assert_eq!(from_hang_json(&json).unwrap(), c);
    }

    #[test]
    fn legacy_container_round_trips() {
        let mut c = Catalog::default();
        c.insert_audio(
            "audio1",
            AudioConfig {
                codec: "opus".into(),
                container: Container::Legacy,
                description: Vec::new(),
                sample_rate: 48000,
                number_of_channels: 2,
                bitrate: None,
                jitter: None,
            },
        );
        let json = to_hang_json(&c).unwrap();
        assert!(json.contains("\"kind\": \"legacy\""));
        assert_eq!(from_hang_json(&json).unwrap(), c);
        let drisl = to_drisl(&c).unwrap();
        assert_eq!(from_drisl(&drisl).unwrap(), c);
    }

    #[test]
    fn catalog_from_real_mp4_round_trips_both_formats() {
        // End-to-end: extract catalog from a real MP4, round-trip through
        // both DRISL and Hang JSON, confirm equality in both directions.
        use std::io::Cursor;
        let data = std::fs::read("samples/fixtures/h264-aac.mp4").unwrap();
        let catalog = crate::init::catalog_from_mp4(Cursor::new(data)).unwrap();

        let drisl = to_drisl(&catalog).unwrap();
        assert_eq!(from_drisl(&drisl).unwrap(), catalog);

        let json = to_hang_json(&catalog).unwrap();
        assert_eq!(from_hang_json(&json).unwrap(), catalog);
    }
}
