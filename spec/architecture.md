# MUXL Architecture

This document describes the relationship between MUXL's format representations, the signing pipeline, and how deterministic canonicalization enables format-independent provenance verification.

## Core Principle

Deterministic canonicalization decouples transport, storage, and signing. The same source frames can exist in three different container formats, all derivable from each other, because the canonicalization rules are fully deterministic. Content bytes (encoded video/audio samples) never change — only the container structure around them.

## Format Representations

```
source frames
  ├─ Hang CMAF (per-frame moof+mdat) → MoQ transport, minimal latency
  ├─ canonical fMP4 (per-GoP segments) → S2PA signing, verification
  └─ flat MP4 (moov+mdat)             → archival, export, playback
```

### Hang CMAF — Transport Format

Each encoded frame is wrapped in a minimal `moof+mdat` pair (`tfhd` + `tfdt` + payload). One frame per fragment, one GoP per MoQ group. Codec configuration lives out-of-band in a MoQ catalog using WebCodecs types (`VideoDecoderConfig`, `AudioDecoderConfig`).

This is the lowest-latency representation. No sample tables, no init segment, no moov. Optimized for real-time delivery over MoQ/QUIC with group-level partial reliability (entire GoPs are dropped during congestion rather than individual frames).

MUXL does not define this format — it is defined by the [Hang specification](https://doc.moq.dev/concept/layer/hang). MUXL only needs to be able to consume it.

### Canonical fMP4 — Signing Format

Each segment is a `moof+mdat` pair containing one GoP's worth of samples across all tracks. Segments are the unit of S2PA signing — each segment carries a cryptographic signature over its canonical bytes.

Structure per segment:
- `moof`: `mfhd` (sequence number) + one `traf` per track (sorted by track_id), each containing `tfhd`, `tfdt`, `trun`
- `mdat`: sample data for all tracks in this segment, in track_id order

File layout: `ftyp` followed by repeating `moof+mdat` pairs. No top-level `moov`.

Segmentation rule: each segment begins at a video sync sample (keyframe). Audio samples are grouped with the video GoP they temporally overlap. This rule is deterministic — given the same samples with the same timestamps, the segment boundaries are always identical.

See `canonical-form.md` for detailed box-level specification.

### Flat MP4 — Archival/Export Format

Standard MP4 with a single `moov` containing complete sample tables (`stts`, `stsz`, `stco`, `stsc`, `stss`, `ctts`) and a single `mdat` containing all sample data.

Layout: `ftyp`, `mdat`, `moov`. Maximally compatible with players, editors, and media tools.

This format is generated on demand from canonical fMP4 and can be deterministically converted back. The round-trip property (flat → fMP4 → flat produces identical bytes) is a hard requirement.

See `canonical-form.md` for detailed box-level specification.

## Round-Trip Properties

The three representations are connected by deterministic transformations:

```
Hang CMAF ──canonicalize──► canonical fMP4 ◄──segment──── flat MP4
                                │                            ▲
                                └──────────flatten───────────┘
```

- **Hang CMAF → canonical fMP4**: Accumulate frames into GoP-sized segments. Move codec config from catalog into `stsd` entries. Construct `trun` sample tables from per-frame metadata. Apply canonical ordering and metadata normalization.

- **canonical fMP4 → flat MP4** (`flatten`): Consolidate all segment `trun` tables into `moov` sample tables (`stts`, `stsz`, `stco`, `stsc`, `ctts`, `stss`). Concatenate all `mdat` payloads. Write single `moov` at end.

- **flat MP4 → canonical fMP4** (`segment`): Walk `moov` sample tables to find keyframe boundaries (`stss`). Slice samples into GoP-sized segments. Construct per-segment `moof` boxes from the sample table data. Each segment's bytes are identical to the original canonical fMP4 segment.

The last transformation is what enables signature verification from a flat MP4 — re-segment, then verify each segment's S2PA signature against the reconstructed canonical bytes.

**Hang CMAF → flat MP4** is not a direct path — it always goes through canonical fMP4. This ensures there is exactly one canonical representation that signatures are computed over.

## Signing Pipeline

For a live stream:

```
encoder → frames → MoQ transport (Hang CMAF, per-frame)
                        │
                   [real-time viewers see frames immediately]
                        │
                   accumulate GoP
                        │
                   canonicalize → canonical fMP4 segment
                        │
                   S2PA sign segment
                        │
                   append to archive (canonical fMP4)
                        │
                   publish signature via S2PA manifest
                        │
                   [verifiers can now check this GoP]
```

Key property: **signing is not on the hot path**. Frames are transmitted immediately via Hang CMAF. The signer runs ~1 GoP behind, accumulating frames, canonicalizing, signing, and publishing. This means:

- **Zero additional latency** for viewers who don't need real-time verification
- **~1 GoP latency** (typically 1-2 seconds) for verifiers who want to check signatures inline
- **Retroactive verification** is always possible from the archive

## Dynamic Stream Changes

Mobile WebRTC/WHIP sources may change resolution or orientation mid-stream (phone rotation, camera switch). This produces new H.264 SPS/PPS (or AV1 sequence headers) at keyframe boundaries.

In the signing pipeline:
1. Resolution change always aligns with a keyframe (codec requirement)
2. Keyframe starts a new GoP → new canonical fMP4 segment
3. New segment's `traf.tfhd` references a new `sample_description_index`
4. In flat MP4, `stsd` accumulates multiple entries; `stsc` tracks the transitions

Because segment boundaries align with codec parameter changes, each segment is self-consistent — it references exactly one set of codec parameters.

## Relationship to S2PA

S2PA (Simple Standard for Provenance and Authenticity) defines how signatures are attached to content. MUXL defines what bytes are signed.

S2PA is agnostic to container format — it signs arbitrary byte ranges. MUXL's role is to ensure those byte ranges are deterministically reproducible. Given the same source frames, any implementation of MUXL must produce identical canonical fMP4 segment bytes, which means S2PA signatures computed by one implementation can be verified by any other.

The S2PA manifest maps segments to signatures. It is stored and transmitted separately from the media data (e.g., in a MoQ catalog track, or as a sidecar file alongside a flat MP4 export).
