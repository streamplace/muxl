//! Wire format for the `muxl segment` streaming protocol.
//!
//! Two event kinds, each one DRISL/CBOR value on stdout:
//!
//! ```cbor
//! {"type": "init", "data": h'<ftyp+moov bytes>', "catalog": {...}, "track_inits": {...}}
//! {"type": "signed-segment",
//!  "number": <u32>,
//!  "tracks": {"1": h'<c2pa+muxl+frags for track 1>', "2": h'<...>'},
//!  "durations": {"1": 60000, ...},
//!  "sample_counts": {"1": 60, ...},
//!  "samples": {"1": {"durations":[...], "sizes":[...], ...}, ...},
//!  "first_decode_times": {"1": 0, "2": 0},
//!  "body_size": 1308612,
//!  "duration_us": 1000000}
//! ```
//!
//! `tracks[tid]` carries the signed canonical-segment bytes for that
//! track (c2pa-uuid + muxl-uuid + moof+mdat run). `samples[tid].offsets`
//! point at sample payload inside the signed bytes — already shifted
//! past the leading c2pa-uuid + muxl-uuid header. `body_size` is the
//! sum of `tracks` byte lengths.
//!
//! Mirrors muxl's own `CborEvent` field-for-field plus the per-track
//! payload being signed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use muxl::GopSegment;
use muxl::catalog::Catalog;
use muxl::cbor::{ByteString, CborTrackSamples};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignedEvent {
    /// Canonical init segment (`ftyp+moov`) and catalog, mirroring
    /// `muxl::cbor::CborEvent::Init`. Streamplace records the catalog
    /// in its DB and derives per-track init segments for HLS playback.
    #[serde(rename = "init")]
    Init {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        #[serde(default)]
        catalog: Option<Catalog>,
        #[serde(default)]
        track_inits: BTreeMap<String, ByteString>,
    },
    /// One signed canonical-segment-per-track for a single GoP.
    ///
    /// Same metadata shape as `muxl::cbor::CborEvent::Segment` plus
    /// `first_decode_times`. The `tracks` values are signed canonical
    /// segments (c2pa-uuid prefixed muxl-uuid + moof+mdat sequences);
    /// `samples` offsets are shifted to land on sample bytes inside the
    /// signed buffers.
    #[serde(rename = "signed-segment")]
    SignedSegment {
        number: u32,
        tracks: BTreeMap<String, ByteString>,
        #[serde(default)]
        durations: BTreeMap<String, u64>,
        #[serde(default)]
        sample_counts: BTreeMap<String, u32>,
        #[serde(default)]
        samples: BTreeMap<String, CborTrackSamples>,
        #[serde(default)]
        first_decode_times: BTreeMap<String, u64>,
        #[serde(default)]
        body_size: u64,
        #[serde(default)]
        duration_us: u64,
    },
}

impl SignedEvent {
    /// Build a `SignedSegment` event from a [`GopSegment`] whose
    /// per-track `tracks` map already contains signed canonical-segment
    /// bytes (c2pa-uuid prefixed). `samples.offsets_in_track` must be
    /// pre-shifted past the c2pa-uuid prefix.
    pub fn signed_from_gop(gop: GopSegment) -> Self {
        SignedEvent::SignedSegment {
            number: gop.number,
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
            // `first_decode_times` would come from the upstream segmenter
            // when it knows; for now we leave it empty (Streamplace already
            // tracks first-segment tfdts through its DB rows).
            first_decode_times: BTreeMap::new(),
            body_size: gop.body_size,
            duration_us: gop.duration_us,
        }
    }
}
