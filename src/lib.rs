#![cfg_attr(feature = "wasm", feature(stdarch_wasm_atomic_wait))]

pub mod catalog;
pub mod cbor;
pub mod concat;
mod error;
pub mod flat;
mod fragment;
mod init;
pub mod io;
pub mod push;
mod segment;
#[cfg(feature = "wasm")]
mod wasm;
#[cfg(feature = "wasm")]
mod wasm_io;

pub use error::{Error, Result};
pub use fragment::{
    FMP4Reader, FragmentStats, Frame, TrackStats, fragment_fmp4, fragment_to_directory,
    fragment_track,
};
pub use flat::{
    flat_mp4_to_flat, plan_from_flat_mp4, plan_from_fmp4, to_flat, write_flat_mp4, FlatFragment,
    FlatMp4Info, FlatSample, FlatTrackInfo, FlatTrackPlan,
};
pub use init::{build_init_segment, catalog_from_moov, catalog_from_mp4, read_moov};
pub use concat::Concatenator;
pub use push::{Segmenter, SegmenterEvent};
pub use segment::{GopSegment, Segment, segment_fmp4};

mod cli;
pub use cli::{cli_main, flat_mp4_to_fmp4, BlobTrack, BlobSegment};
