//! muxl-sign — C2PA/S2PA signing + verification for MUXL canonical segments.
//!
//! Each canonical segment (one track's fragments for one GoP, prefixed by the
//! muxl uuid + DRISL catalog) is signed independently with c2pa-rs as a
//! standalone `.m4s` asset, so each verifies on its own — drop a track or a
//! segment and the rest still verify.
//!
//! Entry points:
//! - [`SignerKey`] — PEM cert chain + private key (or host callback) + alg.
//! - [`sign_segment_stream`] — stream an fMP4 in, emit one CBOR
//!   `signed-segment` event per GoP (per-track signed canonical segments).
//! - [`sign_transcode_segment`] — sign a transcoded segment that declares
//!   the segment it was transcoded from as a `parentOf` ingredient, carrying
//!   a `c2pa.transcoded` action (the Livepeer-orchestrator provenance step).
//! - [`verify_segments`] — validate the signatures on a signed MUXL wrapper.

pub mod cbor;
pub mod cert;
mod cli;
mod error;
pub mod inspect;
mod sign;
mod verify;

pub use c2pa::SigningAlg;
pub use cbor::SignedEvent;
pub use cert::{
    cert_to_pem, did_key_for, generate_cert, generate_cert_with_signer, generate_key, key_to_pem,
};
pub use cli::cli_main;
pub use error::{Error, Result};
pub use sign::{
    SignerKey, TRANSCODE_INGREDIENT_LABEL, sign_segment_stream, sign_segment_stream_host,
    sign_transcode_segment,
};
pub use verify::verify_segments;
