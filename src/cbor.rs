//! CBOR (DRISL) serialization for MUXL streaming events.
//!
//! Defines the wire format for the `--stdout` streaming protocol.
//! Each event is a separate CBOR value in the stream.
//!
//! ```cbor
//! {"type": "init", "data": h'<ftyp+moov bytes>', "catalog": {"video": {...}, "audio": {...}}}
//! {"type": "segment",
//!  "tracks": {"1": h'<video moof+mdat>', "2": h'<audio moof+mdat>'},
//!  "durations": {"1": 60000, "2": 48000},
//!  "sample_counts": {"1": 60, "2": 50},
//!  "samples": {"1": {"durations": [...], "sizes": [...], "cts_offsets": [...],
//!                    "sync_indices": [1], "offsets": [...]},
//!              "2": {...}},
//!  "body_size": 1296357,
//!  "duration_us": 1000000}
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::init::build_track_init_segments;
use crate::push::SegmenterEvent;
use crate::segment::TrackSamples;

/// Wrapper for `Vec<u8>` that serializes as a CBOR byte string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ByteString(#[serde(with = "serde_bytes")] pub Vec<u8>);

/// Current version of the metafile / streaming wire format. Carried on the
/// `Init` event so durable consumers (e.g. an archive of per-segment metafiles
/// fed back to [`crate::metafile::synthesize_flat_header`]) can gate on it.
/// Bump on a breaking schema change; additive fields stay at the same version.
pub const METAFILE_VERSION: u16 = 1;

/// A MUXL streaming event in CBOR-serializable form.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum CborEvent {
    /// Canonical init segment (ftyp+moov).
    #[serde(rename = "init")]
    Init {
        /// Wire-format version ([`METAFILE_VERSION`]). `0` on a stream from a
        /// pre-versioning muxl; `1`+ once versioned.
        #[serde(default)]
        version: u16,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        /// Track configuration metadata (codecs, dimensions, timescales, etc.).
        #[serde(default)]
        catalog: Option<Catalog>,
        /// Per-track init segments (single-track ftyp+moov), keyed by stringified track ID.
        /// Used by HLS CMAF media playlists where each track needs its own init segment.
        #[serde(default)]
        track_inits: BTreeMap<String, ByteString>,
    },
    /// A complete GOP segment with all tracks bundled.
    ///
    /// Track keys are stringified track IDs (DRISL requires string map keys).
    /// Values are CBOR byte strings containing per-track moof+mdat data.
    #[serde(rename = "segment")]
    Segment {
        tracks: BTreeMap<String, ByteString>,
        /// Per-track total duration in timescale ticks.
        #[serde(default)]
        durations: BTreeMap<String, u64>,
        /// Per-track sample (frame) count.
        #[serde(default)]
        sample_counts: BTreeMap<String, u32>,
        /// Per-track per-sample metadata (parallel arrays). Lets
        /// downstream consumers reconstruct a flat-MP4 moov or build an
        /// HLS playlist without re-parsing the segment bytes.
        #[serde(default)]
        samples: BTreeMap<String, CborTrackSamples>,
        /// Per-track byte size of this segment's contribution to the flat-MP4
        /// body — the on-disk size of each track's canonical bytes (uuid
        /// prefix + moof+mdat run, including any c2pa signature). Required to
        /// place `co64` offsets when synthesizing a header from metafiles
        /// alone (payloads absent), where `tracks[tid].len()` isn't available.
        #[serde(default)]
        track_byte_sizes: BTreeMap<String, u64>,
        /// Per-track decode time (`tfdt`) of this segment's first sample, in
        /// the track's media timescale. Anchors the synthesized flat-MP4
        /// `elst` when a segment range begins mid-stream (see
        /// `spec § edts/elst`). Carried on every segment so any sub-range can
        /// be re-anchored to presentation zero.
        #[serde(default)]
        first_decode_times: BTreeMap<String, u64>,
        /// Total body bytes this segment contributes when concatenated
        /// into a flat MP4. Sum of `tracks` values' lengths.
        #[serde(default)]
        body_size: u64,
        /// Segment's playable duration in microseconds. Suitable for
        /// HLS `EXTINF` (just divide by 1e6).
        #[serde(default)]
        duration_us: u64,
    },
}

/// CBOR-serializable shape of [`TrackSamples`]. Same fields, parallel
/// arrays — but the sync sample list rides as a CBOR array of u32 (the
/// `stss` shape) rather than a packed bitmap, since most segments have
/// just one or two sync samples.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct CborTrackSamples {
    pub durations: Vec<u32>,
    pub sizes: Vec<u32>,
    pub cts_offsets: Vec<i32>,
    pub sync_indices: Vec<u32>,
    pub offsets: Vec<u64>,
}

