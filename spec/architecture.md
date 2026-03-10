# MUXL Architecture

This document describes the relationship between MUXL's format representations, the signing pipeline, and how deterministic canonicalization enables format-independent provenance verification.

## Core Principle

Deterministic canonicalization decouples transport, storage, and signing. The same source frames can exist in multiple container formats, all derivable from each other, because the canonicalization rules are fully deterministic. Content bytes (encoded video/audio samples) never change — only the container structure around them.

## Format Representations

```
source frames
  ├─ Hang CMAF (per-frame moof+mdat)     → MoQ transport, minimal latency
  ├─ MUXL segment (uuid+moof+mdat ×N)    → signing, verification, storage
  ├─ canonical fMP4 (ftyp+moov+segments)  → archival, appendable storage
  └─ flat MP4 (ftyp+mdat+moov)           → export, universal playback
```

### Hang CMAF — Transport Format

Each encoded frame is wrapped in a minimal `moof+mdat` pair (`tfhd` + `tfdt` + payload). One frame per fragment, one GoP per MoQ group. Codec configuration lives out-of-band in a MoQ catalog using WebCodecs types (`VideoDecoderConfig`, `AudioDecoderConfig`).

This is the lowest-latency representation. No sample tables, no init segment, no moov. Optimized for real-time delivery over MoQ/QUIC with group-level partial reliability (entire GoPs are dropped during congestion rather than individual frames).

