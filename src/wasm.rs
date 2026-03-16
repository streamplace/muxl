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
                let buf = Uint8Array::from(seg.data.as_slice());
                Reflect::set(&obj, &"data".into(), &buf).unwrap();
            }
        }
        arr.push(&obj);
    }
    arr
}
