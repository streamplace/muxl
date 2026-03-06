use std::io::{self, Read, Write};

/// Errors returned by muxl operations.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidMp4(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::InvalidMp4(msg) => write!(f, "invalid MP4: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Transform an arbitrary MP4 into MUXL canonical form.
///
/// Reads a complete MP4 from `input` and writes the canonicalized MP4 to `output`.
/// The output is byte-deterministic: the same logical content always produces
/// identical bytes.
pub fn canonicalize<R: Read, W: Write>(_input: R, _output: W) -> Result<()> {
    todo!("canonicalize: not yet implemented")
}

/// Split a MUXL canonical MP4 into independently-signable segments.
///
/// Reads a canonical MP4 from `input` and writes segments to `output`.
/// The segment format is TBD.
pub fn segment<R: Read, W: Write>(_input: R, _output: W) -> Result<()> {
    todo!("segment: not yet implemented")
}

/// Concatenate MUXL segments into a single canonical MP4.
///
/// Reads segments from `inputs` and writes the combined MP4 to `output`.
/// Per-segment signatures are preserved.
pub fn concatenate<R: Read, W: Write>(_inputs: &mut [R], _output: W) -> Result<()> {
    todo!("concatenate: not yet implemented")
}
