//! Wire format for the `muxl-sign segment` streaming protocol.
//!
//! Each event is one DRISL/CBOR value on stdout, framed as:
//!
//! ```cbor
//! {"type": "signed-segment", "number": <u32>, "data": h'<signed flat MP4 bytes>'}
//! ```
//!
//! Mirrors muxl's own `{type, ...}`-tagged CBOR events so a Streamplace-style
//! Go decoder can reuse the same `MuxlEvent` shape and just match a new tag.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignedEvent {
    /// A complete signed flat MP4 covering one GoP — wrapper manifest plus
    /// per-track ingredient manifests for every track in the GoP.
    #[serde(rename = "signed-segment")]
    SignedSegment {
        /// 1-based GoP number, matching the upstream segmenter's numbering.
        number: u32,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
}
