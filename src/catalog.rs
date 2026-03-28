//! Catalog types for track configuration metadata.
//!
//! These types capture the codec configuration needed to reconstruct MP4
//! init segments (ftyp+moov) from out-of-band metadata. They align with
//! the Hang catalog schema (WebCodecs-style codec descriptions) but are
//! serialization-format-agnostic at this layer.
//!
//! Spec: architecture.md § Hang CMAF

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Complete catalog describing all tracks in a presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// Video tracks, keyed by rendition name.
    pub video: BTreeMap<String, VideoTrackConfig>,
    /// Audio tracks, keyed by rendition name.
    pub audio: BTreeMap<String, AudioTrackConfig>,
}

/// Configuration for a video track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoTrackConfig {
    /// WebCodecs codec string, e.g. "avc1.64001f", "av01.0.08M.08".
    pub codec: String,
    /// Raw codec-specific description bytes (avcC record for H.264, av1C for AV1).
    /// This is the WebCodecs VideoDecoderConfig.description content.
    #[serde(with = "serde_bytes")]
    pub description: Vec<u8>,
    /// Coded pixel width.
    pub coded_width: u32,
    /// Coded pixel height.
    pub coded_height: u32,
    /// Track ID for the MP4 container.
    pub track_id: u32,
    /// Media timescale (ticks per second). Sample durations in fragments are
    /// expressed in this timescale. Matches Hang catalog container.timescale.
    pub timescale: u32,
}

/// Configuration for an audio track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTrackConfig {
    /// WebCodecs codec string, e.g. "opus", "mp4a.40.2".
    pub codec: String,
    /// Raw codec-specific description bytes (dOps content for Opus, esds content for AAC).
    #[serde(with = "serde_bytes")]
    pub description: Vec<u8>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of audio channels.
    pub number_of_channels: u32,
    /// Track ID for the MP4 container.
    pub track_id: u32,
    /// Media timescale (ticks per second). Sample durations in fragments are
    /// expressed in this timescale. Matches Hang catalog container.timescale.
    pub timescale: u32,
}
