//! muxl-sign — per-track C2PA signing for MUXL flat MP4s.
//!
//! Wraps c2pa-rs's `Builder` around `muxl::flat`-emitted per-track flat MP4s,
//! producing a wrapper container that carries each per-track signed asset as
//! a c2pa `Ingredient`. The result is a multi-track flat MP4 whose top-level
//! signature covers the cross-track manifest and whose ingredient manifests
//! verify each track independently — drop a track and the rest still verify.
//!
//! Entry points:
//! - [`SignerKey`] — PEM cert chain + private key + signing alg.
//! - [`sign_per_track`] — split a multi-track [`muxl::Source`] into per-track
//!   flat MP4s, sign each, and combine into a wrapper signed flat MP4.

mod cli;
mod error;
mod sign;

pub use c2pa::SigningAlg;
pub use cli::cli_main;
pub use error::{Error, Result};
pub use sign::{SignerKey, sign_per_track};
