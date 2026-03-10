# MUXL Canonical Form Specification

This document defines the canonical byte layout for MUXL segments and derived formats.

All choices are provisional and subject to revision after playback testing.

## MUXL Segment

A MUXL segment contains one GoP of content. It is constructed from per-frame fragments (Hang CMAF style) grouped by GoP and assembled in a deterministic order.

```
moof(track 1) + mdat(track 1)
moof(track 2) + mdat(track 2)
...
```

Each track gets its own moof+mdat pair. Tracks are ordered by track_id (ascending). Segments are blindly concatenatable by byte appending.

Track initialization metadata (codec config, timescales) is out-of-band — either in the archive file's init segment or from an external source.

### Segmentation Rule

Each segment begins at a video sync sample (keyframe). Audio samples are grouped with the video GoP they temporally overlap. Given the same samples with the same timestamps, the segment boundaries are always identical.

### moof

Each moof covers exactly one track for one GoP.

- **mfhd**: sequence_number, 1-based, incrementing globally across the stream
- **traf**: exactly one per moof
  - **tfhd**: track_id; flags = `default_base_is_moof`; `sample_description_index` set if != 1
  - **tfdt**: base_media_decode_time in canonical media timescale; version 0 if fits in u32, else version 1
  - **trun**: flags = `data_offset | sample_duration | sample_size | sample_flags`; add `sample_cts` flag if any sample has a non-zero composition time offset

### trun Sample Flags

- Sync sample: `0x02000000` (sample_depends_on = 2: depends on no other sample)
- Non-sync sample: `0x01010000` (sample_depends_on = 1: depends on others; sample_is_non_sync = 1)

### mdat

Samples written sequentially in decode order within the mdat. One mdat per track per segment.

## MUXL Archive fMP4

Init segment followed by concatenated MUXL segments.

```
ftyp
moov (init — track config, empty sample tables)
[MUXL segment 1]
[MUXL segment 2]
...
```

Valid fMP4 file. Players process moof+mdat pairs after the init.

## Flat MP4

Export format for maximum compatibility. Layout: `ftyp`, `mdat`, `moov`.

Generated from MUXL segments + init data by consolidating all trun tables into moov sample tables and concatenating all mdat payloads. Mdat layout: all samples for track 1, then all samples for track 2, etc. (sequential per track, track_id order). Each sample is its own chunk.

## ftyp

- **major_brand**: `isom`
- **minor_version**: `0`
- **compatible_brands**: `[isom, iso2]`

Codec-agnostic. Players use stsd entries for codec detection.

## Init Segment moov

The moov in the init segment describes track configuration with empty sample tables. It uses the same canonical field values as the flat MP4 moov, but with zero durations and no sample entries.

### mvhd

- **version**: 0
- **flags**: 0
- **creation_time**: 0
- **modification_time**: 0
- **timescale**: 1000
- **duration**: 0 (init segment) or max of track durations (flat MP4)
- **rate**: 1.0
- **volume**: 1.0
- **matrix**: identity
- **next_track_id**: max(track_ids) + 1

### trak ordering

Sorted by track_id ascending. No udta, meta, or iods.

### tkhd

- **version**: 0
- **flags**: 3 (track_enabled | track_in_movie)
- **creation_time**: 0
- **modification_time**: 0
- **duration**: 0 (init segment) or derived from mdhd (flat MP4)
- **matrix, width/height, layer, alternate_group, volume**: from track config

### mdhd

- **version**: 0
- **flags**: 0
- **creation_time**: 0
- **modification_time**: 0
- **timescale**: canonical value per track type
- **duration**: 0 (init segment) or recomputed (flat MP4)
- **language**: from track config

Canonical media timescales:

- **Video**: 60000
- **Audio**: 48000

### hdlr

- **version**: 0
- **flags**: 0
- **handler_type**: from track config
- **name**: `"VideoHandler"` / `"SoundHandler"` / `"SubtitleHandler"` / empty

### stbl (Sample Table)

In init segment: stsd populated with codec config, all other tables empty.

In flat MP4:

- **stsd**: codec configuration from track config
- **stts**: from trun sample durations (rescaled to canonical timescale)
- **stss**: derived from trun sample flags (sync samples)
- **ctts**: from trun composition time offsets (rescaled to canonical timescale)
- **stsz**: from trun sample sizes
- **stsc**: one sample per chunk, entries track sample_description_index changes
- **stco/co64**: recomputed from mdat layout; stco if all offsets fit in u32

### edts / elst

From track config. segment_duration in movie timescale (1000). media_time in canonical media timescale.

## Stripped Boxes

The following are stripped entirely:

- **udta**: tool tags are non-deterministic
- **meta**: at moov and trak level
- **free / skip**: padding boxes
- **iods**: not needed