MUXL does not define this format — it is defined by the [Hang specification](https://doc.moq.dev/concept/layer/hang). MUXL only needs to be able to consume it.

### MUXL Segment — Signing Format

A MUXL segment represents one GoP of content across one or more tracks. It is the unit of S2PA signing. Each track within the segment is independently hashable, allowing individual tracks to be verified, added, or dropped without invalidating the others.

Structure of a MUXL segment:

```
uuid(c2pa)                          ← S2PA signature/provenance box
moof(track 1) + mdat(track 1)      ← video frames for this GoP
moof(track 2) + mdat(track 2)      ← audio packets for this GoP
moof(track 3) + mdat(track 3)      ← second audio track, etc.
```

Key properties:
- **Per-track moof+mdat pairs**: each track gets its own moof+mdat within the segment, making each track independently hashable without parsing
- **uuid box first**: the S2PA provenance data (signature, per-track content hashes) appears before the media data; streaming consumers see the signature before the content
- **Blindly concatenatable**: multiple segments can be concatenated by simple byte appending, since there is no ftyp or moov to conflict
- **Not self-contained**: track initialization metadata (codec config, timescales) lives in the S2PA manifest as CBOR, not in the segment itself; an S2PA-aware consumer reconstructs the init data from the manifest

The per-track content hashes in the uuid box reference the bytes of each track's moof+mdat pair. The S2PA manifest (stored separately) contains the track initialization metadata and signs over the per-track hashes.

Segmentation rule: each segment begins at a video sync sample (keyframe). Audio samples are grouped with the video GoP they temporally overlap. This rule is deterministic — given the same samples with the same timestamps, the segment boundaries are always identical.

### Canonical fMP4 — Archive Format

For storage, the canonical fMP4 prepends an init segment (ftyp + moov with empty sample tables) to a sequence of MUXL segments:

```
ftyp + moov (init, empty sample tables)
uuid + moof+mdat + moof+mdat ...       ← GoP 1
uuid + moof+mdat + moof+mdat ...       ← GoP 2
...
```

This is a valid fMP4 file — players skip unknown uuid boxes and process the moof+mdat pairs normally. New GoPs are appended without modifying existing data (crash-safe, no finalization step).

The init segment (ftyp+moov) is stable as long as the track configuration doesn't change. When codec parameters change (e.g., resolution switch), a new init segment is emitted at the point of change.

The init segment is deterministic: given the same track configuration, any MUXL implementation produces identical init bytes. It can be derived from the S2PA manifest's track metadata.

### Flat MP4 — Export Format

Standard MP4 with a single `moov` containing complete sample tables and a single `mdat` containing all sample data. Layout: `ftyp`, `mdat`, `moov`.

Maximally compatible with players, editors, and media tools. Generated on demand from canonical fMP4. Can be deterministically converted back to canonical fMP4 by re-segmenting at keyframe boundaries.

See `canonical-form.md` for detailed box-level specification.

## Round-Trip Properties

The representations are connected by deterministic transformations:

```
Hang CMAF ──canonicalize──► MUXL segments ──prepend init──► canonical fMP4
                                │                                │
                                │                           flatten ↓
                                │                            flat MP4
                                │                                │
                                ◄────────── re-segment ──────────┘
```

- **Hang CMAF → MUXL segments**: Accumulate per-frame fragments into GoP-sized segments. Construct per-track moof+mdat pairs. Add uuid box with S2PA provenance.

- **MUXL segments → canonical fMP4**: Derive init segment from track metadata. Prepend to concatenated segments.

- **canonical fMP4 → flat MP4** (`flatten`): Consolidate all moof/trun tables into moov sample tables. Concatenate all mdat payloads. Write single moov at end.

- **flat MP4 → MUXL segments** (`segment`): Walk moov sample tables to find keyframe boundaries (stss). Slice samples into GoP-sized segments. Construct per-track moof+mdat pairs. Each segment's content bytes are identical to the original MUXL segment.

The last transformation enables signature verification from a flat MP4 export: re-segment, recompute per-track hashes, check against the S2PA manifest.

## Signing Pipeline

For a live stream:

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
                   S2PA sign (per-track hashes → signature → uuid box)
                        │
                   append to archive fMP4
                        │
                   publish updated S2PA manifest
                        │
                   [verifiers can now check this GoP]
```

Key properties:
- **Signing is not on the hot path**: frames are transmitted immediately via Hang CMAF; the signer runs ~1 GoP behind
- **Zero additional latency** for viewers who don't need real-time verification
- **~1 GoP latency** (typically 1-2 seconds) for verifiers who want to check signatures inline
- **Retroactive verification** is always possible from the archive
- **Per-track independence**: a translator dubbing new audio doesn't invalidate the video provenance; tracks can be added or dropped and remaining signatures still hold

## Per-Track Signing Model

Each track within a GoP segment is hashed independently:

```
GoP 1:
  track 1 (video): moof+mdat bytes → hash_v1
  track 2 (audio): moof+mdat bytes → hash_a1
  track 3 (audio): moof+mdat bytes → hash_a2

uuid box: { per_track_hashes: { 1: hash_v1, 2: hash_a1, 3: hash_a2 }, signature: ... }
```

The S2PA manifest contains:
- Track initialization metadata (codec config, timescales, handler types) as CBOR
- References to segment signatures
- Enough information to reconstruct the init segment (ftyp+moov) for playback

This model supports:
- **Subset verification**: verify only the video track without touching audio
- **Track independence**: drop or replace a track without invalidating the others
- **Multi-track streams**: multiple synced video and audio tracks

## Dynamic Stream Changes

Mobile WebRTC/WHIP sources may change resolution or orientation mid-stream (phone rotation, camera switch). This produces new H.264 SPS/PPS (or AV1 sequence headers) at keyframe boundaries.

In the signing pipeline:
1. Resolution change always aligns with a keyframe (codec requirement)
2. Keyframe starts a new GoP → new MUXL segment
3. New segment references updated codec parameters
4. S2PA manifest records the track configuration change
5. A new init segment is derivable from the updated manifest

Because segment boundaries align with codec parameter changes, each segment is self-consistent — it references exactly one set of codec parameters per track.

## Relationship to S2PA

MUXL defines what bytes are signed. S2PA defines how signatures are attached and verified.

- **MUXL segments** contain a `uuid(c2pa)` box with S2PA provenance data (signature, per-track content hashes)
- **S2PA manifest** contains track initialization metadata, segment references, and signer identity — stored and transmitted separately (e.g., in a MoQ catalog track, or as a sidecar file)
- **MUXL segment format depends on S2PA**: the uuid box is part of the canonical segment structure; a MUXL segment without provenance is not a defined format

Given the same source frames, any MUXL implementation must produce identical per-track moof+mdat bytes, which means S2PA signatures computed by one implementation can be verified by any other.
