//! HLS playback artifact generation.
//!
//! Given one or more MP4 inputs, [`emit`] writes a directory of
//! CID-addressed blobs (canonical flat MP4s + per-track init segments)
//! plus a JSON metadata document keyed by the primary blob's CID. With
//! `opts.playlists = true`, it also emits a master `.m3u8` and per-track
//! media playlists that point at byte ranges within the flat MP4 blobs.
//!
//! The primary input contributes the "default" renditions; additional
//! inputs can be supplied as sidecars for alternate renditions (e.g.
//! different resolutions). Each input produces exactly one flat MP4 blob
//! regardless of how many tracks it carries.
//!
//! Timed-text (WebVTT) tracks become `TYPE=SUBTITLES` renditions whose media
//! playlists byte-range the `wvtt` fMP4 fragments in the blob, just like the
//! audio and video playlists. Players must support WebVTT-in-fMP4 (`wvtt`) to
//! render them (for example hls.js and Shaka; Apple's native player expects
//! plain `.vtt` subtitle segments).

use std::collections::HashSet;
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::cid;
use crate::error::Result;
use crate::flat::FlatFragment;
use crate::io::FileReadAt;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-track info extracted for HLS — byte-range segments inside the
/// blob plus codec summary.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobTrack {
    pub track_id: u32,
    pub track_type: String, // "video", "audio", or "text"
    pub codec: String,
    pub timescale: u32,
    pub init_cid: String,
    #[serde(skip)]
    pub init_data: Vec<u8>,
    pub blob_cid: String,
    pub blob_size: u64,
    pub segments: Vec<BlobSegment>,
    // video-specific
    pub width: u32,
    pub height: u32,
    // audio-specific
    pub channels: u32,
    pub sample_rate: u32,
    // text-specific: BCP 47 language tag and optional label. Absent for
    // video and audio, so their serialized form is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Codec summary for one track of a catalog, as carried by [`BlobTrack`].
pub(crate) struct TrackSummary {
    pub track_type: &'static str,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub channels: u32,
    pub sample_rate: u32,
    pub language: Option<String>,
    pub label: Option<String>,
}

/// Summarize track `tid` of `catalog` for [`BlobTrack`]. A track id missing
/// from the catalog reports `"unknown"` with empty fields.
pub(crate) fn track_summary(catalog: &Catalog, tid: u32) -> TrackSummary {
    let empty = TrackSummary {
        track_type: "unknown",
        codec: String::new(),
        width: 0,
        height: 0,
        channels: 0,
        sample_rate: 0,
        language: None,
        label: None,
    };
    if let Some(v) = catalog.video_configs().find(|v| v.track_id() == tid) {
        TrackSummary {
            track_type: "video",
            codec: v.codec.clone(),
            width: v.coded_width,
            height: v.coded_height,
            ..empty
        }
    } else if let Some(a) = catalog.audio_configs().find(|a| a.track_id() == tid) {
        TrackSummary {
            track_type: "audio",
            codec: a.codec.clone(),
            channels: a.number_of_channels,
            sample_rate: a.sample_rate,
            ..empty
        }
    } else if let Some(t) = catalog.text_configs().find(|t| t.track_id() == tid) {
        TrackSummary {
            track_type: "text",
            codec: t.codec.clone(),
            language: Some(t.language.clone()),
            label: t.label.clone(),
            ..empty
        }
    } else {
        empty
    }
}

/// Byte-range segment metadata within a flat MP4 blob.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobSegment {
    pub offset: u64,
    pub size: u64,
    pub duration_ticks: u64,
    pub sample_count: u32,
}

