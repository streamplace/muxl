#![cfg_attr(feature = "wasm", feature(stdarch_wasm_atomic_wait))]

pub mod catalog;
pub mod cbor;
pub mod cid;
pub mod concat;
mod error;
pub mod flat;
pub mod fmp4;
mod fragment;
pub mod hls;
pub mod init;
pub mod io;
pub mod push;
pub mod reader;
pub mod segment;
pub mod source;
#[cfg(feature = "wasm")]
mod wasm;
#[cfg(feature = "wasm")]
mod wasm_io;

pub use error::{Error, Result};
pub use source::{Plan, Sample, Source, TrackPlan};
pub use concat::Concatenator;
pub use push::{Segmenter, SegmenterEvent};
pub use segment::{GopSegment, Segment, segment_fmp4};

// Fragment primitives kept for power users (`Frame`, per-track directory
// emitter, stats). The common fMP4 streaming reader now lives at
// `muxl::fmp4::read_stream` + `muxl::fmp4::StreamReader`.
pub use fragment::{FragmentStats, Frame, TrackStats, fragment_to_directory, fragment_track};

// Flat MP4 write metadata — shared with the HLS module for byte-range
// playlists. The reader/writer entry points live in `muxl::flat::{read, write}`.
// `SegmentMetadata` + `build_synth_flat_header` is the metadata-only path
// for synthesizing a multi-segment flat MP4 header without holding
// sample bytes; see Streamplace's livestream-to-VOD pipeline.
pub use flat::{
    FlatFragment, FlatMp4Info, FlatTrackInfo, SegmentMetadata, build_synth_flat_header,
};

pub mod cli;
pub use cli::cli_main;

// ---------------------------------------------------------------------------
// Top-level convenience API
// ---------------------------------------------------------------------------

/// Read any supported MP4 wrapper (fMP4 or flat MP4) into a [`Source`].
///
/// Auto-detects the layout. Equivalent to [`flat::read`], which itself
/// dispatches to [`fmp4::read`] on fragmented inputs.
pub fn read<R: io::ReadAt + ?Sized>(input: &R) -> Result<Source> {
    flat::read(input)
}
