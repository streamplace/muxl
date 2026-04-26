//! muxl-sign — per-track C2PA signing for MUXL flat MP4s.
//!
//! Wraps c2pa-rs's `Builder` around `muxl::flat`-emitted per-track flat MP4s,
//! producing a wrapper container that carries each per-track signed asset as
//! a c2pa `Ingredient`. The result is a multi-track flat MP4 whose top-level
//! signature covers the cross-track manifest and whose ingredient manifests
//! verify each track independently — drop a track and the rest still verify.
//!
//! Public API lands in follow-up commits.

// Smoke import: confirms the c2pa-rs streamplace fork resolves and links.
#[doc(hidden)]
pub use c2pa::SigningAlg;
