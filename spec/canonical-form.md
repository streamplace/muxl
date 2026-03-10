# MUXL Canonical Form Specification

This document defines the canonical MP4 box structure produced by MUXL. Each section specifies the canonical choice for a box type, with rationale drawn from observed muxer discrepancies.

All choices are provisional and subject to revision after playback testing.

## MUXL Segment Structure

A MUXL segment contains one GoP of content. Each track has its own moof+mdat pair. Track initialization metadata (codec config, timescales) is out-of-band — it comes from the archive file's init segment or an external source.

```
moof(track 1) + mdat(track 1)           ← one track's samples for this GoP
moof(track 2) + mdat(track 2)           ← another track's samples
...
```

Within each moof:
- `mfhd`: sequence_number (1-based, global across the stream)
- `traf`: exactly one per moof (one track per moof+mdat pair)
  - `tfhd`: track_id, flags include `default_base_is_moof`; `sample_description_index` set if not 1
  - `tfdt`: base_media_decode_time for this track in this segment
  - `trun`: per-sample duration, size, flags, composition time offset; `data_offset` points to mdat payload

Track ordering within a segment: tracks are ordered by track_id (ascending).

Segments are blindly concatenatable by byte appending.

## Canonical fMP4 (Archive Format)

The archive file prepends an init segment to concatenated MUXL segments:

```
ftyp                                     ← file type
moov (empty sample tables)               ← init segment: track config only
[MUXL segment 1]                         ← uuid + per-track moof+mdat
[MUXL segment 2]
...
```

This is a valid fMP4 file. Players skip uuid boxes and process moof+mdat pairs.

## Flat MP4 (Export Format)

For maximum compatibility. Layout: `ftyp`, `mdat`, `moov`.

Generated from canonical fMP4 by consolidating all moof/trun data into moov sample tables and concatenating all mdat payloads.

## ftyp (File Type Box)

- **major_brand**: `isom`
- **minor_version**: `0`
- **compatible_brands**: `[isom, iso2]`

Rationale: `isom` is the most universal major brand. We use only codec-agnostic brands — codec-specific brands like `avc1` or `av01` are omitted since players determine codec support from `stsd` entries, not ftyp. This keeps ftyp static regardless of whether the file contains H.264, AV1, AAC, or Opus.

## moov (Movie Box)

In canonical fMP4, the moov appears in the init segment with empty sample tables (no samples, just track configuration). In flat MP4, the moov contains complete sample tables.

### Box Ordering Within moov

`mvhd`, then `trak` boxes sorted by track_id, then nothing else. No `udta`, `meta`, or `iods`.

### mvhd (Movie Header Box)

- **version**: 0 (unless duration overflows u32)
- **flags**: 0
- **creation_time**: 0
- **modification_time**: 0
- **timescale**: 1000
- **duration**: max of track durations, in movie timescale (0 in init segment)
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
- **duration**: derived from mdhd duration, scaled to movie timescale (0 in init segment)
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
- **duration**: recomputed after timescale normalization (0 in init segment)
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

In the init segment (canonical fMP4), sample tables are empty — all sample data is in the moof/trun boxes.

In flat MP4 export:
- **stsd**: preserved from input (codec configuration is content)
- **stts**: sample deltas rescaled to canonical media timescale (structure preserved)
- **stss**: preserved from input (keyframe table is content)
- **ctts**: sample offsets rescaled to canonical media timescale (structure preserved)
- **stsz**: preserved from input (sample sizes are content)
- **stsc**: canonical — one sample per chunk, with entries tracking sample_description_index changes
- **stco/co64**: recomputed from canonical mdat layout. Use stco (32-bit) when all offsets fit in u32, otherwise co64.

Unknown boxes (sgpd, sbgp, etc.) are currently dropped during round-trip through mp4-rust.

## moof (Movie Fragment Box)

Each moof covers exactly one track for one GoP segment.

- **mfhd**: sequence_number, 1-based, incrementing globally across the stream
- **traf**: exactly one per moof
  - **tfhd**: track_id, flags = `default_base_is_moof`; `sample_description_index` set if != 1
  - **tfdt**: base_media_decode_time in canonical media timescale; version 0 if fits in u32, else version 1
  - **trun**: flags include `data_offset`, `sample_duration`, `sample_size`, `sample_flags`; `sample_cts` included if any sample has a non-zero composition time offset

### trun Sample Flags

- Sync sample: `0x02000000` (sample_depends_on = 2: depends on no other sample)
- Non-sync sample: `0x01010000` (sample_depends_on = 1: depends on others; sample_is_non_sync = 1)

## mdat (Media Data Box)

### In MUXL Segments

Each moof+mdat pair contains one track's samples for one GoP. Samples are written sequentially in decode order within the mdat.

### In Flat MP4

Samples are written sequentially per track, in track_id order. All samples for track 1, then all samples for track 2, etc. Each sample is its own chunk.

Rationale: This is the simplest deterministic layout. Not optimal for streaming (interleaved would be better), but trivially reproducible.

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