/// Options for [`emit`].
#[derive(Debug, Clone, Default)]
pub struct HlsOpts {
    /// Sidecar MP4 inputs for additional renditions.
    pub sidecars: Vec<PathBuf>,
    /// Emit a master `.m3u8` plus per-track media playlists alongside
    /// the blob and JSON metadata.
    pub write_playlists: bool,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Generate HLS artifacts under `output_dir` from a primary input and
/// optional sidecar inputs. Everything is CID-addressed: one primary blob
/// CID prefixes all playlist filenames, so multiple streams can share a
/// directory without colliding.
pub fn emit(primary: &Path, output_dir: &Path, opts: &HlsOpts) -> Result<Vec<BlobTrack>> {
    fs::create_dir_all(output_dir)?;
    let blobs_dir = Some(output_dir);

    let mut all_tracks: Vec<BlobTrack> = analyze_input(primary, blobs_dir)?;
    let primary_blob_cid = all_tracks
        .first()
        .map(|t| t.blob_cid.clone())
        .unwrap_or_default();
    let primary_blob_size = all_tracks.first().map(|t| t.blob_size).unwrap_or(0);

    for sidecar in &opts.sidecars {
        let sidecar_tracks = analyze_input(sidecar, blobs_dir)?;
        all_tracks.extend(sidecar_tracks);
    }

    let entries: Vec<TrackEntry> = all_tracks
        .into_iter()
        .map(|track| {
            let key = if track.blob_cid == primary_blob_cid {
                track.track_id.to_string()
            } else {
                // Disambiguate sidecar tracks with a blob-CID prefix so two
                // sidecars that happen to share track IDs don't collide.
                format!(
                    "{}.{}",
                    &track.blob_cid[..track.blob_cid.len().min(16)],
                    track.track_id
                )
            };
            TrackEntry { key, track }
        })
        .collect();

    if opts.write_playlists {
        write_playlists(output_dir, &primary_blob_cid, &entries)?;
    }

    write_metadata_json(
        output_dir,
        &primary_blob_cid,
        primary_blob_size,
        &entries,
    )?;

    let total_blobs: HashSet<_> = entries.iter().map(|e| &e.track.blob_cid).collect();
    eprintln!(
        "  {} tracks, {} blobs{}",
        entries.len(),
        total_blobs.len(),
        if opts.write_playlists {
            " + static playlists"
        } else {
            ""
        },
    );

    // Print unique CIDs written (init segments first, then blobs).
    let mut printed_cids: HashSet<String> = HashSet::new();
    for entry in &entries {
        let t = &entry.track;
        if printed_cids.insert(t.init_cid.clone()) {
            println!("{}.mp4  init({})", t.init_cid, t.track_type);
        }
    }
    for entry in &entries {
        let t = &entry.track;
        if printed_cids.insert(t.blob_cid.clone()) {
            println!("{}.mp4  blob({} bytes)", t.blob_cid, t.blob_size);
        }
    }

    Ok(entries.into_iter().map(|e| e.track).collect())
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

struct TrackEntry {
    key: String,
    track: BlobTrack,
}

/// Canonicalize an input MP4 to flat form on disk, hash it for a blob
/// CID, write the blob + per-track init segments into `blobs_dir`, and
/// return per-track metadata.
fn analyze_input(path: &Path, blobs_dir: Option<&Path>) -> Result<Vec<BlobTrack>> {
    // Canonicalize to flat — single blob serves both "download as MP4"
    // and "HLS byte-range CMAF source".
    let tmp = tempfile::NamedTempFile::new()?;
    let info = {
        let input = FileReadAt::open(path)?;
        let source = crate::read(&input)?;
        let mut output = BufWriter::new(tmp.as_file());
        let info = crate::flat::write(&source, &input, &mut output)?;
        std::io::Write::flush(&mut output)?;
        info
    };

    let blob_cid = cid::from_file(tmp.path())?;
    let blob_size = fs::metadata(tmp.path())?.len();

    if let Some(bd) = blobs_dir {
        let blob_path = bd.join(format!("{blob_cid}.mp4"));
        if !blob_path.exists() {
            fs::copy(tmp.path(), &blob_path)?;
        }
    }

    // Extract the catalog from the canonical blob so init segments
    // derive from the final form — idempotent regardless of input layout.
    let blob_reader = FileReadAt::open(tmp.path())?;
    let catalog = crate::catalog::from_input(&blob_reader)?;
    let track_inits = crate::fmp4::init_segments_per_track(&catalog)?;
    drop(blob_reader);

    let mut tracks: Vec<BlobTrack> = Vec::new();
    for (&tid, track_info) in &info.tracks {
        // Text (WebVTT) samples are all sync and sparse, so they group by
        // duration exactly like audio.
        let segments = if track_info.is_video {
            group_fragments_video(&track_info.fragments)
        } else {
            group_fragments_audio(&track_info.fragments, track_info.timescale)
        };

        let init_data = track_inits.get(&tid).cloned().unwrap_or_default();
        let init_cid = cid::from_bytes(&init_data);
        if let Some(bd) = blobs_dir {
            let p = bd.join(format!("{init_cid}.mp4"));
            if !p.exists() {
                fs::write(&p, &init_data)?;
            }
        }

        let summary = track_summary(&catalog, tid);
        tracks.push(BlobTrack {
            track_id: tid,
            track_type: summary.track_type.to_string(),
            codec: summary.codec,
            timescale: track_info.timescale,
            init_cid,
            init_data,
            blob_cid: blob_cid.clone(),
            blob_size,
            segments,
            width: summary.width,
            height: summary.height,
            channels: summary.channels,
            sample_rate: summary.sample_rate,
            language: summary.language,
            label: summary.label,
        });
    }

    eprintln!(
        "blob: {blob_cid} ({blob_size} bytes, {} tracks)",
        tracks.len(),
    );
    Ok(tracks)
}

/// Group per-sample fragments into HLS segments at video keyframe
/// boundaries. Each new sync sample closes the preceding segment.
fn group_fragments_video(fragments: &[FlatFragment]) -> Vec<BlobSegment> {
    let mut segments = Vec::new();
    let mut cur_offset = 0u64;
    let mut cur_size = 0u64;
    let mut cur_dur = 0u64;
    let mut cur_samples = 0u32;

    for frag in fragments {
        if frag.is_sync && cur_size > 0 {
            segments.push(BlobSegment {
                offset: cur_offset,
                size: cur_size,
                duration_ticks: cur_dur,
                sample_count: cur_samples,
            });
            cur_size = 0;
            cur_dur = 0;
            cur_samples = 0;
        }
        if cur_size == 0 {
            cur_offset = frag.offset;
        }
        cur_size += frag.size;
        cur_dur += frag.duration as u64;
        cur_samples += 1;
    }
    if cur_size > 0 {
        segments.push(BlobSegment {
            offset: cur_offset,
            size: cur_size,
            duration_ticks: cur_dur,
            sample_count: cur_samples,
        });
    }
    segments
}

/// Group per-sample fragments into ~2-second HLS segments (for audio and
/// text).
fn group_fragments_audio(fragments: &[FlatFragment], timescale: u32) -> Vec<BlobSegment> {
    let target_ticks = timescale as u64 * 2;
    let mut segments = Vec::new();
    let mut cur_offset = 0u64;
    let mut cur_size = 0u64;
    let mut cur_dur = 0u64;
    let mut cur_samples = 0u32;

    for frag in fragments {
        if cur_size == 0 {
            cur_offset = frag.offset;
        }
        cur_size += frag.size;
        cur_dur += frag.duration as u64;
        cur_samples += 1;

        if cur_dur >= target_ticks {
            segments.push(BlobSegment {
                offset: cur_offset,
                size: cur_size,
                duration_ticks: cur_dur,
                sample_count: cur_samples,
            });
            cur_size = 0;
            cur_dur = 0;
            cur_samples = 0;
        }
    }
    if cur_size > 0 {
        segments.push(BlobSegment {
            offset: cur_offset,
            size: cur_size,
            duration_ticks: cur_dur,
            sample_count: cur_samples,
        });
    }
    segments
}

/// Write the master playlist and per-track media playlists. Filenames
/// are prefixed with `primary_blob_cid` so multiple streams can share an
/// output directory.
fn write_playlists(
    output_dir: &Path,
    primary_blob_cid: &str,
    entries: &[TrackEntry],
) -> Result<()> {
    let mut master = String::new();
    master.push_str("#EXTM3U\n#EXT-X-VERSION:6\n\n");

    // Audio renditions — prefer AAC as DEFAULT for Safari compatibility.
    let default_audio_key = entries
        .iter()
        .find(|e| e.track.track_type == "audio" && e.track.codec.starts_with("mp4a"))
        .or_else(|| entries.iter().find(|e| e.track.track_type == "audio"))
        .map(|e| e.key.clone());

    for entry in entries {
        if entry.track.track_type != "audio" {
            continue;
        }
        let is_default = default_audio_key.as_deref() == Some(&entry.key);
        let default = if is_default { "YES" } else { "NO" };
        master.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{}\",\
             DEFAULT={default},AUTOSELECT=YES,CHANNELS=\"{}\",URI=\"{primary_blob_cid}.audio-{}.m3u8\"\n",
            entry.track.codec,
            entry.track.channels,
            entry.key,
        ));
    }
    // Subtitle renditions (WebVTT in fMP4). Only emitted when a text track
    // exists, so AV-only master playlists are unchanged.
    let has_subtitles = entries.iter().any(|e| e.track.track_type == "text");
    for entry in entries {
        if entry.track.track_type != "text" {
            continue;
        }
        let t = &entry.track;
        let language = t.language.as_deref().unwrap_or("und");
        let name = t.label.as_deref().unwrap_or(language);
        master.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{}\",\
             LANGUAGE=\"{}\",DEFAULT=NO,AUTOSELECT=YES,URI=\"{primary_blob_cid}.text-{}.m3u8\"\n",
            hls_quoted(name),
            hls_quoted(language),
            entry.key,
        ));
    }
    master.push('\n');

    // CODECS string for video variants — pair with the AAC audio track when present.
    let audio_codec = entries
        .iter()
        .find(|e| e.track.track_type == "audio" && e.track.codec.starts_with("mp4a"))
        .or_else(|| entries.iter().find(|e| e.track.track_type == "audio"))
        .map(|e| e.track.codec.as_str())
        .unwrap_or("mp4a.40.2");
    // With subtitles, each variant names the group and lists the text codec.
    let (subs_attr, subs_codec) = if has_subtitles {
        (",SUBTITLES=\"subs\"", ",wvtt")
    } else {
        ("", "")
    };

    for entry in entries {
        if entry.track.track_type != "video" {
            continue;
        }
        let t = &entry.track;
        let total_bytes: u64 = t.segments.iter().map(|s| s.size).sum();
        let total_ticks: u64 = t.segments.iter().map(|s| s.duration_ticks).sum();
        let total_samples: u32 = t.segments.iter().map(|s| s.sample_count).sum();
        let ts = t.timescale as f64;
        let total_dur = total_ticks as f64 / ts;
        let bandwidth = if total_dur > 0.0 {
            (total_bytes as f64 * 8.0 / total_dur) as u64
        } else {
            0
        };
        let frame_rate = if total_dur > 0.0 {
            total_samples as f64 / total_dur
        } else {
            0.0
        };

        master.push_str(&format!(
            "#EXT-X-STREAM-INF:AUDIO=\"audio\"{subs_attr},BANDWIDTH={bandwidth},\
             CODECS=\"{},{audio_codec}{subs_codec}\",RESOLUTION={}x{},FRAME-RATE={frame_rate:.3}\n",
            t.codec, t.width, t.height,
        ));
        master.push_str(&format!("{primary_blob_cid}.video-{}.m3u8\n", entry.key));
    }

    fs::write(
        output_dir.join(format!("{primary_blob_cid}.m3u8")),
        &master,
    )?;

    // Per-track media playlists.
    for entry in entries {
        let t = &entry.track;
        let ts = t.timescale as f64;
        let blob_file = format!("{}.mp4", t.blob_cid);

        let max_dur: f64 = t
            .segments
            .iter()
            .map(|s| s.duration_ticks as f64 / ts)
            .fold(0.0, f64::max);
        let target_dur = (max_dur.ceil() as u64).max(1);

        let mut playlist = String::new();
        playlist.push_str("#EXTM3U\n");
        playlist.push_str("#EXT-X-VERSION:6\n");
        playlist.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        playlist.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        playlist.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur}\n"));
        playlist.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
        playlist.push_str(&format!("#EXT-X-MAP:URI=\"{}.mp4\"\n\n", t.init_cid));

        for seg in &t.segments {
            let dur_sec = seg.duration_ticks as f64 / ts;
            playlist.push_str(&format!("#EXTINF:{dur_sec:.6},\n"));
            playlist.push_str(&format!(
                "#EXT-X-BYTERANGE:{}@{}\n",
                seg.size, seg.offset
            ));
            playlist.push_str(&blob_file);
            playlist.push('\n');
        }

        playlist.push_str("#EXT-X-ENDLIST\n");
        let prefix = match t.track_type.as_str() {
            "video" => "video",
            "text" => "text",
            _ => "audio",
        };
        fs::write(
            output_dir.join(format!("{primary_blob_cid}.{prefix}-{}.m3u8", entry.key)),
            &playlist,
        )?;
    }

    Ok(())
}

