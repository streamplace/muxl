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

use std::collections::HashSet;
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

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
    pub track_type: String, // "video" or "audio"
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

        let (track_type, codec, width, height, channels, sample_rate): (
            &str,
            String,
            u32,
            u32,
            u32,
            u32,
        ) = if let Some(v) = catalog.video_configs().find(|v| v.track_id() == tid) {
            ("video", v.codec.clone(), v.coded_width, v.coded_height, 0, 0)
        } else if let Some(a) = catalog.audio_configs().find(|a| a.track_id() == tid) {
            (
                "audio",
                a.codec.clone(),
                0,
                0,
                a.number_of_channels,
                a.sample_rate,
            )
        } else {
            ("unknown", String::new(), 0, 0, 0, 0)
        };

        tracks.push(BlobTrack {
            track_id: tid,
            track_type: track_type.to_string(),
            codec,
            timescale: track_info.timescale,
            init_cid,
            init_data,
            blob_cid: blob_cid.clone(),
            blob_size,
            segments,
            width,
            height,
            channels,
            sample_rate,
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

/// Group per-sample fragments into ~2-second HLS segments (for audio).
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
    master.push('\n');

    // CODECS string for video variants — pair with the AAC audio track when present.
    let audio_codec = entries
        .iter()
        .find(|e| e.track.track_type == "audio" && e.track.codec.starts_with("mp4a"))
        .or_else(|| entries.iter().find(|e| e.track.track_type == "audio"))
        .map(|e| e.track.codec.as_str())
        .unwrap_or("mp4a.40.2");

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
            "#EXT-X-STREAM-INF:AUDIO=\"audio\",BANDWIDTH={bandwidth},\
             CODECS=\"{},{audio_codec}\",RESOLUTION={}x{},FRAME-RATE={frame_rate:.3}\n",
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
        let prefix = if t.track_type == "video" {
            "video"
        } else {
            "audio"
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
