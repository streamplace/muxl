pub mod catalog;
mod error;
mod fragment;
mod init;
mod segment;

pub use error::{Error, Result};
pub use fragment::{
    FMP4Reader, FragmentStats, Frame, TrackStats, fragment_fmp4, fragment_to_directory,
    fragment_track,
};
pub use init::{build_init_segment, catalog_from_moov, catalog_from_mp4, read_moov};
pub use segment::{Segment, segment_fmp4};
