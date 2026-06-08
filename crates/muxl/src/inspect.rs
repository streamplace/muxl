//! Inspection of a MUXL segment file.
//!
//! Walks each canonical segment in a wrapper (bare m4s, fMP4, or flat MP4)
//! and reports per-segment codec info (from the segment's embedded catalog)
//! plus signing info (from c2pa-rs verification, when a manifest is present).
//!
//! Two renderers share one extracted [`Report`]: a colorized human report
//! (default; color when stdout is a TTY) and a `--json` machine form. With
//! `--manifests` each segment also carries the full embedded C2PA manifest
//! store — printed under the human report, or nested in the JSON.

use std::fmt::Display;
use std::io::{self, Cursor, IsTerminal, Write};
use std::path::Path;

use c2pa::{Manifest, Reader, ValidationState};
use serde::Serialize;
use serde_json::Value;
use x509_cert::Certificate;

use crate::error::Result;
use crate::sign::init_default_settings;

/// What to include in (and how to render) an inspection report.
#[derive(Clone, Copy, Default)]
pub struct InspectOptions {
    /// Emit the report as JSON instead of the colorized human form.
    pub json: bool,
    /// Include the full embedded C2PA manifest store for each segment.
    pub manifests: bool,
}

/// Inspect a file on disk and write the report to stdout. The human form is
/// colorized when stdout is a terminal; `--json` is never colorized.
pub fn inspect_file(path: &Path, opts: InspectOptions) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let color = !opts.json && out.is_terminal();
    inspect_to(&bytes, path, &mut out, color, opts)
}

