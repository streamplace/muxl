//! `Source` and `Plan` — the wrapper-agnostic in-memory view of an MP4
//! input that any of the three codec representations (CBOR catalog, fMP4
//! init, flat MP4 header) can be produced from.
//!
//! A source carries two things:
//!
//! - [`Catalog`] — codec configuration for every track (what's the codec,
//!   dimensions, sample rate, timescale, track id). See `src/catalog.rs`.
//! - [`Plan`] — the per-sample layout: durations, sizes, sync flags, cts
//!   offsets, and the sample's byte offset in the *input*. No sample bytes
//!   live in a `Plan`; they're streamed from the input at write time.
//!
//! Because a `Plan` stores only metadata, even a 24-hour source sits at a
//! bounded memory cost (≈24 B/sample; ~120 MB for a 24 h/60 fps video).
//! Sample payload is always read on-demand from the original input, so
//! write paths are streaming from the input side and from the output side.
//!
//! Every reader (`muxl::read`, `fmp4::read`, `flat::read`) returns a
//! `Source`; every writer (`fmp4::write`, `flat::write`) takes one.
//! Convert flat → fMP4 is `fmp4::write(&flat::read(input)?, input, out)`.

use crate::catalog::Catalog;

/// In-memory view of an MP4 input — catalog plus a sample plan that can
/// be re-emitted into any wrapper.
#[derive(Debug, Clone)]
pub struct Source {
    /// Codec configuration for every track.
    pub catalog: Catalog,
    /// Per-track sample plan.
    pub plan: Plan,
}

impl Source {
    /// Return a new `Source` whose catalog and plan contain only the
    /// requested track. Useful for emitting per-track flat MP4s from a
    /// multi-track input — the resulting source can be passed to
    /// [`crate::flat::write`] verbatim.
    ///
    /// Returns `None` if no track has the given id.
    pub fn filter_to_track(&self, track_id: u32) -> Option<Source> {
        let track = self.plan.track(track_id)?.clone();
        Some(Source {
            catalog: self.catalog.filter_to_track(track_id),
            plan: Plan { tracks: vec![track] },
        })
    }
}

/// Per-track sample plans in track-id order.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub tracks: Vec<TrackPlan>,
}

impl Plan {
    pub fn new(tracks: Vec<TrackPlan>) -> Self {
        let mut tracks = tracks;
        tracks.sort_by_key(|t| t.track_id);
        Self { tracks }
    }

    /// Find a track plan by `track_id`.
    pub fn track(&self, track_id: u32) -> Option<&TrackPlan> {
        self.tracks.iter().find(|t| t.track_id == track_id)
    }
}

/// One track's sample plan — metadata only, no sample bytes.
#[derive(Debug, Clone)]
pub struct TrackPlan {
    pub track_id: u32,
    /// `true` for video tracks, `false` for audio/other.
    pub is_video: bool,
    /// Media timescale (ticks per second) — matches the track's `mdhd`.
    pub timescale: u32,
    /// Presentation start offset in the track's media timescale. Baked
    /// into the first fragment's `tfdt` on write, and into a synthesized
    /// canonical `elst` for the flat MP4 moov. Source file leading
    /// empty-edit → this value. See `spec/canonical-form.md § edts/elst`.
    pub start_offset_ticks: u64,
    /// Samples in decode order.
    pub samples: Vec<Sample>,
}

/// Per-sample metadata. 24 B/sample (plus align) — a 24 h/60 fps video
/// is ~120 MB of `Sample` records.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Sample duration in the track's media timescale.
    pub duration: u32,
    /// Encoded sample size in bytes.
    pub size: u32,
    /// Sync (key) frame flag.
    pub is_sync: bool,
    /// Composition-time offset (decode time → presentation time) in the
    /// track's media timescale. Zero for audio and for video without
    /// B-frames.
    pub cts_offset: i32,
    /// Byte offset of this sample's encoded data in the *original* input.
    /// The writer streams these bytes through via `ReadAt::read_at`.
    pub input_offset: u64,
}
