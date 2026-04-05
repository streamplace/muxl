//! WASM [`ReadAt`] implementation using SharedArrayBuffer + Atomics.
//!
//! The WASM code runs in a Web Worker and blocks on `Atomics.wait()` when
//! it needs data. The main thread (or another worker) fulfills read requests
//! via `Blob.slice()` and signals completion with `Atomics.notify()`.
//!
//! # SharedArrayBuffer layout
//!
//! The caller allocates a SharedArrayBuffer and passes it to `WasmReadAt::new()`.
//! The buffer is split into a control region and a data region:
//!
//! ```text
//! Offset  Size    Field           Description
//! ------  ------  --------------  -----------------------------------------
//! 0       4       status (i32)    Atomic flag for request/response handshake
//! 4       8       req_offset      Read offset (u64 LE)
//! 12      4       req_size        Bytes requested (u32 LE)
//! 16      8       file_size       Total file size (u64 LE), set once at init
//! 24      4       resp_size       Bytes actually read (u32 LE)
//! 28      4       (reserved)
//! 32      ...     data            Read data (up to SAB.byteLength - 32)
//! ```
//!
//! # Protocol
//!
//! Status values:
//!   0 = IDLE       — no pending request
//!   1 = REQUEST    — WASM has posted a read request, waiting for JS
//!   2 = RESPONSE   — JS has filled the data, WASM may read it
//!   3 = ERROR      — JS encountered an error
//!
//! WASM (worker) side:
//!   1. Write req_offset + req_size
//!   2. Store status = REQUEST
//!   3. Atomics.notify(status)
//!   4. Atomics.wait(status, REQUEST)  — blocks until status != REQUEST
//!   5. Read resp_size + data
//!   6. Store status = IDLE
//!
//! JS (main thread) side:
//!   1. Atomics.wait(status, IDLE)     — blocks until status != IDLE
//!   2. Read req_offset + req_size
//!   3. const buf = await file.slice(offset, offset+size).arrayBuffer()
//!   4. Copy buf into data region
//!   5. Write resp_size
//!   6. Store status = RESPONSE
//!   7. Atomics.notify(status)

#[cfg(feature = "wasm")]
mod inner {
    use crate::io::ReadAt;
    use js_sys::SharedArrayBuffer;
    use std::io;
    use wasm_bindgen::prelude::*;

    const OFFSET_STATUS: usize = 0;
    const OFFSET_REQ_OFFSET: usize = 4;
    const OFFSET_REQ_SIZE: usize = 12;
    const OFFSET_FILE_SIZE: usize = 16;
    const OFFSET_RESP_SIZE: usize = 24;
    const OFFSET_DATA: usize = 32;

    const STATUS_IDLE: i32 = 0;
    const STATUS_REQUEST: i32 = 1;
    const STATUS_RESPONSE: i32 = 2;
    const STATUS_ERROR: i32 = 3;

    /// [`ReadAt`] implementation that delegates reads to the JS main thread
    /// via SharedArrayBuffer + Atomics.
    ///
    /// Must be used from a Web Worker (main thread cannot `Atomics.wait`).
    pub struct WasmReadAt {
        /// Raw pointer to the SharedArrayBuffer's backing memory.
        /// The SAB is kept alive by JS; we access it via raw pointer
        /// because wasm_bindgen doesn't expose typed views over SABs directly.
        sab_ptr: *mut u8,
        sab_len: usize,
        file_size: u64,
    }

    // Safety: WasmReadAt is single-threaded in practice (WASM is single-threaded
    // within a worker). The SharedArrayBuffer is shared with the main thread
    // but we only access it through atomic operations on the status field and
    // through the defined protocol.
    unsafe impl Send for WasmReadAt {}
    unsafe impl Sync for WasmReadAt {}

    impl WasmReadAt {
        /// Create a new `WasmReadAt` from a SharedArrayBuffer.
        ///
        /// The file_size field at offset 16 must already be set by JS before
        /// calling this constructor.
        pub fn new(sab: &SharedArrayBuffer) -> io::Result<Self> {
            let sab_len = sab.byte_length() as usize;
            if sab_len < OFFSET_DATA + 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("SharedArrayBuffer too small: {} bytes (need at least {})", sab_len, OFFSET_DATA + 1),
                ));
            }

            // Get raw pointer to SAB memory
            let sab_ptr = js_sys::Uint8Array::new(sab).as_ptr() as *mut u8;

            // Read file_size from the SAB (set by JS at init)
            let file_size = unsafe {
                let bytes: [u8; 8] = std::ptr::read(sab_ptr.add(OFFSET_FILE_SIZE) as *const [u8; 8]);
                u64::from_le_bytes(bytes)
            };

            Ok(Self {
                sab_ptr,
                sab_len,
                file_size,
            })
        }

        fn max_data_size(&self) -> usize {
            self.sab_len - OFFSET_DATA
        }

        /// Perform an atomic wait on the status field.
        fn atomic_wait(&self, expected: i32) {
            unsafe {
                let status_ptr = self.sab_ptr.add(OFFSET_STATUS) as *const i32;
                // Use core::arch::wasm32::memory_atomic_wait32
                // This blocks the worker thread until status != expected
                core::arch::wasm32::memory_atomic_wait32(
                    status_ptr,
                    expected,
                    -1, // infinite timeout
                );
            }
        }

        /// Atomic store to the status field + notify.
        fn atomic_store_notify(&self, value: i32) {
            unsafe {
                let status_ptr = self.sab_ptr.add(OFFSET_STATUS) as *mut i32;
                core::arch::wasm32::memory_atomic_notify(status_ptr as *mut i32, 1);
                std::sync::atomic::AtomicI32::from_ptr(status_ptr)
                    .store(value, std::sync::atomic::Ordering::SeqCst);
                core::arch::wasm32::memory_atomic_notify(status_ptr as *mut i32, 1);
            }
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

            let req_size = buf.len().min(self.max_data_size());
            if req_size == 0 {
                return Ok(0);
            }

            unsafe {
                // Write request fields
                std::ptr::write(
                    self.sab_ptr.add(OFFSET_REQ_OFFSET) as *mut [u8; 8],
                    offset.to_le_bytes(),
                );
                std::ptr::write(
                    self.sab_ptr.add(OFFSET_REQ_SIZE) as *mut [u8; 4],
                    (req_size as u32).to_le_bytes(),
                );

                // Signal request and wait for response
                self.atomic_store_notify(STATUS_REQUEST);
                self.atomic_wait(STATUS_REQUEST);

                // Check for error
                let status = std::sync::atomic::AtomicI32::from_ptr(
                    self.sab_ptr.add(OFFSET_STATUS) as *mut i32,
                )
                .load(std::sync::atomic::Ordering::SeqCst);

                if status == STATUS_ERROR {
                    self.atomic_store_notify(STATUS_IDLE);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "JS read callback reported an error",
                    ));
                }

                // Read response
                let resp_size_bytes: [u8; 4] =
                    std::ptr::read(self.sab_ptr.add(OFFSET_RESP_SIZE) as *const [u8; 4]);
                let resp_size = u32::from_le_bytes(resp_size_bytes) as usize;
                let n = resp_size.min(buf.len());

                std::ptr::copy_nonoverlapping(self.sab_ptr.add(OFFSET_DATA), buf.as_mut_ptr(), n);

                // Signal idle
                self.atomic_store_notify(STATUS_IDLE);

                Ok(n)
            }
        }
    }
}

#[cfg(feature = "wasm")]
pub use inner::WasmReadAt;