/// Pure entry point — write the report for `bytes` (named `path` in the
/// header) to `out`. Renders JSON when `opts.json`, otherwise the human form
/// with optional ANSI colorization.
pub fn inspect_to<W: Write>(
    bytes: &[u8],
    path: &Path,
    out: &mut W,
    color: bool,
    opts: InspectOptions,
) -> Result<()> {
    init_default_settings();
    let report = build_report(bytes, path, opts)?;
    if opts.json {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        writeln!(out, "{json}")?;
    } else {
        render_human(&report, out, &Style { color })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Report model — the single extracted form both renderers consume. Serialized
// verbatim for `--json`; walked field-by-field for the human report.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Report {
    file: String,
    bytes: usize,
    segment_count: usize,
    segments: Vec<SegmentReport>,
}

#[derive(Serialize)]
struct SegmentReport {
    /// 1-based position in the wrapper.
    index: usize,
    track_id: u32,
    bytes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    video: Vec<VideoReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    audio: Vec<AudioReport>,
    signing: SigningReport,
    /// Full embedded C2PA manifest store (only with `--manifests`, and only
    /// when the segment carries one).
    #[serde(skip_serializing_if = "Option::is_none")]
    manifests: Option<Value>,
}

#[derive(Serialize)]
struct VideoReport {
    codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    friendly: Option<&'static str>,
    coded_width: u32,
    coded_height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    framerate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_aspect_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_aspect_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitrate: Option<u64>,
    timescale: u32,
    description_bytes: usize,
}

#[derive(Serialize)]
struct AudioReport {
    codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    friendly: Option<&'static str>,
    sample_rate: u32,
    channels: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitrate: Option<u64>,
    timescale: u32,
    description_bytes: usize,
}

#[derive(Serialize, Default)]
struct SigningReport {
    /// `unsigned` · `no_active_manifest` · `trusted` · `valid` · `invalid`.
    state: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failure_codes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    when: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,
    /// Leaf cert public key, full lowercase hex (abbreviated in the human form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pubkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dc_creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dc_date: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ingredients: Vec<IngredientReport>,
}

#[derive(Serialize)]
struct IngredientReport {
    relationship: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
}

/// Walk the wrapper's canonical segments and extract the full report.
fn build_report(bytes: &[u8], path: &Path, opts: InspectOptions) -> Result<Report> {
    let segments = muxl::reader::unwrap(bytes)?;
    let segment_count = segments.len();
    let mut out = Vec::with_capacity(segment_count);
    for (idx, seg) in segments.iter().enumerate() {
        let (signing, manifests) = analyze_signing(seg.data, opts.manifests);
        out.push(SegmentReport {
            index: idx + 1,
            track_id: seg.track_id,
            bytes: seg.data.len(),
            video: seg.catalog.video_configs().map(video_report).collect(),
            audio: seg.catalog.audio_configs().map(audio_report).collect(),
            signing,
            manifests,
        });
    }
    Ok(Report {
        file: path.display().to_string(),
        bytes: bytes.len(),
        segment_count,
        segments: out,
    })
}

fn video_report(v: &muxl::catalog::VideoConfig) -> VideoReport {
    VideoReport {
        codec: v.codec.clone(),
        friendly: friendly_codec(&v.codec),
        coded_width: v.coded_width,
        coded_height: v.coded_height,
        framerate: v.framerate,
        display_aspect_width: v.display_aspect_width,
        display_aspect_height: v.display_aspect_height,
        bitrate: v.bitrate,
        timescale: v.timescale(),
        description_bytes: v.description.len(),
    }
}

fn audio_report(a: &muxl::catalog::AudioConfig) -> AudioReport {
    AudioReport {
        codec: a.codec.clone(),
        friendly: friendly_codec(&a.codec),
        sample_rate: a.sample_rate,
        channels: a.number_of_channels,
        bitrate: a.bitrate,
        timescale: a.timescale(),
        description_bytes: a.description.len(),
    }
}

/// Verify the segment as a standalone `.m4s` C2PA asset and extract its
/// signing report. Returns the report plus, when `want_manifests`, the full
/// embedded manifest store as JSON. A segment with no JUMBF / no c2pa box is
/// reported `unsigned` rather than erroring — the common "what is this file"
/// case for plain MUXL segments.
fn analyze_signing(seg_bytes: &[u8], want_manifests: bool) -> (SigningReport, Option<Value>) {
    let reader = match Reader::from_stream("m4s", Cursor::new(seg_bytes)) {
        Ok(r) => r,
        Err(_) => return (state_report("unsigned"), None),
    };
    // The manifest store dump is the standard c2pa Reader JSON (manifests +
    // validation), parsed back into a Value so it nests in `--json`.
    let manifests = if want_manifests {
        serde_json::from_str::<Value>(&reader.json()).ok()
    } else {
        None
    };
    let Some(manifest) = reader.active_manifest() else {
        return (state_report("no_active_manifest"), manifests);
    };

    let (state, failure_codes) = match reader.validation_state() {
        ValidationState::Trusted => ("trusted", Vec::new()),
        ValidationState::Valid => ("valid", Vec::new()),
        ValidationState::Invalid => {
            let codes = reader
                .validation_results()
                .and_then(|vr| vr.active_manifest())
                .map(|sc| sc.failure.iter().map(|f| f.code().to_string()).collect())
                .unwrap_or_default();
            ("invalid", codes)
        }
    };

    let sig_info = manifest.signature_info();
    let cert_chain = sig_info.map(|si| si.cert_chain());
    let (creator, date) = cawg_creator_and_date(manifest).unwrap_or((None, None));

    let report = SigningReport {
        state,
        failure_codes,
        title: manifest.title().map(String::from),
        alg: sig_info.and_then(|si| si.alg).map(|a| format!("{a:?}")),
        when: sig_info.and_then(|si| si.time.clone()),
        signer: cert_chain.and_then(leaf_subject_cn),
        pubkey: cert_chain.and_then(leaf_pubkey_hex),
        dc_creator: creator,
        dc_date: date,
        actions: action_names(manifest),
        ingredients: manifest
            .ingredients()
            .iter()
            .map(|ing| IngredientReport {
                relationship: format!("{:?}", ing.relationship()),
                title: ing.title().unwrap_or("(untitled)").to_string(),
                hash: ing.hash().filter(|h| !h.is_empty()).map(String::from),
            })
            .collect(),
    };
    (report, manifests)
}

fn state_report(state: &'static str) -> SigningReport {
    SigningReport { state, ..Default::default() }
}

// ---------------------------------------------------------------------------
// Human renderer
// ---------------------------------------------------------------------------

fn render_human<W: Write>(report: &Report, out: &mut W, s: &Style) -> Result<()> {
    writeln!(
        out,
        "{} {} {}",
        s.bold(s.cyan("file:")),
        report.file,
        s.dim(format!(
            "({} bytes, {} segments)",
            report.bytes, report.segment_count
        )),
    )?;

    if report.segments.is_empty() {
        writeln!(out, "  {}", s.yellow("no canonical segments found"))?;
        return Ok(());
    }

    let total = report.segment_count;
    for seg in &report.segments {
        writeln!(out)?;
        writeln!(
            out,
            "{} {}",
            s.bold(s.cyan(format!("── segment {}/{} ──", seg.index, total))),
            s.dim(format!("track {} · {} bytes", seg.track_id, seg.bytes)),
        )?;
        for v in &seg.video {
            render_video(out, s, v)?;
        }
        for a in &seg.audio {
            render_audio(out, s, a)?;
        }
        render_signing(out, s, &seg.signing)?;
        if let Some(manifests) = &seg.manifests {
            render_manifests(out, s, manifests)?;
        }
    }
    Ok(())
}

fn render_video<W: Write>(out: &mut W, s: &Style, v: &VideoReport) -> Result<()> {
    writeln!(
        out,
        "  {} {} {}",
        s.label("video:"),
        s.bold(&v.codec),
        s.dim(v.friendly.map(|f| format!("({f})")).unwrap_or_default()),
    )?;
    writeln!(
        out,
        "  {} {}×{}{}",
        s.label("dims:  "),
        v.coded_width,
        v.coded_height,
        v.framerate.map(|fr| format!(" @ {fr} fps")).unwrap_or_default(),
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
        v.timescale,
        v.description_bytes,
    )?;
    Ok(())
}

fn render_audio<W: Write>(out: &mut W, s: &Style, a: &AudioReport) -> Result<()> {
    writeln!(
        out,
        "  {} {} {}",
        s.label("audio:"),
        s.bold(&a.codec),
        s.dim(a.friendly.map(|f| format!("({f})")).unwrap_or_default()),
    )?;
    writeln!(
        out,
        "  {} {} Hz, {} ch",
        s.label("fmt:   "),
        a.sample_rate,
        a.channels,
    )?;
    if let Some(b) = a.bitrate {
        writeln!(out, "  {} {} bps", s.label("rate:  "), b)?;
    }
    writeln!(
        out,
        "  {} timescale {}, {} desc bytes",
        s.label("cmaf:  "),
        a.timescale,
        a.description_bytes,
    )?;
    Ok(())
}

fn render_signing<W: Write>(out: &mut W, s: &Style, sig: &SigningReport) -> Result<()> {
    match sig.state {
        "unsigned" => {
            writeln!(out, "  {} {}", s.label("sign:  "), s.dim("(unsigned)"))?;
            return Ok(());
        }
        "no_active_manifest" => {
            writeln!(out, "  {} {}", s.label("sign:  "), s.dim("(no active manifest)"))?;
            return Ok(());
        }
        _ => {}
    }

    let (badge, body) = match sig.state {
        "trusted" => (s.green("✓ trusted"), String::new()),
        "valid" => (s.green("✓ valid"), String::new()),
        _ => {
            let extra = if sig.failure_codes.is_empty() {
                String::new()
            } else {
                format!(" ({})", sig.failure_codes.join(", "))
            };
            (s.red("✗ invalid"), extra)
        }
    };
    writeln!(out, "  {} {}{}", s.label("sign:  "), badge, body)?;

    if let Some(title) = &sig.title {
        writeln!(out, "  {} {}", s.label("title: "), title)?;
    }
    if let Some(alg) = &sig.alg {
        writeln!(out, "  {} {}", s.label("alg:   "), alg)?;
    }
    if let Some(when) = &sig.when {
        writeln!(out, "  {} {}", s.label("when:  "), when)?;
    }
    if let Some(signer) = &sig.signer {
        writeln!(out, "  {} {}", s.label("signer:"), signer)?;
    }
    if let Some(pk) = &sig.pubkey {
        writeln!(out, "  {} {}", s.label("pubkey:"), short_str(pk))?;
    }
    // cawg.metadata carries a presentational identity (dc:creator) and the
    // segment's wall-clock (dc:date) — distinct from the cert subject (the
    // signer's did:key). Streamplace surfaces both.
    if let Some(c) = &sig.dc_creator {
        writeln!(out, "  {} {}", s.label("dc:cr: "), c)?;
    }
    if let Some(d) = &sig.dc_date {
        writeln!(out, "  {} {}", s.label("dc:dt: "), d)?;
    }
    if !sig.actions.is_empty() {
        writeln!(out, "  {} {}", s.label("acts:  "), sig.actions.join(", "))?;
    }
    if !sig.ingredients.is_empty() {
        writeln!(out, "  {}", s.label("ingr:  "))?;
        for ing in &sig.ingredients {
            let hash_suffix = ing
                .hash
                .as_deref()
                .map(|h| format!("  {}", s.dim(short_str(h))))
                .unwrap_or_default();
            writeln!(
                out,
                "    {} {}{}",
                s.dim(format!("- {}", ing.relationship)),
                ing.title,
                hash_suffix
            )?;
        }
    }
    Ok(())
}

/// Print the full embedded manifest store under the segment, one indented
/// line per line of the pretty JSON.
fn render_manifests<W: Write>(out: &mut W, s: &Style, manifests: &Value) -> Result<()> {
    writeln!(out, "  {}", s.label("manifest store:"))?;
    let pretty = serde_json::to_string_pretty(manifests)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    for line in pretty.lines() {
        writeln!(out, "    {}", s.dim(line))?;
    }
    Ok(())
}

/// Friendly label for a WebCodecs-style codec string. `None` for unknown
/// codecs (the codec ID itself already prints).
fn friendly_codec(codec: &str) -> Option<&'static str> {
    let lower = codec.to_ascii_lowercase();
    if lower.starts_with("avc1.") || lower.starts_with("avc3.") {
        Some("H.264 / AVC")
    } else if lower.starts_with("hev1.") || lower.starts_with("hvc1.") {
        Some("H.265 / HEVC")
    } else if lower.starts_with("av01.") {
        Some("AV1")
    } else if lower.starts_with("vp09.") || lower == "vp9" {
        Some("VP9")
    } else if lower.starts_with("vp08.") || lower == "vp8" {
        Some("VP8")
    } else if lower.starts_with("mp4a.40.") {
        Some("AAC")
    } else if lower == "opus" {
        Some("Opus")
    } else if lower == "flac" {
        Some("FLAC")
    } else {
        None
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
