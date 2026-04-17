# MUXL Canonical Form Specification

This document defines the canonical byte layout for MUXL segments and derived formats.

All choices are provisional and subject to revision after playback testing.

## MUXL Segment

A MUXL segment contains one track's data for one GoP. Each frame gets its own moof+mdat pair. Segments are per-track — a GoP with N tracks produces N segments.

```
Segment (track 1, GoP 1):
  moof(frame 1) + mdat(frame 1)
  moof(frame 2) + mdat(frame 2)
  ...

Segment (track 2, GoP 1):
  moof(frame 1) + mdat(frame 1)
  moof(frame 2) + mdat(frame 2)
  ...
```

Per-track segments enable byte-range addressing (HLS playlists can index a single MUXL fMP4 file) and independent per-track content hashing. Segments for the same track are blindly concatenatable by byte appending.

Track initialization metadata (codec config, timescales) is out-of-band — either in the MUXL fMP4 file's init segment or from an external source.

### Segmentation Rule

Segment boundaries are driven by video sync samples (keyframes). Audio samples are grouped with the video GoP they temporally overlap. Given the same samples with the same timestamps, the segment boundaries are always identical.

### moof

Each moof covers exactly one sample (frame) from one track.

- **mfhd**: sequence_number, 1-based, incrementing globally across the stream
- **traf**: exactly one per moof
  - **tfhd**: track_id; flags = `default_base_is_moof`; no default sample values (all explicit in trun)
  - **tfdt**: base_media_decode_time in the track's media timescale
  - **trun**: exactly one entry; flags = `data_offset | sample_duration | sample_size | sample_flags`; add `sample_cts` flag if the sample has a non-zero composition time offset

### trun Sample Flags

- Sync sample: `0x02000000` (sample_depends_on = 2: depends on no other sample)
- Non-sync sample: `0x01010000` (sample_depends_on = 1: depends on others; sample_is_non_sync = 1)

### mdat

One mdat per moof, containing exactly one sample's data.

## MUXL fMP4

Init segment followed by per-track segments grouped by track.

```
ftyp
moov (init — track config, empty sample tables)
[track 1 segments: GoP 1, GoP 2, ...]
[track 2 segments: GoP 1, GoP 2, ...]
...
```

Valid fMP4 file. Each track's segments form a contiguous byte range, enabling HLS byte-range playlists to address individual tracks within a single file. Tracks are ordered by track_id ascending.

## MUXL Flat MP4

