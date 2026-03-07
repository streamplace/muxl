# MUXL Canonical Form Specification

This document defines the canonical MP4 box structure produced by MUXL. Each section specifies the canonical choice for a box type, with rationale drawn from observed muxer discrepancies.

All choices are provisional and subject to revision after playback testing.

## Top-Level Box Ordering

Canonical order: `ftyp`, `mdat`, `moov`. No `free`, `skip`, or `udta` boxes at top level.

Rationale: mdat-before-moov is the simplest layout (avoids offset chicken-and-egg problems). We may switch to moov-first (faststart) later for streaming, but mdat-first is deterministic without two-pass writing.

## ftyp (File Type Box)

- **major_brand**: `isom`
- **minor_version**: `0`
- **compatible_brands**: `[isom, iso2]`

Rationale: `isom` is the most universal major brand. We use only codec-agnostic brands — codec-specific brands like `avc1` or `av01` are omitted since players determine codec support from `stsd` entries, not ftyp. This keeps ftyp static regardless of whether the file contains H.264, AV1, AAC, or Opus.

## moov (Movie Box)

### Box Ordering Within moov

`mvhd`, then `trak` boxes sorted by track_id, then nothing else. No `udta`, `meta`, or `iods`.

### mvhd (Movie Header Box)

- **version**: 0 (unless duration overflows u32)
- **flags**: 0
- **creation_time**: 0
- **modification_time**: 0
- **timescale**: 1000
- **duration**: max of track durations, in movie timescale
- **rate**: 1.0 (0x00010000)
- **volume**: 1.0 (0x0100)
- **matrix**: identity
- **next_track_id**: max(track_ids) + 1

Rationale: Timestamps are non-deterministic metadata (they embed wall-clock time). Zero them. Timescale 1000 (millisecond precision) matches ffmpeg default and is sufficient for movie-level duration.

### trak (Track Box)

Tracks are ordered by track_id (ascending). No trak-level `meta` or `udta`.

#### tkhd (Track Header Box)

- **version**: 0 (unless duration overflows u32)
- **flags**: 3 (track_enabled | track_in_movie)
- **creation_time**: 0
- **modification_time**: 0
- **duration**: derived from mdhd duration, scaled to movie timescale
- **matrix**: preserved from input
- **width/height**: preserved from input
- **layer, alternate_group, volume**: preserved from input

Rationale: flags=3 is the ffmpeg default. gstreamer uses flags=7 (adds track_in_preview) — we pick the minimal set.

#### edts (Edit Box)

Preserved from input with rescaling: `segment_duration` values are rescaled from the original movie timescale to the canonical movie timescale (1000). `media_time` values are rescaled from the original media timescale to the canonical media timescale. Empty edits (media_time = -1) are not rescaled.

Edit lists are content-meaningful (audio priming, A/V sync).

#### mdia (Media Box)

##### mdhd (Media Header Box)

- **version**: 0
- **flags**: 0
- **creation_time**: 0
- **modification_time**: 0
- **timescale**: normalized to canonical value per track type (see below)
- **duration**: recomputed after timescale normalization
- **language**: preserved from input

Canonical media timescales:
- **Video**: 60000 (ffmpeg default, works for 24/25/30/60fps and VFR content)
- **Audio**: 48000 (standard for 48kHz AAC/Opus; matches sample rate)

Timescale normalization is lossless: all stts deltas, ctts offsets, and elst media_time values must scale to exact integers. If any value would require rounding, canonicalization fails with an error.

Rationale: Media timescale varies by muxer (ffmpeg: 60000, gstreamer: 6000 for the same ~30fps video). By normalizing to a canonical timescale, identical content from different muxers produces identical stts/ctts tables.

##### hdlr (Handler Box)

- **version**: 0
- **flags**: 0
- **handler_type**: preserved from input
- **name**: canonical strings: `"VideoHandler"` for vide, `"SoundHandler"` for soun, `"SubtitleHandler"` for sbtl/text, empty for others

Rationale: Handler name strings vary wildly across muxers and are purely informational.

##### minf (Media Information Box)

###### vmhd / smhd (Video/Sound Media Header)

Preserved from input.

###### dinf (Data Information Box)

Preserved from input (always a self-referencing dref).

###### stbl (Sample Table Box)

- **stsd**: preserved from input (codec configuration is content)
- **stts**: sample deltas rescaled to canonical media timescale (structure preserved)
- **stss**: preserved from input (keyframe table is content)
- **ctts**: sample offsets rescaled to canonical media timescale (structure preserved)
- **stsz**: preserved from input (sample sizes are content)
- **stsc**: canonical — one sample per chunk: `[(first_chunk=1, samples_per_chunk=1, sample_description_index=1)]`
- **stco/co64**: recomputed from canonical mdat layout. Use stco (32-bit) when all offsets fit in u32, otherwise co64.

Unknown boxes (sgpd, sbgp, etc.) are currently dropped during round-trip through mp4-rust.

## mdat (Media Data Box)

### Flat MP4

Samples are written sequentially per track, in track_id order. All samples for track 1, then all samples for track 2, etc. Each sample is its own chunk.

Rationale: This is the simplest deterministic layout. Not optimal for streaming (interleaved would be better), but trivially reproducible.

### Fragmented MP4 (fMP4)

In the canonical fMP4 form, each segment is a `moof+mdat` pair. The segmentation rule is deterministic: each segment begins at a sync sample (keyframe), as identified by stss in a flat MP4 or sample_flags in fMP4 trun boxes. Segments contain all tracks' samples for that time range.

Segment structure:
- `moof`: contains `mfhd` (sequence_number, 1-based) and one `traf` per track (sorted by track_id)
- Each `traf` contains: `tfhd` (track_id, default values), `tfdt` (base decode time), `trun` (sample table for this segment)
- `mdat`: sample data for all tracks in this segment, in track_id order within the segment

The canonical fMP4 file layout is: `ftyp`, then repeating `moof+mdat` pairs. No top-level `moov` (the moof boxes carry all metadata). No `free`, `skip`, or `udta` boxes.

Round-trip property: a canonical flat MP4 can be deterministically segmented into canonical fMP4 (using the keyframe-based segmentation rule), and a canonical fMP4 can be deterministically flattened into canonical flat MP4. The sample bytes in mdat are identical in both representations.

## Multiple Sample Descriptions (stsd)

When codec parameters change mid-stream (e.g., resolution/orientation change from a mobile WebRTC/WHIP source causing new H.264 SPS/PPS), the stsd box contains multiple sample entries. The stsc table maps chunks to sample description indices.

In canonical form:
- `stsd` entries are preserved from input in order
- `stsc` entries track sample description index changes: a new stsc entry is emitted whenever the sample_description_index changes
- In fMP4, `tfhd.sample_description_index` indicates which stsd entry applies to a given fragment

This means our canonical stsc is NOT always a single `(1, 1, 1)` entry — it's one entry per sample-description-index run, with samples_per_chunk=1.

## udta (User Data Box)

Stripped entirely. Tool tags (e.g., "Lavf58.76.100") are non-deterministic.

## free / skip (Free Space Boxes)

Stripped entirely.
