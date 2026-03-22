pub mod catalog;
pub mod cbor;
pub mod concat;
mod error;
mod fragment;
mod init;
pub mod push;
mod segment;
#[cfg(feature = "wasm")]
mod wasm;

pub use error::{Error, Result};
pub use fragment::{
    FMP4Reader, FragmentStats, Frame, TrackStats, fragment_fmp4, fragment_to_directory,
    fragment_track,
};
pub use init::{build_init_segment, catalog_from_moov, catalog_from_mp4, read_moov};
pub use concat::Concatenator;
pub use push::{Segmenter, SegmenterEvent};
pub use segment::{Segment, segment_fmp4};

mod cli;
pub use cli::cli_main;
