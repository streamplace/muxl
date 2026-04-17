# MUXL Architecture

This document describes the relationship between MUXL's format representations and how deterministic canonicalization enables format-independent content addressing and provenance verification.

## Core Principle

Deterministic canonicalization decouples transport, storage, and signing. The same source frames can exist in multiple container formats, all derivable from each other, because the canonicalization rules are fully deterministic. Content bytes (encoded video/audio samples) never change — only the container structure around them.

## Format Representations

```
source frames
  ├─ Hang CMAF (per-frame moof+mdat)            → MoQ transport, minimal latency
  ├─ MUXL segment (per-GoP moof+mdat pairs)     → canonical byte sequence, signing unit
  └─ MUXL fMP4 (ftyp+moov+segments)     → appendable storage, playback
```

### Hang CMAF — Transport Format

Each encoded frame is wrapped in a minimal `moof+mdat` pair (`tfhd` + `tfdt` + payload). One frame per fragment, one GoP per MoQ group. Codec configuration lives out-of-band in a MoQ catalog using WebCodecs types (`VideoDecoderConfig`, `AudioDecoderConfig`).

This is the lowest-latency representation. No sample tables, no init segment, no moov. Optimized for real-time delivery over MoQ/QUIC with group-level partial reliability (entire GoPs are dropped during congestion rather than individual frames).

MUXL does not define this format — it is defined by the [Hang specification](https://doc.moq.dev/concept/layer/hang). MUXL only needs to be able to consume it.

### MUXL Segment — Canonical Byte Sequence

A MUXL segment contains one track's per-frame moof+mdat pairs for one GoP. Each GoP produces one segment per track. This is the canonical byte sequence that content hashes and signatures are computed over.

```
GoP 1:
  segment (track 1): moof+mdat, moof+mdat, ...   ← video frames
  segment (track 2): moof+mdat, moof+mdat, ...   ← audio packets
```

Key properties:

- **Per-track segments**: each track is independently hashable, addressable, and concatenatable
- **Byte-range addressable**: in the MUXL fMP4, all segments for a track are contiguous, enabling HLS byte-range playlists over a single file
- **Blindly concatenatable**: segments for the same track can be appended by simple byte concatenation
- **Init data is out-of-band**: track initialization metadata (codec config, timescales) is not part of the segment; it comes from the MUXL fMP4 file header or an external source (e.g., S2PA manifest)
- **Deterministic**: given the same source frames, any MUXL implementation produces identical segment bytes

Segmentation rule: segment boundaries are driven by video sync samples (keyframes). Audio samples are grouped with the video GoP they temporally overlap. This rule is deterministic — given the same samples with the same timestamps, the segment boundaries are always identical.

### MUXL fMP4 — Storage Format

For storage, the MUXL fMP4 prepends an init segment (ftyp + moov with empty sample tables) to per-track segments grouped by track:

```
ftyp + moov (init, empty sample tables)
[track 1 segments: GoP 1, GoP 2, ...]   ← e.g., all video
[track 2 segments: GoP 1, GoP 2, ...]   ← e.g., all audio
```

This is a valid fMP4 file. Each track's data is a contiguous byte range, enabling HLS byte-range playlists to reference individual tracks within the single file. Tracks are ordered by track_id ascending.

The init segment is stable as long as the track configuration doesn't change. When codec parameters change (e.g., resolution switch), a new init segment is needed (see Open Questions).

The init segment is deterministic: given the same track configuration, any MUXL implementation produces identical init bytes.

## Round-Trip Properties

```
Hang CMAF ──canonicalize──► MUXL segments ──prepend init──► MUXL fMP4
```

- **Hang CMAF → MUXL segments**: Accumulate per-frame fragments into GoP-sized segments. Apply canonical ordering and metadata normalization.

- **MUXL segments → MUXL fMP4**: Derive init segment from track metadata. Prepend to concatenated segments.

## Signing Pipeline

MUXL defines what bytes are canonical. S2PA (or any signing system) defines how to sign them. MUXL has no dependency on S2PA.

For a live stream with S2PA:

```
encoder → frames → MoQ transport (Hang CMAF, per-frame)
                        │
                   [real-time viewers see frames immediately]
                        │
                   accumulate GoP
                        │
                   build MUXL segment (per-track moof+mdat)
                        │
                   hash each track's moof+mdat independently
                        │
                   S2PA sign (per-track hashes → signature)
                        │
                   append segment to MUXL fMP4
                        │
                   publish updated S2PA manifest
                        │
                   [verifiers can now check this GoP]
```

Key properties:

- **Signing is not on the hot path**: frames transmit immediately; the signer runs ~1 GoP behind
- **Zero additional latency** for viewers who don't need real-time verification
- **~1 GoP latency** (typically 1-2 seconds) for inline verification
- **Retroactive verification** is always possible from the MUXL fMP4 + manifest

### Per-Track Signing Model

Each per-track segment is hashed independently:

```
GoP 1:
  segment (track 1, video): bytes → hash_v1
  segment (track 2, audio): bytes → hash_a1
  segment (track 3, audio): bytes → hash_a2
```

This supports:

- **Subset verification**: verify only the video track without touching audio
- **Track independence**: drop or replace a track without invalidating the others
- **Multi-track streams**: multiple synced video and audio tracks

## Dynamic Stream Changes

Mobile WebRTC/WHIP sources may change resolution or orientation mid-stream (phone rotation, camera switch). This produces new H.264 SPS/PPS (or AV1 sequence headers) at keyframe boundaries.

In the pipeline:

1. Resolution change always aligns with a keyframe (codec requirement)
2. Keyframe starts a new GoP → new MUXL segment
3. New segment references updated codec parameters via the init data

Because segment boundaries align with codec parameter changes, each segment is self-consistent — it references exactly one set of codec parameters per track.