fn write_metadata_json(
    output_dir: &Path,
    primary_blob_cid: &str,
    primary_blob_size: u64,
    entries: &[TrackEntry],
) -> Result<()> {
    let mut meta_tracks = serde_json::Map::new();
    for entry in entries {
        let t = &entry.track;
        let segments: Vec<serde_json::Value> = t
            .segments
            .iter()
            .map(|s| {
                serde_json::json!({
                    "offset": s.offset,
                    "size": s.size,
                    "durationTicks": s.duration_ticks,
                    "sampleCount": s.sample_count,
                })
            })
            .collect();

        let mut info = serde_json::json!({
            "type": t.track_type,
            "codec": t.codec,
            "timescale": t.timescale,
            "initCid": t.init_cid,
            "blobCid": t.blob_cid,
            "blobSize": t.blob_size,
            "segments": segments,
        });
        if t.track_type == "video" {
            info["width"] = serde_json::json!(t.width);
            info["height"] = serde_json::json!(t.height);
        } else if t.track_type == "text" {
            info["language"] = serde_json::json!(t.language);
            if let Some(label) = &t.label {
                info["label"] = serde_json::json!(label);
            }
        } else {
            info["channels"] = serde_json::json!(t.channels);
            info["sampleRate"] = serde_json::json!(t.sample_rate);
        }
        meta_tracks.insert(entry.key.clone(), info);
    }

    let metadata = serde_json::json!({
        "blobCid": primary_blob_cid,
        "blobSize": primary_blob_size,
        "tracks": meta_tracks,
    });
    let metadata_str = serde_json::to_string_pretty(&metadata).unwrap_or_default();
    fs::write(
        output_dir.join(format!("{primary_blob_cid}.json")),
        &metadata_str,
    )?;
    Ok(())
}

