# MUXL Canonical Form Specification

This document defines the canonical byte layout for MUXL. The formal specification is at [dasl.ing/muxl](https://dasl.ing/muxl); this file mirrors the byte-level rules for implementers working in the muxl repository.

All choices are provisional and subject to revision after playback testing.

## Layered Model

MUXL is a three-layer stack:

- **MUXL fragment** — one encoded sample (video frame or audio packet) in a minimal `moof+mdat` pair. The smallest unit. Bit-identical regardless of how it's transported or stored.
- **MUXL canonical segment** — a `uuid` box carrying the per-track catalog as a DRISL payload, followed by one track's fragments for one GoP. The unit of content addressing.
- **Synthesized storage format** — fMP4 (appendable) or flat MP4 (finalized faststart) wrapping N canonical segments together with a derived ISOBMFF header. The header is synthesized from the segments' embedded catalogs. Canonical segments are recoverable byte-for-byte from any storage format.

Signing and provenance — c2pa manifests, S2PA assertions, signed claim chains — are layered on top of MUXL by a separate signing format (see `muxl-sign`). MUXL defines what bytes are canonical; the signing layer defines how those bytes are attested. No c2pa structure appears in MUXL's canonical form.

## MUXL Fragment

One sample, one `moof+mdat` pair.

### moof

Each moof covers exactly one sample from one track.

- **mfhd**: `sequence_number`, 1-based, incrementing per fragment within a segment (restarts at 1 each segment).
- **traf**: exactly one per moof.
  - **tfhd**: `track_id`; flags = `default_base_is_moof`; no default sample values (all explicit in trun).
  - **tfdt**: `base_media_decode_time` in the track's media timescale, carrying the absolute media time of this sample in the track's stream timeline.
  - **trun**: exactly one entry; flags = `data_offset | sample_duration | sample_size | sample_flags`; add `sample_cts` flag if the sample has a non-zero composition time offset.

### trun Sample Flags

- Sync sample: `0x02000000` (`sample_depends_on = 2`: depends on no other sample).
- Non-sync sample: `0x01010000` (`sample_depends_on = 1`: depends on others; `sample_is_non_sync = 1`).

### mdat

One mdat per moof, containing exactly one sample's data.

## MUXL Canonical Segment

A canonical segment is the unit of content addressing. Signing is layered on top — see § Signing & Provenance.

### Structure

```
uuid (muxl catalog box; uuid = 6d75786c-0001-0000-0000-000000000001)
moof+mdat (sample 1)
moof+mdat (sample 2)
...
moof+mdat (sample K)
```

Each canonical segment carries fragments for exactly one track and one GoP. A multi-track GoP produces multiple canonical segments — one per track.

The leading uuid is *always* present — never omitted — so segment boundaries are unambiguous at the byte level. The 16-byte UUID identifier `6d75786c-0001-0000-0000-000000000001` (leading bytes spell `muxl` in ASCII) is provisional pending DASL registration.

### uuid Body

The `uuid` box body is a single DRISL-encoded MUXL catalog ([[drisl]]) describing exactly one track — one entry in `video.renditions` *or* one entry in `audio.renditions`, never both. The catalog is the entire body of the box; no JSON-LD wrapper, no c2pa manifest, no signature claim.

DRISL canonical CBOR encoding makes the uuid body byte-deterministic: any two MUXL implementations producing a canonical segment for the same track configuration produce byte-identical uuid box bytes.

### Tamper Resistance

Modifying the catalog or any fragment changes the canonical segment's bytes, which changes its CID. Detection at the muxl layer is by content-address comparison alone. Cryptographic provenance (proving *who* generated the bytes, not just *that they are these specific bytes*) is added by the signing layer.

### Segmentation Rule

Segment boundaries are driven by video sync samples (keyframes). A new segment begins at each video keyframe. Audio samples are grouped with the video GoP they temporally overlap.

For audio-only streams (no video reference), segments are 1-second wall-clock spans.

Given the same samples with the same timestamps, segment boundaries are always identical.

### Per-Segment Properties

- **`mfhd.sequence_number`**: per-segment, restarting at 1 for each segment's first fragment. Within a segment, sequence numbers increment monotonically. Storage-format synthesizers may rewrite to globally monotonic if a downstream player demands it; the canonical segment bytes remain per-segment-anchored.
- **`tfdt.base_media_decode_time`**: absolute media time of the segment's first sample in the track's stream timeline. Preserved verbatim across storage-format round-trips.

### Round-Trip Property

A canonical segment's bytes are recoverable byte-for-byte from any storage format by stripping the synthesized header and splitting on `uuid` boundaries. This is what lets signatures applied by an upper signing layer survive storage-format conversion.

## Synthesized Storage Formats

Two storage formats wrap N canonical segments with a derived ISOBMFF header. The header is synthesized from the embedded catalogs; the segments' bytes are concatenated verbatim into the body.

### Interleaving Order

Canonical segments are written in time-slice order — for each GoP, all tracks' segments are concatenated contiguously before moving to the next GoP:

```
GoP 1: [track 1 segment][track 2 segment]...
GoP 2: [track 1 segment][track 2 segment]...
...
```

Within a GoP, tracks are ordered by `track_id` ascending. This matches HLS byte-range CMAF expectations (one byte range per time slice covers all tracks).

### Catalog Stability

All canonical segments in a single storage-format file must share a compatible catalog (same track set, same codec configurations). A catalog change mid-stream (resolution switch, orientation flip) is out of scope for this revision — handle it by starting a new track or storage-format file. In-codec parameter changes (H.264 SPS/PPS updates at keyframe boundaries) ride through the existing fragment stream unchanged and do not constitute a catalog change.

### MUXL fMP4 (appendable)

```
ftyp
moov (init — track config, empty sample tables, mvex present)
[GoP 1: track 1 seg, track 2 seg, ...]
[GoP 2: track 1 seg, track 2 seg, ...]
...
```

Appendable: new GoPs are byte-appended without rewriting the header. Used during livestream ingest and 24-hour streams.

### MUXL Flat MP4 (finalized)

```
ftyp
moov (populated sample tables; no mvex; faststart)
mdat (64-bit largesize envelope; payload =)
  [GoP 1: track 1 seg, track 2 seg, ...]
  [GoP 2: track 1 seg, track 2 seg, ...]
  ...
```

Top-level view: a normal flat MP4 with populated stbl. `co64` entries point at sample bytes inside the inner mdats, past each fragment's preceding moof header. The leading `uuid` of each canonical segment lives at the start of that segment's byte range; flat-MP4 parsers ignore it (uuid is a permitted ISOBMFF box at any level).

HLS byte-range view: the envelope contains canonical-segment-prefixed CMAF fragments. HLS playlist byte ranges target the `moof+mdat` portion; the leading `uuid` is informational and may be addressed separately by signature-aware players.

### Layout Arithmetic (Flat MP4)

Given `ftyp` size `F`, `moov` size `M`, per-segment `uuid` sizes `u_s`, and per-sample inner fragment sizes `f_i = moof_size_i + 8 + sample_size_i`:

- Outer mdat payload starts at `P = F + M + 16`.
- For sample `i` belonging to segment `s`, the absolute file offset is `P + (sum of all u and f preceding sample i) + moof_size_i + 8`.
- Outer `mdat.largesize` = `16 + sum(u_s) + sum(f_i)`.

### Header Synthesis

`build_synth_flat_header` in `src/flat.rs` constructs the `ftyp + moov + mdat-envelope-header` from per-segment metadata only — no sample bytes required. Each segment's metadata contributes: track byte sizes (including its leading uuid), per-sample arrays (duration, size, cts offset, sync index, offset-in-segment), and first decode time. The caller assembles the full file by concatenating the synth header with each segment's body bytes (e.g. via S3 multipart UploadPartCopy from per-segment objects).

## Box Rules

### ftyp

- **major_brand**: `muxl`
- **minor_version**: `0`
- **compatible_brands**: `[muxl, isom, iso2]`

`muxl` signals conformance. `isom`/`iso2` keep the file playable by generic ISOBMFF tools. Codec-agnostic; players use stsd for codec detection.

### Init Segment moov

The init `moov` describes track configuration with empty sample tables, zero durations, and no sample entries. Used in MUXL fMP4 storage.

Required child boxes: `mvhd`, `trak` (one per track), `mvex` (with `trex` per track).

#### mvhd

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

#### mvex

Required for fMP4 playback — signals that moof+mdat pairs follow the moov.

- **trex** (one per track):
  - **track_id**: matching the trak
  - **default_sample_description_index**: 1
  - **default_sample_duration**: 0
  - **default_sample_size**: 0
  - **default_sample_flags**: 0

All sample metadata is explicit in each trun entry, so trex defaults are all zero.

#### trak ordering

Sorted by track_id ascending. No udta, meta, or iods.

#### tkhd

- **version**: 0
- **flags**: 3 (track_enabled | track_in_movie)
- **creation_time**: 0
- **modification_time**: 0
- **duration**: 0
- **matrix, width/height, layer, alternate_group, volume**: from track config

#### mdhd

- **version**: 0
- **flags**: 0
- **creation_time**: 0
- **modification_time**: 0
- **timescale**: preserved from source track (passthrough)
- **duration**: 0
- **language**: `"und"`

#### hdlr

- **version**: 0
- **flags**: 0
- **handler_type**: `"vide"` for video, `"soun"` for audio
- **name**: empty string (name is cosmetic and varies across muxers)

#### minf

- **vmhd**: present for video tracks (default values)
- **smhd**: present for audio tracks (default values)
- **dinf**: required, contains dref
  - **dref**: one self-contained `url` entry with empty location string (signals data is in the same file)

#### stbl (init)

stsd populated with codec config, all other tables empty.

### Flat MP4 moov

Same `mvhd`/`trak`/`tkhd`/`mdhd`/`hdlr`/`minf` rules as the init segment, with:

- Populated `stbl` sample tables (see below).
- **No** `mvex`. The top-level view is non-fragmented; HLS consumers use an out-of-band init segment.
- Duration fields (`mvhd.duration`, `tkhd.duration`, `mdhd.duration`) filled in from the samples.

#### stbl (populated)

- **stsd**: same as init segment
- **stts**: RLE per-sample decode durations (media timescale)
- **ctts**: version 1 (signed), RLE, present only if any sample has a non-zero composition time offset
- **stsz**: uniform if all samples have equal size; per-sample list otherwise
- **stsc**: exactly one entry — `first_chunk=1, samples_per_chunk=1, sample_description_index=1`. Each sample is its own chunk, because each is preceded by its own inner moof+mdat header bytes.
- **co64**: one entry per sample. Entry `i` = absolute file offset of sample i's bytes inside its inner mdat (past the segment's leading `uuid` and the sample's preceding `moof`). Always 64-bit, never `stco`.
- **stss**: 1-based sync sample indices (video only; omitted for audio and all-sync tracks)

No other `stbl` child boxes (no `stsh`/`stps`/`stdp`/`padb`/`sdtp`).

### Outer mdat (flat MP4)

Always 64-bit extended size header (16 bytes: `size=1` + "mdat" + 8-byte `largesize`). Payload is the time-slice-interleaved sequence of canonical segments (§ Interleaving Order).

### edts / elst

Never emitted in the init segment's moov.

Edit lists are a pre-CMAF mechanism for expressing presentation-start offsets (e.g. a LosslessCut clip that delays one track by 9 ms to align video keyframes with an audio cut). CMAF has a native mechanism for the same thing — the per-track `tfdt` on the first fragment — so the canonical init segment drops elst and instead expects the offset to be baked into the first fragment's `base_media_decode_time`.

Round-trip:

1. **Source → MUXL.** Any leading empty-edit entries (`media_time == -1`) at the head of a source track's `elst` are summed and rescaled from the movie timescale into the track's media timescale, becoming that track's *presentation offset* (`start_offset_ticks` in the canonical sample plan). For an fMP4 input, the same value is read directly from the first fragment's `tfdt.base_media_decode_time`. Any non-empty entries on the source elst beyond the leading empty-edit shape are discarded; a canonical MUXL track's media timeline begins at `media_time == 0`.

   Per-track presentation offsets are preserved verbatim — there is no inter-track normalization. A/V sync rides on the natural delta between each track's offset; absolute time anchoring is preserved as-is. This is load-bearing for livestream-segment workflows, where each segment of a stream carries cumulative-from-stream-start tfdts, and downstream concatenation must produce monotonic output without a rebase step. Same-track-anchor inputs (a segment of a stream at the 5-second mark with both tracks at offset 5000 ticks) preserve that 5000 in the canonical bytes; both tracks emit synthesized elsts.

2. **MUXL → flat MP4.** For any track whose presentation offset is non-zero, `write_flat_mp4` synthesizes a canonical two-entry `elst` in that track's `trak`:
   - Entry 1: `segment_duration = offset_movie_ts, media_time = -1` (empty edit)
   - Entry 2: `segment_duration = media_duration_movie_ts, media_time = 0` (normal play)

   A zero offset produces no `edts` box at all.
3. **MUXL → fragments.** First fragment's `tfdt` carries the presentation offset; later fragments' tfdts follow from per-sample durations as usual. No `elst` is ever in play.

Two consequences worth noting:

- **Capture-clock anchor preserved.** A source whose first sample lands at decode_time=24000 produces canonical bytes whose first-fragment tfdt is 24000. For HLS playback this is invisible (the playlist anchors the timeline); for direct `<video src>` playback the timeline starts at the encoded position, not at zero. Callers who specifically need a "shift to time zero" transform should apply it explicitly before writing.
- **Different absolute anchors → different canonical CIDs.** Two source files with the same logical content but different leading offsets produce different canonical bytes (and therefore different CIDs). Same-logical-content / same-CID is not a property of muxl's canonical form; it never has been across all dimensions, and absolute time anchoring is a meaningful axis here. Wall-clock provenance, when needed, is carried by an upper signing layer (C2PA/S2PA), not by MUXL itself.

Source `elst` patterns outside the leading-empty-edit shape — media-time offsets used for encoder priming, rate changes, trims — are not converged by MUXL and are tracked in `open-questions.md`. A source file with a priming `elst` (e.g. `media_time = 1024` for AAC) currently loses the priming metadata in the MUXL form; playback is offset by the priming duration until a separate sample-dropping normalization lands.

## Stripped Boxes

The following are stripped entirely:

- **udta**: tool tags are non-deterministic
- **meta**: at moov and trak level
- **free / skip**: padding boxes
- **iods**: not needed