impl From<&TrackSamples> for CborTrackSamples {
    fn from(s: &TrackSamples) -> Self {
        Self {
            durations: s.durations.clone(),
            sizes: s.sizes.clone(),
            cts_offsets: s.cts_offsets.clone(),
            sync_indices: s.sync_indices.clone(),
            offsets: s.offsets_in_track.clone(),
        }
    }
}

impl From<TrackSamples> for CborTrackSamples {
    fn from(s: TrackSamples) -> Self {
        Self {
            durations: s.durations,
            sizes: s.sizes,
            cts_offsets: s.cts_offsets,
            sync_indices: s.sync_indices,
            offsets: s.offsets_in_track,
        }
    }
}

impl From<&CborTrackSamples> for TrackSamples {
    fn from(s: &CborTrackSamples) -> Self {
        Self {
            durations: s.durations.clone(),
            sizes: s.sizes.clone(),
            cts_offsets: s.cts_offsets.clone(),
            sync_indices: s.sync_indices.clone(),
            offsets_in_track: s.offsets.clone(),
        }
    }
}

impl From<CborTrackSamples> for TrackSamples {
    fn from(s: CborTrackSamples) -> Self {
        Self {
            durations: s.durations,
            sizes: s.sizes,
            cts_offsets: s.cts_offsets,
            sync_indices: s.sync_indices,
            offsets_in_track: s.offsets,
        }
    }
}

impl CborEvent {
    /// Convert a [`SegmenterEvent`] reference into a serializable CBOR event.
    pub fn from_event(event: &SegmenterEvent) -> Self {
        match event {
            SegmenterEvent::InitSegment { catalog, data } => {
                let track_inits = build_track_init_segments(catalog)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(tid, bytes)| (tid.to_string(), ByteString(bytes)))
                    .collect();
                CborEvent::Init {
                    version: METAFILE_VERSION,
                    data: data.clone(),
                    catalog: Some(catalog.clone()),
                    track_inits,
                }
            }
            SegmenterEvent::Segment(gop) => CborEvent::Segment {
                tracks: gop
                    .tracks
                    .iter()
                    .map(|(tid, data)| (tid.to_string(), ByteString(data.clone())))
                    .collect(),
                durations: gop
                    .durations
                    .iter()
                    .map(|(tid, dur)| (tid.to_string(), *dur))
                    .collect(),
                sample_counts: gop
                    .sample_counts
                    .iter()
                    .map(|(tid, count)| (tid.to_string(), *count))
                    .collect(),
                samples: gop
                    .samples
                    .iter()
                    .map(|(tid, s)| (tid.to_string(), s.into()))
                    .collect(),
                track_byte_sizes: gop
                    .tracks
                    .iter()
                    .map(|(tid, data)| (tid.to_string(), data.len() as u64))
                    .collect(),
                first_decode_times: gop
                    .first_decode_times
                    .iter()
                    .map(|(tid, dt)| (tid.to_string(), *dt))
                    .collect(),
                body_size: gop.body_size,
                duration_us: gop.duration_us,
            },
        }
    }

    /// Convert a [`SegmenterEvent`] into a serializable CBOR event (owned).
    pub fn from_event_owned(event: SegmenterEvent) -> Self {
        match event {
            SegmenterEvent::InitSegment { catalog, data } => {
                let track_inits = build_track_init_segments(&catalog)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(tid, bytes)| (tid.to_string(), ByteString(bytes)))
                    .collect();
                CborEvent::Init {
                    version: METAFILE_VERSION,
                    data,
                    catalog: Some(catalog),
                    track_inits,
                }
            }
            SegmenterEvent::Segment(gop) => {
                // Compute the per-track byte sizes before `tracks` is moved.
                let track_byte_sizes = gop
                    .tracks
                    .iter()
                    .map(|(tid, data)| (tid.to_string(), data.len() as u64))
                    .collect();
                let first_decode_times = gop
                    .first_decode_times
                    .iter()
                    .map(|(tid, dt)| (tid.to_string(), *dt))
                    .collect();
                CborEvent::Segment {
                    tracks: gop
                        .tracks
                        .into_iter()
                        .map(|(tid, data)| (tid.to_string(), ByteString(data)))
                        .collect(),
                    durations: gop
                        .durations
                        .into_iter()
                        .map(|(tid, dur)| (tid.to_string(), dur))
                        .collect(),
                    sample_counts: gop
                        .sample_counts
                        .into_iter()
                        .map(|(tid, count)| (tid.to_string(), count))
                        .collect(),
                    samples: gop
                        .samples
                        .into_iter()
                        .map(|(tid, s)| (tid.to_string(), s.into()))
                        .collect(),
                    track_byte_sizes,
                    first_decode_times,
                    body_size: gop.body_size,
                    duration_us: gop.duration_us,
                }
            }
        }
    }
}