/// Make `s` safe inside an HLS quoted-string attribute, which may not contain
/// `"`, CR, or LF (RFC 8216 § 4.2).
fn hls_quoted(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '"' => '\'',
            '\r' | '\n' => ' ',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(track_id: u32, track_type: &str, codec: &str) -> BlobTrack {
        BlobTrack {
            track_id,
            track_type: track_type.into(),
            codec: codec.into(),
            timescale: 1000,
            init_cid: format!("init{track_id}"),
            init_data: Vec::new(),
            blob_cid: "blob".into(),
            blob_size: 100,
            segments: vec![BlobSegment {
                offset: 0,
                size: 10,
                duration_ticks: 2000,
                sample_count: 2,
            }],
            width: if track_type == "video" { 640 } else { 0 },
            height: if track_type == "video" { 360 } else { 0 },
            channels: if track_type == "audio" { 2 } else { 0 },
            sample_rate: if track_type == "audio" { 48000 } else { 0 },
            language: None,
            label: None,
        }
    }

    fn entries(with_text: bool) -> Vec<TrackEntry> {
        let mut tracks = vec![track(1, "video", "avc1.64001f"), track(2, "audio", "opus")];
        if with_text {
            let mut t = track(3, "text", "wvtt");
            t.language = Some("en-US".into());
            t.label = Some("Live \"captions\"".into());
            tracks.push(t);
        }
        tracks
            .into_iter()
            .map(|track| TrackEntry {
                key: track.track_id.to_string(),
                track,
            })
            .collect()
    }

    fn master(with_text: bool) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        write_playlists(dir.path(), "blob", &entries(with_text)).unwrap();
        let master = fs::read_to_string(dir.path().join("blob.m3u8")).unwrap();
        (dir, master)
    }

    #[test]
    fn av_master_has_no_subtitles() {
        let (_dir, m) = master(false);
        assert!(!m.contains("SUBTITLES"), "got: {m}");
        assert!(m.contains("CODECS=\"avc1.64001f,opus\""), "got: {m}");
    }

    #[test]
    fn text_track_becomes_subtitles_rendition() {
        let (dir, m) = master(true);
        assert!(
            m.contains(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Live 'captions'\",\
                 LANGUAGE=\"en-US\",DEFAULT=NO,AUTOSELECT=YES,URI=\"blob.text-3.m3u8\""
            ),
            "got: {m}"
        );
        assert!(
            m.contains("#EXT-X-STREAM-INF:AUDIO=\"audio\",SUBTITLES=\"subs\",BANDWIDTH="),
            "got: {m}"
        );
        assert!(m.contains("CODECS=\"avc1.64001f,opus,wvtt\""), "got: {m}");

        let media = fs::read_to_string(dir.path().join("blob.text-3.m3u8")).unwrap();
        assert!(media.contains("#EXT-X-MAP:URI=\"init3.mp4\""), "got: {media}");
        assert!(media.contains("#EXT-X-BYTERANGE:10@0"), "got: {media}");
    }

    #[test]
    fn metadata_json_describes_text_track() {
        let dir = tempfile::tempdir().unwrap();
        write_metadata_json(dir.path(), "blob", 100, &entries(true)).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("blob.json")).unwrap())
                .unwrap();
        let text = &json["tracks"]["3"];
        assert_eq!(text["type"], "text");
        assert_eq!(text["codec"], "wvtt");
        assert_eq!(text["language"], "en-US");
        assert_eq!(text["label"], "Live \"captions\"");
        assert!(text.get("channels").is_none());
        // AV entries keep their shape.
        assert_eq!(json["tracks"]["2"]["channels"], 2);
        assert!(json["tracks"]["2"].get("language").is_none());
    }

    #[test]
    fn blob_track_json_omits_text_fields_for_av() {
        let v = serde_json::to_value(track(1, "video", "avc1.64001f")).unwrap();
        assert!(v.get("language").is_none());
        assert!(v.get("label").is_none());
    }

    #[test]
    fn track_summary_covers_text() {
        let mut c = Catalog::default();
        c.insert_text(
            "text3",
            crate::catalog::TextConfig {
                codec: "wvtt".into(),
                container: crate::catalog::Container::cmaf(1000, 3),
                language: "es".into(),
                label: None,
                config: "WEBVTT".into(),
            },
        );
        let s = track_summary(&c, 3);
        assert_eq!(s.track_type, "text");
        assert_eq!(s.codec, "wvtt");
        assert_eq!(s.language.as_deref(), Some("es"));
        assert_eq!(s.label, None);
        assert_eq!(track_summary(&c, 9).track_type, "unknown");
    }
}
