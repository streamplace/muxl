# MUXL Canonical Form Specification

This document defines the canonical byte layout for MUXL. The formal specification is at [dasl.ing/muxl](https://dasl.ing/muxl); this file mirrors the byte-level rules for implementers working in the muxl repository.

All choices are provisional and subject to revision after playback testing.

## Layered Model

MUXL is a three-layer stack:

- **MUXL fragment** — one encoded sample (video frame, audio packet, or timed-text sample) in a minimal `moof+mdat` pair. The smallest unit. Bit-identical regardless of how it's transported or stored.
- **MUXL canonical segment** — a `uuid` box carrying the per-track catalog as a DRISL payload, followed by one track's fragments for one GoP. The unit of content addressing.
- **Synthesized storage format** — fMP4 (appendable) or flat MP4 (finalized faststart) wrapping N canonical segments together with a derived ISOBMFF header. The header is synthesized from the segments' embedded catalogs. Canonical segments are recoverable byte-for-byte from any storage format.

Signing and provenance — c2pa manifests, S2PA assertions, signed claim chains — are layered on top of MUXL by a separate signing layer, implemented by the `muxl` CLI's signing commands. MUXL defines what bytes are canonical; the signing layer defines how those bytes are attested. No c2pa structure appears in MUXL's canonical form.

## MUXL Fragment

One sample, one `moof+mdat` pair.

### moof

Each moof covers exactly one sample from one track.

- **mfhd**: video and audio `sequence_number` increments by 1 per fragment within a track, starting at `1` for the first fragment of each track. Per-track counters are independent and remain monotonic across GoPs. **Text tracks instead restart at `1` at each GoP**, making freshly attached text reproducible without a prior fragment count. Storage wrappers preserve all sequence numbers verbatim. Playback timing comes from `tfdt`.
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
uuid (muxl catalog box; uuid = e6404ea2-8f01-4305-98da-7bec3c2a9173)
moof+mdat (sample 1)
moof+mdat (sample 2)
...
moof+mdat (sample K)
```

Each canonical segment carries fragments for exactly one track and one GoP. A multi-track GoP produces multiple canonical segments — one per track.

The leading uuid is _always_ present — never omitted — so segment boundaries are unambiguous at the byte level. The 16-byte UUID identifier is `e6404ea2-8f01-4305-98da-7bec3c2a9173`.

### uuid Body

The `uuid` box body is a single DRISL-encoded MUXL catalog ([[drisl]]) describing exactly one track — one entry in `video.renditions`, `audio.renditions`, _or_ `text.renditions`, never more than one. A catalog with no text track encodes no `text` key at all, so the uuid bytes of video and audio segments are unchanged by text support. The catalog is the entire body of the box; no JSON-LD wrapper, no c2pa manifest, no signature claim.

DRISL canonical CBOR encoding makes the uuid body byte-deterministic: any two MUXL implementations producing a canonical segment for the same track configuration produce byte-identical uuid box bytes.

### Tamper Resistance

Modifying the catalog or any fragment changes the canonical segment's bytes, which changes its CID. Detection at the muxl layer is by content-address comparison alone. Cryptographic provenance (proving _who_ generated the bytes, not just _that they are these specific bytes_) is added by the signing layer.

### Segmentation Rule

Segment boundaries are driven by video sync samples (keyframes). A new segment begins at each video keyframe. Audio samples are grouped with the video GoP they temporally overlap.

For audio-only streams (no video reference), segments are 1-second wall-clock spans.

Timed-text tracks never define a boundary. Each GoP gets one text segment per text track, clipped and padded to exactly the GoP's span (see § Timed Text (WebVTT) → Segmentation).

Given the same samples with the same timestamps, segment boundaries are always identical.

### Per-Segment Properties

- **`mfhd.sequence_number`**: video/audio use per-track 1-based counters monotonic across GoPs; text uses per-GoP 1-based counters. A fresh text segment therefore requires only its configuration, span, and cues, not stream context. Storage-format synthesizers preserve these values verbatim.
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

### Metafile Wire Format

The **metafile** is the durable, versioned wire form of that per-segment metadata — what a consumer archives (exactly one per canonical segment) so it can synthesize a flat-MP4 faststart header for any contiguous segment range on demand, without touching the blob. This makes "flat-MP4 VOD" content-addressable: `[synthesized header][canonical blob range]` is a byte-range-seekable MP4 whose `moov` is exact.

It is DRISL / dag-cbor: one self-contained `segment` value per canonical `.m4s` (per-track), in canonical interleave order — there is no separate init blob. Each metafile carries that segment's single-track `catalog` (the codec config the `moov` needs) alongside the tables, mirroring how the canonical `.m4s` already embeds its catalog per segment, so the archive is a flat per-segment store with no init coordination and the catalog is negligible next to the per-sample arrays. Map keys are stringified track ids. The field names match the live event stream (`src/cbor.rs` `CborEvent`); a metafile is its payload-free subset (no `tracks`) plus the catalog, so one consumer-side decoder reads both. Each carries `version` (`METAFILE_VERSION`, currently `1`); the schema evolves additively and the version is the hard gate.

```
segment = { "type": "segment", "version": uint,
            "catalog": <Catalog>,                  ; this segment's single-track catalog
            "samples":            { tid: { durations, sizes, cts_offsets, sync_indices, offsets } },
            "track_byte_sizes":   { tid: uint },   ; on-disk bytes incl. any c2pa signature
            "first_decode_times": { tid: uint },   ; tfdt of first sample (track ticks)
            "durations":          { tid: uint },   ; HLS convenience
            "sample_counts":      { tid: uint },   ; HLS convenience
            "body_size": uint, "duration_us": uint }
```

`samples[tid].offsets` and `track_byte_sizes[tid]` are measured from the exact bytes stored in the blob, so they must be taken **after** c2pa signing (the signed uuid prefix shifts every offset and grows the size); the per-sample `durations`/`sizes`/`cts`/`sync` are signing-invariant. The metafile is a plain struct, not a `#[serde(tag)]` enum, so DRISL decodes the catalog's byte fields (`avcC`/`esds`) directly — a tagged enum buffers them through an intermediate that does not round-trip.

**Synthesis** (`muxl::metafile::synthesize_flat_header`, CLI `muxl flat-header`) consumes an ordered set of `segment` metafiles, aggregates their single-track catalogs into the multi-track `moov`, regroups them into GoPs (a new GoP begins when a track id repeats — the same rule `unwrap` orders by), and runs `build_synth_flat_header`. muxl owns all offset math: the synthesized `co64` already resolves to `header_len + body_offset + per_sample_offset`, so the caller passes **no** base offset — it serves the header bytes immediately followed by the segment bodies, in input order. A sub-range synthesizes its own header (the range's first `first_decode_times` re-anchors the `elst` to presentation zero), so clips fall out for free. Synthesis is a pure function of its input, so the header is content-addressable and cacheable.

The metafiles are emitted by `muxl metafile` (one self-contained metafile per segment, streaming — constant memory over a multi-GB blob) or built directly from canonical segment bytes via `muxl::metafile::segment_metafile`.

## Timed Text (WebVTT)

MUXL carries WebVTT as ISO/IEC 14496-30 timed text (sample entry `wvtt`). A text track is a first-class track: it has its own catalog rendition, its own canonical segment per GoP, and its own entry in every storage format, exactly like a video or audio track. WebVTT is the only text format. Muxer-specific subtitle formats (`tx3g`, `stpp`) are skipped on extraction.

### Catalog

Text renditions live in a top-level `text` group, keyed `text{track_id}` like the other groups:

```json
"text": { "renditions": { "text3": {
  "codec": "wvtt",
  "container": { "kind": "cmaf", "timescale": 1000, "trackId": 3 },
  "language": "en-US",
  "label": "captions",
  "config": "WEBVTT"
} } }
```

- **codec**: always `"wvtt"`.
- **container.timescale**: always `1000`. Cue times are millisecond-precise in WebVTT, so a millisecond timescale represents them exactly.
- **language**: a BCP 47 tag (`und` when unknown).
- **label**: optional human-readable label. Absent (not empty) when there is none.
- **config**: the WebVTT file header, which is the `vttC` body. It starts with `WEBVTT`. The minimal value is `"WEBVTT"`. Header blocks such as `STYLE` belong here, not in samples.

The `text` group is a MUXL extension; Hang catalogs have no text group.

### Init

The text `trak` follows the common rules (§ Init Segment moov), with:

- **tkhd**: `volume` 0, `width`/`height` 0, `alternate_group` 0, identity matrix.
- **mdhd.timescale**: 1000.
- **mdhd.language**: the ISO 639-2/T code of the tag's primary language subtag. Two-letter subtags map through ISO 639-1 (`en` → `eng`). Three-letter ISO 639-2/B codes are rewritten to their /T form (`ger` → `deu`). Anything else (`x-`/`i-` tags, unknown codes) is `und`.
- **elng** (ExtendedLanguageBox, ISO/IEC 14496-12 § 8.4.6): carries the full BCP 47 tag, but only when `mdhd.language` alone would not reproduce it on extraction. `en` is written as `mdhd eng` with no `elng`; `en-US` is written as `mdhd eng` plus `elng "en-US"`. `elng` sits between `hdlr` and `minf`.
- **hdlr**: `handler_type = "text"`, empty name.
- **minf**: `nmhd` (no `vmhd`/`smhd`), plus the common `dinf`.
- **stsd**: exactly one `wvtt` sample entry, `data_reference_index = 1`, containing:
  - `vttC` = the catalog `config` string.
  - `vlab` = the catalog `label`, only when the label is non-empty.
  - No `btrt`.

On extraction, the `text`, `subt`, and `sbtl` handlers are all accepted, but only a `wvtt` sample entry yields a rendition. `language` comes from `elng` when it is present and non-empty. Otherwise it comes from `mdhd`, shortened to ISO 639-1 where a two-letter code exists (`eng` → `en`), so ordinary muxer output yields a BCP 47 tag. An empty `vttC` is read as `"WEBVTT"`, and an empty `vlab` as no label.

### Samples

A WebVTT sample covers a time interval and holds the complete set of cues active for the whole interval:

- **One or more active cues**: one `vttc` (VTTCueBox) per cue. Each `vttc` holds, in order, an `iden` (CueIDBox) only when the cue has a non-empty identifier, an `sttg` (CueSettingsBox) only when the cue has non-empty settings, and a `payl` (CuePayloadBox) with the cue text.
- **No active cue**: a single `vtte` (VTTEmptyCueBox). Every gap in the timeline is an explicit `vtte` sample, so a text track's samples tile its timeline with no holes.

Determinism rules:

- **Canonical cue order.** The `vttc` boxes in a sample are sorted by `(text, id, settings)`, compared as UTF-8 bytes. Source cue order never affects the bytes.
- **Overlaps split into disjoint samples.** The timeline is cut at every cue start and every cue end. Each resulting interval becomes one sample containing every cue active over it. Overlapping cues therefore never produce overlapping samples: two cues that overlap by 500 ms yield three samples (first cue alone, both cues, second cue alone).
- **No per-sample timing boxes.** A sample's time is its decode time and duration, so `ctim` (CueTimeBox), `vsid` (CueSourceIDBox), and `vtta` (VTTAdditionalTextBox) are not emitted.
- **Sync and timing.** Every text sample is a sync sample (`trun` flags `0x02000000`) with zero composition offset. Its duration is the length of its interval. Zero-length intervals are never emitted.
- **Equivalent intervals.** Adjacent intervals with identical encoded active cue sets coalesce within a GoP. Identical contiguous pieces merge when reading cues; distinct adjacent cues need distinct IDs if their boundary must survive.

### Segmentation

Text tracks follow the GoP structure that the reference track defines. The reference track is the first video track, otherwise the first audio track. A text track is never the reference while any video or audio track exists.

- **Boundary in text ticks.** A GoP that starts at reference decode time `t_ref` (reference timescale `ts_ref`) starts at `floor(t_ref * 1000 / ts_ref)` on the text track. Flooring is part of the canonical form. The flat-MP4 writer assigns text samples to GoPs with the same floored boundary, so a sample that starts exactly on a boundary lands in the GoP it opens rather than being pulled into the previous GoP by microsecond rounding.
- **Clipping.** A cue that spans a GoP boundary is cut at the boundary. The cue appears, complete and unchanged, in a sample on each side.
- **Padding.** Each GoP's text segment covers its GoP span exactly, from the GoP's text start to the next GoP's text start. Time with no active cue at the start, middle, or end of the span is filled with `vtte` samples. A text track therefore has a segment in every GoP, even a GoP with no captions, and the text segments of consecutive GoPs tile the stream timeline.
- **GoP duration.** A text segment never extends a GoP's playable duration (`duration_us`). That duration comes from the GoP's video and audio tracks. It falls back to the text span only for a GoP that has no video or audio track.
- **Text-only streams.** The smallest-id text track defines 1-second spans anchored at its first sample, with a shorter final span. Input text samples must arrive before the boundary that closes their span; live segmentation cannot retroactively modify an emitted GoP.

### Storage formats and hashing

Text canonical segments are interleaved, unwrapped, and recovered byte-for-byte exactly like video and audio segments (§ Interleaving Order, § Round-Trip Property). In the flat MP4, the text `trak` has populated `stts`/`stsz`/`stsc`/`co64` like any track, with no `stss` (every sample is sync) and no `ctts`. Its `co64` entries point at the `vttc`/`vtte` payloads inside the inner mdats.

A text segment is a canonical segment. Its content address and any signature cover its whole byte range (uuid catalog plus every `moof+mdat`), with the same per-track hashing and signing as video and audio. Dropping, replacing, or verifying a text track never affects the other tracks.

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
- **language**: `"und"` for video and audio. Timed-text tracks carry their language; see § Timed Text (WebVTT) → Init.

#### hdlr

- **version**: 0
- **flags**: 0
- **handler_type**: `"vide"` for video, `"soun"` for audio, `"text"` for timed text
- **name**: empty string (name is cosmetic and varies across muxers)

#### minf

- **vmhd**: present for video tracks (default values)
- **smhd**: present for audio tracks (default values)
- **nmhd**: present for timed-text tracks (ISO/IEC 14496-30 § 7.3)
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
- **stss**: 1-based sync sample indices (video only; omitted for audio, timed text, and all-sync tracks)

No other `stbl` child boxes (no `stsh`/`stps`/`stdp`/`padb`/`sdtp`).

### Outer mdat (flat MP4)

Always 64-bit extended size header (16 bytes: `size=1` + "mdat" + 8-byte `largesize`). Payload is the time-slice-interleaved sequence of canonical segments (§ Interleaving Order).

### edts / elst

Never emitted in the init segment's moov.

Edit lists are a pre-CMAF mechanism for expressing presentation-start offsets (e.g. a LosslessCut clip that delays one track by 9 ms to align video keyframes with an audio cut). CMAF has a native mechanism for the same thing — the per-track `tfdt` on the first fragment — so the canonical init segment drops elst and instead expects the offset to be baked into the first fragment's `base_media_decode_time`.

Round-trip:

1. **Source → MUXL.** Any leading empty-edit entries (`media_time == -1`) at the head of a source track's `elst` are summed and rescaled from the movie timescale into the track's media timescale, becoming that track's _presentation offset_ (`start_offset_ticks` in the canonical sample plan). For an fMP4 input, the same value is read directly from the first fragment's `tfdt.base_media_decode_time`. Any non-empty entries on the source elst beyond the leading empty-edit shape are discarded; a canonical MUXL track's media timeline begins at `media_time == 0`.

   In the **canonical bytes**, per-track presentation offsets are preserved verbatim — there is no inter-track normalization, and absolute time anchoring is kept as-is. This is load-bearing for livestream-segment workflows, where each segment of a stream carries cumulative-from-stream-start tfdts, and downstream concatenation must produce monotonic output without a rebase step. A same-track-anchor input (a segment of a stream at the 5-second mark with both tracks at offset 5000 ticks) preserves that 5000 in the canonical bytes. (The flat MP4 *presentation* rebases that shared base away — see step 2 — but it does so in the synthesized `moov` only, leaving the canonical-byte tfdts intact, so concatenation stays monotonic.)

2. **MUXL → flat MP4.** The flat MP4 is a *presentation* wrapper, so its synthesized `moov` is rebased to begin playback at presentation time zero. `build_synth_flat_header` takes the global-minimum presentation offset across all tracks (the common base a mid-stream VOD inherits) and subtracts it, so the earliest track lands at offset zero and the others keep only their *residual* skew relative to it:
   - The anchor (earliest) track gets **no `edts` box** — it plays from zero.
   - Each remaining track gets a canonical two-entry `elst`:
     - Entry 1: `segment_duration = residual_movie_ts, media_time = -1` (empty edit)
     - Entry 2: `segment_duration = media_duration_movie_ts, media_time = 0` (normal play)
   - A track whose residual is zero (its offset equalled the anchor) gets no `edts` box.

   The rebase rewrites the synthesized `moov` only — the canonical segment bytes (and their `tfdt`s) are written from the original absolute offsets, so step 1's concatenation continuity is unaffected. This makes a VOD wrapped from a mid-stream point (every track's first `tfdt` tens of seconds into a live stream) play as an ordinary file from zero, instead of presenting that offset as a leading empty edit followed by content.

3. **MUXL → fragments.** First fragment's `tfdt` carries the presentation offset; later fragments' tfdts follow from per-sample durations as usual. No `elst` is ever in play.

Two consequences worth noting:

- **Capture-clock anchor preserved in the bytes; flat playback starts at zero.** A source whose first sample lands at decode_time=24000 produces canonical bytes whose first-fragment tfdt is 24000 — preserved for concatenation and provenance. The flat MP4 *presentation*, however, rebases to zero by default (step 2), so direct `<video src>` playback begins at the start of content rather than after a 24000-tick empty edit. HLS playback was always anchored by the playlist regardless.
- **Different absolute anchors → different canonical CIDs.** Two source files with the same logical content but different leading offsets produce different canonical bytes (and therefore different CIDs). Same-logical-content / same-CID is not a property of muxl's canonical form; it never has been across all dimensions, and absolute time anchoring is a meaningful axis here. Wall-clock provenance, when needed, is carried by an upper signing layer (C2PA/S2PA), not by MUXL itself.

Source `elst` patterns outside the leading-empty-edit shape — media-time offsets used for encoder priming, rate changes, trims — are not converged by MUXL and are tracked in `open-questions.md`. A source file with a priming `elst` (e.g. `media_time = 1024` for AAC) currently loses the priming metadata in the MUXL form; playback is offset by the priming duration until a separate sample-dropping normalization lands.

## Stripped Boxes

The following are stripped entirely:

- **udta**: tool tags are non-deterministic
- **meta**: at moov and trak level
- **free / skip**: padding boxes
- **iods**: not needed
