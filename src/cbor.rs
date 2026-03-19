//! CBOR (DRISL) serialization for MUXL streaming events.
//!
//! Defines the wire format for the `--stdout` streaming protocol.
//! Each event is a CBOR map written as a separate value in the stream.
//!
//! ```cbor
//! {"type": "init", "data": h'<ftyp+moov bytes>'}
//! {"type": "segment", "data": h'<moof+mdat bytes>'}
//! ```

use serde::{Deserialize, Serialize};

use crate::push::SegmenterEvent;

/// A MUXL streaming event in CBOR-serializable form.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum CborEvent {
    /// Canonical init segment (ftyp+moov).
    #[serde(rename = "init")]
    Init {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// A GOP-aligned MUXL segment.
    #[serde(rename = "segment")]
    Segment {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
}

impl CborEvent {
    /// Convert a [`SegmenterEvent`] reference into a serializable CBOR event.
    pub fn from_event(event: &SegmenterEvent) -> Self {
        match event {
            SegmenterEvent::InitSegment { data, .. } => CborEvent::Init { data: data.clone() },
            SegmenterEvent::Segment(seg) => CborEvent::Segment {
                data: seg.data.clone(),
            },
        }
    }

    /// Convert a [`SegmenterEvent`] into a serializable CBOR event (owned).
    pub fn from_event_owned(event: SegmenterEvent) -> Self {
        match event {
            SegmenterEvent::InitSegment { data, .. } => CborEvent::Init { data },
            SegmenterEvent::Segment(seg) => CborEvent::Segment { data: seg.data },
        }
    }
}
