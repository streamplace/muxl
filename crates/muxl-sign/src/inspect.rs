//! Human-readable inspection of a MUXL segment file.
//!
//! Walks each canonical segment in a wrapper (bare m4s, fMP4, or flat MP4)
//! and prints per-segment codec info (from the segment's embedded catalog)
//! plus signing info (from c2pa-rs verification, when a manifest is present).
//! Output is colorized when stdout is a TTY.

use std::fmt::Display;
use std::io::{self, Cursor, IsTerminal, Write};
use std::path::Path;

use c2pa::{Manifest, Reader, ValidationState};
use serde_json::Value;
use x509_cert::Certificate;

use crate::error::Result;
use crate::sign::init_default_settings;

/// Inspect a file on disk and write the report to stdout. Colorized when
/// stdout is a terminal.
pub fn inspect_file(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let color = out.is_terminal();
    inspect_to(&bytes, path, &mut out, color)
}

/// Pure entry point — write the report for `bytes` (named `path` in the
/// header) to `out`, with optional ANSI colorization.
pub fn inspect_to<W: Write>(bytes: &[u8], path: &Path, out: &mut W, color: bool) -> Result<()> {
    init_default_settings();
    let s = Style { color };
    let segments = muxl::reader::unwrap(bytes)?;

    writeln!(
        out,
        "{} {} {}",
        s.bold(s.cyan("file:")),
        path.display(),
        s.dim(format!("({} bytes, {} segments)", bytes.len(), segments.len())),
    )?;

    if segments.is_empty() {
        writeln!(out, "  {}", s.yellow("no canonical segments found"))?;
        return Ok(());
    }

    let total = segments.len();
    for (idx, seg) in segments.iter().enumerate() {
        writeln!(out)?;
        writeln!(
            out,
            "{} {}",
            s.bold(s.cyan(format!("── segment {}/{} ──", idx + 1, total))),
            s.dim(format!("track {} · {} bytes", seg.track_id, seg.data.len())),
        )?;
        print_codec(out, &s, seg)?;
        print_signing(out, &s, seg.data)?;
    }
    Ok(())
}

/// Print the per-track codec block for a single segment, using the catalog
/// embedded in the segment's MUXL uuid box.
fn print_codec<W: Write>(out: &mut W, s: &Style, seg: &muxl::reader::Segment<'_>) -> Result<()> {
    for v in seg.catalog.video_configs() {
        let friendly = friendly_codec(&v.codec);
        writeln!(
            out,
            "  {} {} {}",
            s.label("video:"),
            s.bold(&v.codec),
            s.dim(friendly),
        )?;
        writeln!(
            out,
            "  {} {}×{}{}",
            s.label("dims:  "),
            v.coded_width,
            v.coded_height,
            v.framerate
                .map(|fr| format!(" @ {fr} fps"))
                .unwrap_or_default(),
        )?;
        if let (Some(w), Some(h)) = (v.display_aspect_width, v.display_aspect_height) {
            writeln!(out, "  {} {}:{}", s.label("dar:   "), w, h)?;
        }
        if let Some(b) = v.bitrate {
            writeln!(out, "  {} {} bps", s.label("rate:  "), b)?;
        }
        writeln!(
            out,
            "  {} timescale {}, {} desc bytes",
            s.label("cmaf:  "),
            v.timescale(),
            v.description.len(),
        )?;
    }
    for a in seg.catalog.audio_configs() {
        let friendly = friendly_codec(&a.codec);
        writeln!(
            out,
            "  {} {} {}",
            s.label("audio:"),
            s.bold(&a.codec),
            s.dim(friendly),
        )?;
        writeln!(
            out,
            "  {} {} Hz, {} ch",
            s.label("fmt:   "),
            a.sample_rate,
            a.number_of_channels,
        )?;
        if let Some(b) = a.bitrate {
            writeln!(out, "  {} {} bps", s.label("rate:  "), b)?;
        }
        writeln!(
            out,
            "  {} timescale {}, {} desc bytes",
            s.label("cmaf:  "),
            a.timescale(),
            a.description.len(),
        )?;
    }
    Ok(())
}

