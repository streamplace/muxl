//! WebAssembly bindings for the MUXL segmenter.
//!
//! Exposes a `WasmSegmenter` class to JavaScript via wasm-bindgen.
//!
//! ```js
//! import init, { WasmSegmenter } from './muxl.js';
//! await init();
//!
//! const segmenter = new WasmSegmenter();
//! const response = await fetch('stream.mp4');
//! const reader = response.body.getReader();
//!
//! while (true) {
//!   const { done, value } = await reader.read();
//!   if (done) break;
//!   const events = segmenter.feed(value);
//!   for (const event of events) {
//!     if (event.type === 'init') {
//!       // event.data is a Uint8Array with the canonical init segment
//!     } else if (event.type === 'segment') {
//!       // event.number is the segment number
//!       // event.data is a Uint8Array with the segment bytes
//!     }
//!   }
//! }
//! // Flush any remaining partial segment
//! const final_events = segmenter.flush();
//! ```

use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;

use crate::push::{Segmenter, SegmenterEvent};
use crate::wasm_io::{WasmReadAt, WasmWriteAt};

/// MUXL streaming segmenter for WebAssembly.
///
/// Feed fMP4 chunks via `feed()`, receive init segments and MUXL segments
/// as JavaScript objects.
#[wasm_bindgen]
pub struct WasmSegmenter {
    inner: Segmenter,
}

#[wasm_bindgen]
impl WasmSegmenter {
    /// Create a new segmenter ready to receive fMP4 data.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        WasmSegmenter {
            inner: Segmenter::new(),
        }
    }

    /// Feed a chunk of fMP4 data. Returns an array of event objects.
    ///
    /// Each event is `{ type: "init", data: Uint8Array }` or
    /// `{ type: "segment", number: number, data: Uint8Array }`.
    pub fn feed(&mut self, data: &[u8]) -> Result<Array, JsValue> {
        let events = self
            .inner
            .feed(data)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(events_to_js(events))
    }

    /// Signal end of stream. Returns any remaining partial segment.
    pub fn flush(&mut self) -> Result<Array, JsValue> {
        let events = self
            .inner
            .flush()
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(events_to_js(events))
    }
}

/// Convert a flat MP4 to a MUXL fMP4, streaming I/O through WASM linear memory.
///
/// Before calling, JS must:
/// 1. Write the file size (u64 LE) at `read_buf_offset() + 16`
/// 2. Be ready to serve read requests on the read buffer
/// 3. Be ready to drain write chunks from the write buffer
///
/// Returns a JSON string containing the track metadata (codecs, segments,
/// init CIDs, byte offsets). Init segment data is written through the write
/// buffer after the MUXL fMP4 / flat MP4 data, prefixed by a 4-byte LE length per track.
#[wasm_bindgen]
pub fn convert_flat_mp4() -> Result<String, JsValue> {
    let reader = WasmReadAt::new()
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    let mut writer = WasmWriteAt::new()
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    let tracks = crate::flat_mp4_to_fmp4(&reader, &mut writer)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    // Write init segments to the output stream so the main thread can upload them.
    // Format: [4-byte LE length][init data] for each track, in order.
    use std::io::Write;
    for track in &tracks {
        let len = (track.init_data.len() as u32).to_le_bytes();
        writer.write_all(&len).map_err(|e| JsValue::from_str(&e.to_string()))?;
        writer.write_all(&track.init_data).map_err(|e| JsValue::from_str(&e.to_string()))?;
    }

    // Signal end of stream
    writer.finish();

    // Return track metadata as JSON (small — just offsets, codecs, CIDs)
    let json = serde_json::to_string(&tracks)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(json)
}

fn events_to_js(events: Vec<SegmenterEvent>) -> Array {
    let arr = Array::new();
    for event in events {
        let obj = Object::new();
        match event {
            SegmenterEvent::InitSegment { data, .. } => {
                Reflect::set(&obj, &"type".into(), &"init".into()).unwrap();
                let buf = Uint8Array::from(data.as_slice());
                Reflect::set(&obj, &"data".into(), &buf).unwrap();
            }
            SegmenterEvent::Segment(seg) => {
                Reflect::set(&obj, &"type".into(), &"segment".into()).unwrap();
                Reflect::set(&obj, &"number".into(), &JsValue::from(seg.number)).unwrap();
                // Concatenate all track data for this GOP
                let mut all_data = Vec::new();
                for data in seg.tracks.values() {
                    all_data.extend_from_slice(data);
                }
                let buf = Uint8Array::from(all_data.as_slice());
                Reflect::set(&obj, &"data".into(), &buf).unwrap();
            }
        }
        arr.push(&obj);
    }
    arr
}
