//! WASM I/O implementations using static buffers in WASM linear memory.
//!
//! When built with `+atomics`, WASM linear memory is backed by a
//! `SharedArrayBuffer`. We allocate fixed-size buffers as statics and
//! expose their byte offsets to JS via `read_buf_offset()` / `write_buf_offset()`.
//! Both sides access the same memory:
//! - WASM reads/writes via normal pointers (they point into linear memory)
//! - JS reads/writes via `new Int32Array(wasmMemory.buffer, offset, len)`
//!
//! ## Buffer layout (32-byte header + 1MB data region)
//!
//! ```text
//! [0..4]   i32  status (atomic): IDLE=0, REQUEST=1, RESPONSE=2, ERROR=3, DONE=4
//! [4..12]  u64  arg0: read offset (read buf) or unused (write buf)
//! [12..16] u32  arg1: bytes requested (read buf) or unused (write buf)
//! [16..24] u64  meta: file_size (read, set once by JS) or total_written (write)
//! [24..28] u32  resp: actual bytes in data region
//! [28..32] (reserved)
//! [32..]   data region (1 MB)
//! ```
//!
//! ## Protocol
//!
//! **Read** (WASM requests data from JS):
//!   1. WASM writes offset + size, sets status = REQUEST, notifies, waits
//!   2. JS sees REQUEST, reads file via `Blob.slice()`, copies into data region
//!   3. JS sets resp size, status = RESPONSE, notifies
//!   4. WASM wakes, reads data, sets status = IDLE
//!
//! **Write** (WASM sends data to JS):
//!   1. WASM copies chunk into data region, sets resp = chunk size
//!   2. WASM sets status = REQUEST, notifies, waits
//!   3. JS sees REQUEST, reads chunk from data region (→ BLAKE3 + S3)
//!   4. JS sets status = RESPONSE, notifies
//!   5. WASM wakes, continues. When done, sets status = DONE.

#[cfg(feature = "wasm")]
mod inner {
    use crate::io::ReadAt;
    use std::io;
    use std::sync::atomic::{AtomicI32, Ordering};
    use wasm_bindgen::prelude::*;

    const HEADER_SIZE: usize = 32;
    const DATA_SIZE: usize = 1024 * 1024; // 1 MB
    const BUF_SIZE: usize = HEADER_SIZE + DATA_SIZE;

    // Header offsets
    const OFF_STATUS: usize = 0;
    const OFF_ARG0: usize = 4;
    const OFF_ARG1: usize = 12;
    const OFF_META: usize = 16;
    const OFF_RESP: usize = 24;
    const OFF_DATA: usize = 32;

    // Status values
    const STATUS_IDLE: i32 = 0;
    const STATUS_REQUEST: i32 = 1;
    const STATUS_RESPONSE: i32 = 2;
    const STATUS_ERROR: i32 = 3;
    const STATUS_DONE: i32 = 4;

    // Static buffers in WASM linear memory (shared when built with +atomics)
    static mut READ_BUF: [u8; BUF_SIZE] = [0u8; BUF_SIZE];
    static mut WRITE_BUF: [u8; BUF_SIZE] = [0u8; BUF_SIZE];

    /// Byte offset of the read buffer within WASM linear memory.
    /// JS uses this to create views: `new Int32Array(memory.buffer, offset, 1)`
    #[wasm_bindgen]
    pub fn read_buf_offset() -> u32 {
        unsafe { std::ptr::addr_of!(READ_BUF) as *const u8 as u32 }
    }

    /// Byte offset of the write buffer within WASM linear memory.
    #[wasm_bindgen]
    pub fn write_buf_offset() -> u32 {
        unsafe { std::ptr::addr_of!(WRITE_BUF) as *const u8 as u32 }
    }

    /// Size of the data region in each buffer.
    #[wasm_bindgen]
    pub fn buf_data_size() -> u32 {
        DATA_SIZE as u32
    }

    // Atomic helpers
    fn status_ptr(buf: *mut u8) -> *mut i32 {
        unsafe { buf.add(OFF_STATUS) as *mut i32 }
    }

    fn atomic_wait(buf: *mut u8, expected: i32) {
        unsafe {
            core::arch::wasm32::memory_atomic_wait32(status_ptr(buf), expected, -1);
        }
    }

