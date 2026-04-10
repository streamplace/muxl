//! Platform-agnostic random access I/O.
//!
//! [`ReadAt`] is the minimal interface that platform bindings must implement:
//! positional reads and a size query. Everything else (streaming conversion,
//! segmentation, HLS generation) is built on top of it.
//!
//! This keeps language/platform bindings thin:
//!   - CLI (Rust):  [`FileReadAt`] wraps `std::fs::File`
//!   - WASM (browser): implements `ReadAt` over SharedArrayBuffer + Atomics
//!   - Mobile (FFI): implements `ReadAt` via a C callback
//!
//! [`ReadAtCursor`] adapts any `ReadAt` into `Read + Seek` so existing code
//! that expects those traits (e.g. `catalog_from_mp4`, `read_moov`) works
//! unchanged.

use std::io::{self, Read, Seek, SeekFrom};

/// Stateless positional read interface.
///
/// Implementations must be safe to call from any position without affecting
/// other concurrent reads — there is no cursor. This maps naturally to
/// `pread(2)`, `Blob.slice()`, `RandomAccessFile.read()`, etc.
pub trait ReadAt {
    /// Total size of the underlying data in bytes.
    fn size(&self) -> io::Result<u64>;

    /// Read up to `buf.len()` bytes starting at `offset`.
    ///
    /// Returns the number of bytes actually read (may be less than `buf.len()`
    /// at EOF). Returns `Ok(0)` if `offset >= size()`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    /// Read exactly `buf.len()` bytes starting at `offset`.
    ///
    /// Returns an error if fewer bytes are available.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut pos = 0;
        while pos < buf.len() {
            let n = self.read_at(offset + pos as u64, &mut buf[pos..])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "read_exact_at: wanted {} bytes at offset {}, got {}",
                        buf.len(),
                        offset,
                        pos
                    ),
                ));
            }
            pos += n;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// File implementation (CLI / native)
// ---------------------------------------------------------------------------

/// [`ReadAt`] backed by a `std::fs::File`.
///
/// Uses `pread` on Unix for truly stateless reads. On other platforms, falls
/// back to `Seek + Read` with a mutex (not yet implemented — Unix-only for
/// now).
pub struct FileReadAt {
    file: std::fs::File,
}

impl FileReadAt {
    pub fn open(path: &std::path::Path) -> io::Result<Self> {
        Ok(Self {
            file: std::fs::File::open(path)?,
        })
    }
}

impl ReadAt for FileReadAt {
    fn size(&self) -> io::Result<u64> {
        self.file.metadata().map(|m| m.len())
    }

    #[cfg(unix)]
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        self.file.read_at(buf, offset)
    }

    #[cfg(not(unix))]
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> io::Result<usize> {
        // TODO: Seek+Read with mutex for Windows
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ReadAt not yet implemented for non-Unix platforms",
        ))
    }
}

// ---------------------------------------------------------------------------
// ReadAt → Read + Seek adapter
// ---------------------------------------------------------------------------

/// Wraps a [`ReadAt`] with a cursor position, providing `Read + Seek`.
///
/// This lets existing code that expects `Read + Seek` (e.g. `catalog_from_mp4`,
/// `read_moov`) work with any `ReadAt` implementation.
pub struct ReadAtCursor<'a, R: ReadAt + ?Sized> {
    inner: &'a R,
    pos: u64,
    size: u64,
}

impl<'a, R: ReadAt + ?Sized> ReadAtCursor<'a, R> {
    pub fn new(inner: &'a R) -> io::Result<Self> {
        let size = inner.size()?;
        Ok(Self {
            inner,
            pos: 0,
            size,
        })
    }
}

impl<R: ReadAt + ?Sized> Read for ReadAtCursor<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: ReadAt + ?Sized> Seek for ReadAtCursor<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.size as i64 + offset,
            SeekFrom::Current(offset) => self.pos as i64 + offset,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative position",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

// ---------------------------------------------------------------------------
// In-memory implementation (tests / small data)
// ---------------------------------------------------------------------------

/// [`ReadAt`] backed by a byte slice. Useful for tests.
impl ReadAt for [u8] {
    fn size(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let offset = offset as usize;
        if offset >= self.len() {
            return Ok(0);
        }
        let available = &self[offset..];
        let n = buf.len().min(available.len());
        buf[..n].copy_from_slice(&available[..n]);
        Ok(n)
    }
}

impl ReadAt for Vec<u8> {
    fn size(&self) -> io::Result<u64> {
        self.as_slice().size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.as_slice().read_at(offset, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slice_read_at() {
        let data = b"hello world";
        let mut buf = [0u8; 5];
        assert_eq!(data.as_slice().read_at(0, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"hello");
        assert_eq!(data.as_slice().read_at(6, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"world");
        assert_eq!(data.as_slice().read_at(11, &mut buf).unwrap(), 0);
    }

    #[test]
    fn test_read_exact_at_eof() {
        let data = b"hi";
        let mut buf = [0u8; 5];
        assert!(data.as_slice().read_exact_at(0, &mut buf).is_err());
    }

    #[test]
    fn test_cursor_read_seek() {
        let data = b"abcdefghij";
        let cursor_result = ReadAtCursor::new(data.as_slice());
        let mut cursor = cursor_result.unwrap();

        let mut buf = [0u8; 3];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"abc");

        cursor.seek(SeekFrom::Start(7)).unwrap();
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hij");

        cursor.seek(SeekFrom::Current(-3)).unwrap();
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hij");

        cursor.seek(SeekFrom::End(-2)).unwrap();
        let mut buf2 = [0u8; 2];
        cursor.read_exact(&mut buf2).unwrap();
        assert_eq!(&buf2, b"ij");
    }
}
