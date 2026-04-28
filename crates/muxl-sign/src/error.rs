//! Errors returned by `muxl-sign`. Bridges `muxl::Error` and `c2pa::Error`.

use std::io;

#[derive(Debug)]
pub enum Error {
    /// Underlying muxl read/write failure.
    Muxl(muxl::Error),
    /// Underlying c2pa-rs signing or manifest failure.
    C2pa(c2pa::Error),
    /// I/O error against a user-supplied stream or filesystem path.
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Muxl(e) => write!(f, "muxl: {e}"),
            Error::C2pa(e) => write!(f, "c2pa: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<muxl::Error> for Error {
    fn from(e: muxl::Error) -> Self {
        Error::Muxl(e)
    }
}

impl From<c2pa::Error> for Error {
    fn from(e: c2pa::Error) -> Self {
        Error::C2pa(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