A hybrid layout that reads as a flat MP4 at the top level *and* contains inline [Hang CMAF](https://doc.moq.dev/concept/layer/hang) fragments addressable by byte range. One file serves both downloads (LosslessCut, desktop players, editors) and HLS byte-range playlists. This design is adapted from the [OBS Studio hybrid MP4](https://obsproject.com/blog/obs-studio-hybrid-mp4) approach.

```
ftyp
moov (populated sample tables; no mvex; faststart)
mdat (64-bit largesize envelope; payload =)
  [track 1: moof+mdat, moof+mdat, ...]   ← canonical single-sample MUXL fragments
  [track 2: moof+mdat, moof+mdat, ...]
  ...
```

Top-level view: ftyp + moov + mdat. A flat-MP4 parser uses the populated `stbl` tables; `co64` entries point at sample bytes *inside* the inner mdats, skipping past the inner moof headers.

CMAF byte-range view: the inner `moof+mdat` pairs are canonical MUXL fragments (self-contained, `default-base-is-moof`). An HLS player fetching a byte range sees only the fragment and never parses the outer container.

### Relationship to the MUXL fMP4

The inner `moof+mdat` sequence is byte-identical to a MUXL fMP4's body. MUXL fMP4 ↔ flat MP4 conversion is a wrapper swap:
- MUXL fMP4 → flat: replace `moov_init` with populated `moov`, prepend 16-byte outer mdat header, copy body verbatim.
- Flat → MUXL fMP4: drop outer envelope header, replace populated `moov` with init `moov`, leave inner fragments untouched.

No sample bytes are ever touched. Per-sample metadata (durations, sizes, sync flags, cts offsets) is already present in the MUXL fMP4's `trun` entries.

### moov

Same `mvhd`/`trak`/`tkhd`/`mdhd`/`hdlr`/`minf` rules as the init segment, with:
- Populated `stbl` sample tables (see below).
- **No** `mvex`. The top-level view is non-fragmented; HLS consumers use an out-of-band init segment.
- Duration fields (`mvhd.duration`, `tkhd.duration`, `mdhd.duration`) filled in from the samples.

### stbl (populated)

- **stsd**: same as init segment
- **stts**: RLE per-sample decode durations (media timescale)
- **ctts**: version 1 (signed), RLE, present only if any sample has a non-zero composition time offset
- **stsz**: uniform if all samples have equal size; per-sample list otherwise
- **stsc**: exactly one entry — `first_chunk=1, samples_per_chunk=1, sample_description_index=1`. Each sample is its own chunk, because each is preceded by its own inner moof+mdat header bytes.
- **co64**: one entry per sample. Entry `i` = `inner_moof_start + inner_moof_size + 8` (absolute file offset of sample i's bytes inside its inner mdat). Always 64-bit, never `stco`.
- **stss**: 1-based sync sample indices (video only; omitted for audio and all-sync tracks)

No other `stbl` child boxes (no `stsh`/`stps`/`stdp`/`padb`/`sdtp`).

### Outer mdat

Always 64-bit extended size header (16 bytes: `size=1` + "mdat" + 8-byte `largesize`). Payload is `[moof+mdat]*` grouped by `track_id` ascending, samples within a track in decode order.

### Inner moof+mdat fragments

Canonical MUXL fragments per § MUXL Segment. `mfhd.sequence_number` increments globally across all tracks, starting at 1.

### Layout arithmetic

Given `ftyp` size `F`, `moov` size `M`, and per-sample inner fragment sizes `f_i = moof_size_i + 8 + sample_size_i`:

- Outer mdat payload starts at `P = F + M + 16`
- Sample `i`'s `co64` entry = `P + sum(f_j for j < i) + moof_size_i + 8`
- Outer `mdat.largesize` = `16 + sum(f_i)`

## ftyp

- **major_brand**: `muxl`
- **minor_version**: `0`
- **compatible_brands**: `[muxl, isom, iso2]`

`muxl` signals conformance. `isom`/`iso2` keep the file playable by generic ISOBMFF tools. Codec-agnostic; players use stsd for codec detection.

## Init Segment moov

The moov in the init segment describes track configuration with empty sample tables, zero durations, and no sample entries.

Required child boxes: `mvhd`, `trak` (one per track), `mvex` (with `trex` per track).

### mvhd

- **version**: 0
- **flags**: 0
- **creation_time**: 0
- **modification_time**: 0
- **timescale**: 1000
- **duration**: 0
- **rate**: 1.0
- **volume**: 1.0
- **matrix**: identity
- **next_track_id**: max(track_ids) + 1

### mvex

Required for fMP4 playback — signals that moof+mdat pairs follow the moov.

- **trex** (one per track):
  - **track_id**: matching the trak
  - **default_sample_description_index**: 1
  - **default_sample_duration**: 0
  - **default_sample_size**: 0
  - **default_sample_flags**: 0

All sample metadata is explicit in each trun entry, so trex defaults are all zero.

### trak ordering

Sorted by track_id ascending. No udta, meta, or iods.

### tkhd

- **version**: 0
- **flags**: 3 (track_enabled | track_in_movie)
- **creation_time**: 0
- **modification_time**: 0
- **duration**: 0
- **matrix, width/height, layer, alternate_group, volume**: from track config

### mdhd

- **version**: 0
- **flags**: 0
- **creation_time**: 0
- **modification_time**: 0
- **timescale**: preserved from source track (passthrough)
- **duration**: 0
- **language**: `"und"`

### hdlr

- **version**: 0
- **flags**: 0
- **handler_type**: `"vide"` for video, `"soun"` for audio
- **name**: empty string (name is cosmetic and varies across muxers)

### minf

- **vmhd**: present for video tracks (default values)
- **smhd**: present for audio tracks (default values)
- **dinf**: required, contains dref
  - **dref**: one self-contained `url` entry with empty location string (signals data is in the same file)

### stbl (Sample Table)

stsd populated with codec config, all other tables empty.

### edts / elst

Optional. When present in the source track, preserved byte-for-byte on output. `segment_duration` is in the movie timescale (1000); `media_time` is in the track's media timescale, with `-1` denoting an empty edit.

Edit lists are track-level presentation metadata, not per-fragment metadata — they live in the init segment (or in the MUXL flat MP4 moov), not in individual MUXL fragments or segments. A fragments-only round-trip drops edit lists; fragments + catalog preserves them, because the catalog carries `VideoTrackConfig::edits` / `AudioTrackConfig::edits`.

## Stripped Boxes

The following are stripped entirely:

- **udta**: tool tags are non-deterministic
- **meta**: at moov and trak level
- **free / skip**: padding boxes
- **iods**: not needed