/// Try to verify the segment as a standalone `.m4s` C2PA asset and print
/// either the signing block (manifest + cert + validity) or "unsigned"
/// when no manifest is present.
fn print_signing<W: Write>(out: &mut W, s: &Style, seg_bytes: &[u8]) -> Result<()> {
    let reader = match Reader::from_stream("m4s", Cursor::new(seg_bytes)) {
        Ok(r) => r,
        // No JUMBF / not a c2pa-signed segment. Treat as unsigned rather
        // than a hard error — that's the most common "what is this file"
        // outcome for plain MUXL segments.
        Err(_) => {
            writeln!(out, "  {} {}", s.label("sign:  "), s.dim("(unsigned)"))?;
            return Ok(());
        }
    };
    let Some(manifest) = reader.active_manifest() else {
        writeln!(out, "  {} {}", s.label("sign:  "), s.dim("(no active manifest)"))?;
        return Ok(());
    };
    let state = reader.validation_state();
    let (badge, body) = match state {
        ValidationState::Trusted => (s.green("✓ trusted"), String::new()),
        ValidationState::Valid => (s.green("✓ valid"), String::new()),
        ValidationState::Invalid => {
            // Surface the failure code(s) so the report is self-explanatory.
            let codes: Vec<String> = reader
                .validation_results()
                .and_then(|vr| vr.active_manifest())
                .map(|sc| sc.failure.iter().map(|f| f.code().to_string()).collect())
                .unwrap_or_default();
            let extra = if codes.is_empty() {
                String::new()
            } else {
                format!(" ({})", codes.join(", "))
            };
            (s.red("✗ invalid"), extra)
        }
    };
    writeln!(out, "  {} {}{}", s.label("sign:  "), badge, body)?;

    if let Some(title) = manifest.title() {
        writeln!(out, "  {} {}", s.label("title: "), title)?;
    }
    let sig_info = manifest.signature_info();
    if let Some(alg) = sig_info.and_then(|si| si.alg) {
        writeln!(out, "  {} {:?}", s.label("alg:   "), alg)?;
    }
    if let Some(when) = sig_info.and_then(|si| si.time.clone()) {
        writeln!(out, "  {} {}", s.label("when:  "), when)?;
    }
    if let Some(cert_chain) = sig_info.map(|si| si.cert_chain()) {
        if let Some(subject) = leaf_subject_cn(cert_chain) {
            writeln!(out, "  {} {}", s.label("signer:"), subject)?;
        }
        if let Some(pk) = leaf_pubkey_hex(cert_chain) {
            writeln!(out, "  {} {}", s.label("pubkey:"), short_hex(&pk))?;
        }
    }
    // cawg.metadata carries a presentational identity (dc:creator) and the
    // segment's wall-clock (dc:date). Distinct from the cert subject (the
    // signer's did:key) — Streamplace surfaces both.
    if let Some((creator, date)) = cawg_creator_and_date(manifest) {
        if let Some(c) = creator {
            writeln!(out, "  {} {}", s.label("dc:cr: "), c)?;
        }
        if let Some(d) = date {
            writeln!(out, "  {} {}", s.label("dc:dt: "), d)?;
        }
    }
    let actions = action_names(manifest);
    if !actions.is_empty() {
        writeln!(out, "  {} {}", s.label("acts:  "), actions.join(", "))?;
    }
    if !manifest.ingredients().is_empty() {
        writeln!(out, "  {}", s.label("ingr:  "))?;
        for ing in manifest.ingredients() {
            let title = ing.title().unwrap_or("(untitled)");
            let rel = format!("{:?}", ing.relationship());
            let hash_suffix = ing
                .hash()
                .filter(|h| !h.is_empty())
                .map(|h| format!("  {}", s.dim(short_str(h))))
                .unwrap_or_default();
            writeln!(out, "    {} {}{}", s.dim(format!("- {rel}")), title, hash_suffix)?;
        }
    }
    Ok(())
}

/// Friendly label for a WebCodecs-style codec string. Returns an empty
/// string for unknown codecs (the codec ID itself already prints).
fn friendly_codec(codec: &str) -> &'static str {
    let lower = codec.to_ascii_lowercase();
    if lower.starts_with("avc1.") || lower.starts_with("avc3.") {
        "(H.264 / AVC)"
    } else if lower.starts_with("hev1.") || lower.starts_with("hvc1.") {
        "(H.265 / HEVC)"
    } else if lower.starts_with("av01.") {
        "(AV1)"
    } else if lower.starts_with("vp09.") || lower == "vp9" {
        "(VP9)"
    } else if lower.starts_with("vp08.") || lower == "vp8" {
        "(VP8)"
    } else if lower.starts_with("mp4a.40.") {
        "(AAC)"
    } else if lower == "opus" {
        "(Opus)"
    } else if lower == "flac" {
        "(FLAC)"
    } else {
        ""
    }
}