    fn atomic_store_notify(buf: *mut u8, value: i32) {
        unsafe {
            AtomicI32::from_ptr(status_ptr(buf)).store(value, Ordering::SeqCst);
            core::arch::wasm32::memory_atomic_notify(status_ptr(buf), 1);
        }
    }

    fn atomic_load(buf: *mut u8) -> i32 {
        unsafe { AtomicI32::from_ptr(status_ptr(buf)).load(Ordering::SeqCst) }
    }

    // -----------------------------------------------------------------------
    // WasmReadAt
    // -----------------------------------------------------------------------

    pub struct WasmReadAt {
        file_size: u64,
    }

    impl WasmReadAt {
        pub fn new() -> io::Result<Self> {
            let file_size = unsafe {
                let buf = std::ptr::addr_of_mut!(READ_BUF) as *mut u8;
                let bytes: [u8; 8] = std::ptr::read(buf.add(OFF_META) as *const [u8; 8]);
                u64::from_le_bytes(bytes)
            };
            if file_size == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file_size not set in read buffer",
                ));
            }
            Ok(Self { file_size })
        }
    }

    impl ReadAt for WasmReadAt {
        fn size(&self) -> io::Result<u64> {
            Ok(self.file_size)
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            if offset >= self.file_size {
                return Ok(0);
            }
            let req_size = buf.len().min(DATA_SIZE);
            if req_size == 0 {
                return Ok(0);
            }

            unsafe {
                let rbuf = std::ptr::addr_of_mut!(READ_BUF) as *mut u8;
                std::ptr::write(rbuf.add(OFF_ARG0) as *mut [u8; 8], offset.to_le_bytes());
                std::ptr::write(rbuf.add(OFF_ARG1) as *mut [u8; 4], (req_size as u32).to_le_bytes());

                atomic_store_notify(rbuf, STATUS_REQUEST);
                atomic_wait(rbuf, STATUS_REQUEST);

                let status = atomic_load(rbuf);
                if status == STATUS_ERROR {
                    atomic_store_notify(rbuf, STATUS_IDLE);
                    return Err(io::Error::new(io::ErrorKind::Other, "JS read error"));
                }

                let resp: [u8; 4] = std::ptr::read(rbuf.add(OFF_RESP) as *const [u8; 4]);
                let n = (u32::from_le_bytes(resp) as usize).min(buf.len());
                std::ptr::copy_nonoverlapping(rbuf.add(OFF_DATA), buf.as_mut_ptr(), n);

                atomic_store_notify(rbuf, STATUS_IDLE);
                Ok(n)
            }
        }
    }

    // -----------------------------------------------------------------------
    // WasmWriteAt
    // -----------------------------------------------------------------------

    pub struct WasmWriteAt {
        total_written: u64,
    }

    impl WasmWriteAt {
        pub fn new() -> io::Result<Self> {
            unsafe {
                AtomicI32::from_ptr(status_ptr(std::ptr::addr_of_mut!(WRITE_BUF) as *mut u8))
                    .store(STATUS_IDLE, Ordering::SeqCst);
            }
            Ok(Self { total_written: 0 })
        }

        pub fn finish(&self) {
            unsafe { atomic_store_notify(std::ptr::addr_of_mut!(WRITE_BUF) as *mut u8, STATUS_DONE); }
        }
    }

    impl io::Write for WasmWriteAt {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let chunk_size = buf.len().min(DATA_SIZE);

            unsafe {
                let wbuf = std::ptr::addr_of_mut!(WRITE_BUF) as *mut u8;

                std::ptr::copy_nonoverlapping(buf.as_ptr(), wbuf.add(OFF_DATA), chunk_size);
                std::ptr::write(wbuf.add(OFF_RESP) as *mut [u8; 4], (chunk_size as u32).to_le_bytes());

                self.total_written += chunk_size as u64;
                std::ptr::write(wbuf.add(OFF_META) as *mut [u8; 8], self.total_written.to_le_bytes());

                atomic_store_notify(wbuf, STATUS_REQUEST);
                atomic_wait(wbuf, STATUS_REQUEST);

                let status = atomic_load(wbuf);
                if status == STATUS_ERROR {
                    return Err(io::Error::new(io::ErrorKind::Other, "JS write consumer error"));
                }
            }

            Ok(chunk_size)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(feature = "wasm")]
pub use inner::{WasmReadAt, WasmWriteAt};