/// Extract the leaf cert's Subject CN from a PEM cert chain. The S2PA
/// leaf cert puts the signer's DID in CN (`CN=did:key:...` or `CN=did:web:...`),
/// so this is the human-readable signer identity.
fn leaf_subject_cn(pem: &str) -> Option<String> {
    let chain = Certificate::load_pem_chain(pem.as_bytes()).ok()?;
    let leaf = chain.first()?;
    // The Subject's RDN list is small (CN + maybe O); walk it for CN.
    let subj = leaf.tbs_certificate.subject.to_string();
    // Name's Display gives RFC 4514 form like "CN=did:key:...,O=Streamplace".
    // Pull the CN value out by hand — it's the most useful single field.
    for part in subj.split(',') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("CN=") {
            return Some(rest.to_string());
        }
    }
    Some(subj)
}

/// Leaf cert public key as lowercase hex (SEC1 uncompressed for secp256k1).
fn leaf_pubkey_hex(pem: &str) -> Option<String> {
    let chain = Certificate::load_pem_chain(pem.as_bytes()).ok()?;
    let leaf = chain.first()?;
    let bytes = leaf
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes();
    Some(hex_lower(bytes))
}

/// Pull the cawg.metadata `dc:creator` and `dc:date` out of `manifest`,
/// if either is set. Returned as `(creator, date)`.
fn cawg_creator_and_date(manifest: &Manifest) -> Option<(Option<String>, Option<String>)> {
    let a = manifest
        .assertions()
        .iter()
        .find(|a| a.label() == "cawg.metadata")?;
    let value: Value = a.value().ok()?.clone();
    let creator = value
        .get("dc:creator")
        .and_then(Value::as_str)
        .map(String::from);
    let date = value
        .get("dc:date")
        .and_then(Value::as_str)
        .map(String::from);
    if creator.is_none() && date.is_none() {
        return None;
    }
    Some((creator, date))
}

/// Collect the `action` strings from any c2pa.actions / c2pa.actions.v2
/// assertion. Used for the "acts:" line.
fn action_names(manifest: &Manifest) -> Vec<String> {
    let mut out = Vec::new();
    for a in manifest.assertions() {
        if a.label() != "c2pa.actions" && a.label() != "c2pa.actions.v2" {
            continue;
        }
        let Ok(value) = a.value() else { continue };
        let Some(actions) = value.get("actions").and_then(Value::as_array) else {
            continue;
        };
        for act in actions {
            if let Some(name) = act.get("action").and_then(Value::as_str) {
                out.push(name.to_string());
            }
        }
    }
    out
}

fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Abbreviate a long hex/identifier string with a center ellipsis. Useful
/// for cert keys and assertion hashes where the prefix is identifying but
/// the full value would wrap.
fn short_hex(s: &str) -> String {
    short_str(s)
}

fn short_str(s: &str) -> String {
    if s.len() <= 24 {
        return s.to_string();
    }
    format!("{}…{}", &s[..16], &s[s.len() - 4..])
}

/// ANSI styling helper. When `color` is false, every method returns the
/// input unchanged so the output stays pipe-friendly.
struct Style {
    color: bool,
}

impl Style {
    fn wrap(&self, code: &str, s: impl Display) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn bold(&self, s: impl Display) -> String {
        self.wrap("1", s)
    }
    fn dim(&self, s: impl Display) -> String {
        self.wrap("2", s)
    }
    fn cyan(&self, s: impl Display) -> String {
        self.wrap("36", s)
    }
    fn green(&self, s: impl Display) -> String {
        self.wrap("32", s)
    }
    fn red(&self, s: impl Display) -> String {
        self.wrap("31", s)
    }
    fn yellow(&self, s: impl Display) -> String {
        self.wrap("33", s)
    }
    /// Left-aligned dim label. Same color treatment as `dim`, separated
    /// to keep call sites readable.
    fn label(&self, s: impl Display) -> String {
        self.dim(s)
    }
}
