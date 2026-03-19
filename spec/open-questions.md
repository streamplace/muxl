# Sources of Nondeterminism

Issues that need further investigation before the canonical form is finalized.

## mfhd sequence number determinism

The `mfhd` sequence_number in each moof is currently globally incrementing across the stream. This means the same content produces different moof bytes depending on where it appears in a stream — a 1-hour file and a 30-minute file containing the last 30 minutes of the same stream will have identical frame data but different sequence numbers, breaking content-addressed identity.

Neither ffmpeg nor GStreamer use this field during demuxing; the ISOBMFF spec says it's for "detecting loss/reordering" in streaming contexts.

Options:

1. **Zero all sequence numbers**: simplest, maximally deterministic. Risk: some proprietary decoder or player might reject or mishandle repeated moof with sequence_number=0.
2. **Per-segment numbering** (reset to 1 at each segment boundary): each segment is independently numbered starting at 1. Maintains monotonicity within a playback session for any given segment. Segments are identical regardless of position in the stream.
3. **Global numbering** (current): simple, spec-compliant, but prevents content-identical segments from being byte-identical.

Need to verify playback behavior of options 1 and 2 across a range of decoders (browsers, mobile players, hardware decoders, smart TVs) before committing to a choice.

## Timescale normalization

Currently, media timescales are passed through from the source (e.g., 16000 for 60fps video from GStreamer, 60000 from ffmpeg). This means the same logical content from different encoders produces different timescales in the init segment and different duration values in trun entries.

For true canonicalization (byte-identical output from identical content regardless of source), we'd need to normalize to a canonical timescale per track type and rescale all durations accordingly.

Considerations:

1. **Passthrough** (current): simple, no rounding errors, but timescale is encoder-dependent. Two files with identical frames but different source encoders won't produce identical MUXL output.
2. **Canonical timescale**: e.g., 60000 for video, 48000 for audio. Requires rescaling all sample durations, which can introduce rounding errors for timescales that don't divide evenly. Risk of accumulated drift over long streams.
3. **Least-common-multiple approach**: could pick a timescale that's a multiple of common values (e.g., 240000 for video covers 24/25/30/60fps). Larger numbers, but exact for common frame rates.

Timescale passthrough is fine for the livestream ingest use case (single source encoder), but needs resolution for cross-encoder canonicalization.

## MUXL brand in ftyp

Should `muxl` appear in `ftyp.compatible_brands` as a signal that the file conforms to the MUXL canonical form? ISOBMFF allows arbitrary brand FourCCs and this is the standard mechanism for signaling profile compliance.

Pros:

- Tools and players can quickly check whether a file claims MUXL conformance without parsing the full structure
- Standard ISOBMFF practice — CMAF uses `cmfc`/`cmfs`, DASH uses `dash`, etc.
- Could eventually register with MP4RA if MUXL gains traction

Cons:

- Need to decide exactly what `muxl` brand promises about the file's structure
- A brand is a promise — need to be confident the spec is stable enough before stamping files with it

## Audio priming sample handling

Muxers disagree on how to handle Opus/AAC encoder delay (priming samples):

- **ffmpeg/mp4box**: keep the priming sample in mdat, use `elst` with `media_time=312` to skip past it during playback. 51 audio samples.
- **gstreamer**: drops the first audio sample from mdat entirely, uses a 2-entry elst with an empty edit (media_time=-1) for the gap. 50 audio samples.

The decoded audio is the same — they just disagree on whether priming data lives in the file.

Options:

1. **Normalize edit list representation only** (safe, doesn't touch mdat) — always use single-entry elst with media_time offset. Doesn't converge gstreamer and ffmpeg since actual sample data differs.
2. **Always strip priming samples** — detect encoder delay from edit list, drop those samples from mdat, adjust stsz/stts/stco, set media_time=0. Would converge all muxers but requires correctly interpreting every edit list pattern. Risk of double-trimming if upstream already trimmed but didn't update the edit list.
3. **Always keep priming samples** — can't reconstruct stripped data, so only works as a "don't strip" rule.

Leaning toward option 2 with good test coverage, but needs more investigation.

## Final Opus packet duration

ffmpeg and mp4box assign different durations to the last Opus audio sample in the stts table:

- **ffmpeg**: last sample delta = 328 (total audio duration = 48360 at 48kHz)
- **mp4box**: last sample delta = 312 (total audio duration = 48344 at 48kHz)

Same sample count (51), same sample bytes, same edit list. The only difference is 16 samples (0.33ms) on the final packet's stts delta.

The Opus spec says the decoder determines actual frame duration from the packet header, so the stts value is somewhat advisory for the last packet.

Options:

1. **Parse the Opus packet header** to determine the true frame duration and use that as the canonical stts delta. Most correct, but requires an Opus header parser.
2. **Derive from edit list** — compute expected total duration and adjust the last delta to match. Hacky, might not generalize.
3. **Accept the ambiguity** — treat this as a content-level decision that different muxers disagree on.

## Dynamic resolution changes (WebRTC/WHIP ingest)

Mobile devices sending via WebRTC (WHIP) can change resolution and orientation mid-stream (e.g., phone rotation, camera switch). This produces new H.264 SPS/PPS NAL units at keyframe boundaries.

In the MP4 container, this means multiple `stsd` sample entries (each `avc1` with its own `avcC` containing different SPS/PPS). The `stsc` table maps chunks to sample description indices.

Questions:

1. **Should we normalize SPS/PPS?** Some encoders include redundant parameters. Could canonicalize the binary SPS/PPS representation, but risk is high (any bit flip breaks decoding).
2. **Segment boundaries vs resolution changes** — in fMP4, should a resolution change force a new segment? Probably yes, since tfhd carries a single sample_description_index per fragment. This aligns naturally with keyframe boundaries.
3. **Orientation via tkhd matrix vs actual pixel dimensions** — some sources signal rotation via the track header matrix while keeping pixel dimensions constant. Others actually rotate the pixels. Need to decide how to canonicalize this distinction.

## Init segment evolution over long streams

For 24-hour livestreams, the init segment (ftyp+moov) is stable as long as the track configuration doesn't change. When codec parameters change (new SPS/PPS from resolution switch), a new init segment is needed.

Questions:

1. **Where does the new init appear in the archive fMP4?** Could emit a new ftyp+moov inline in the file at the point of change, but multi-moov fMP4 files are unusual. Alternatively, the S2PA manifest tracks init segment versions and the archive file just has one init at the start covering the initial config.
2. **How does the S2PA manifest reference init changes?** Could version the init metadata, with each segment referencing which init version it uses.

## Audio-only segmentation

Segment boundaries are currently driven by video keyframes. For audio-only streams (e.g., podcast ingest, audio-only WHIP), there are no keyframes to split on.

Options:

1. **Fixed duration** (e.g., 1 second): simple, predictable segment sizes. Need to pick a duration that aligns cleanly with common audio frame sizes (20ms Opus, ~21.3ms AAC).
2. **Fixed sample count**: e.g., 50 Opus packets per segment (= 1 second). Simpler alignment but duration varies if frame size changes.
3. **Codec-frame-aligned duration target**: pick a target duration (e.g., 1s) and round to the nearest codec frame boundary. Avoids splitting mid-frame (which we'd never do anyway since each sample is atomic).

Not urgent — current use case is always video+audio — but worth defining before audio-only ingest is supported.

## Content hashing details

When computing per-track content hashes for signing (by S2PA or any other system), the hash input is each track's moof+mdat bytes within a MUXL segment.

Questions:

1. **Hash boundary**: does the hash cover the full box bytes (headers included) or just payloads? Full box bytes is simpler and more robust.
2. **Hash algorithm**: BLAKE3 is the natural choice for content addressing (used elsewhere in DASL/AT Protocol ecosystem), but this is ultimately a decision for the signing layer, not MUXL.

## Export format (flat MP4)

MUXL currently only defines fMP4-based formats (segments and archives). A canonical flat MP4 format (ftyp+mdat+moov with complete sample tables) would provide maximum compatibility with players, editors, and media tools that don't support fMP4.

A flat MP4 could be deterministically generated from a MUXL archive by consolidating trun tables into moov sample tables and concatenating mdat payloads. The reverse (flat MP4 → MUXL segments via re-segmentation at keyframe boundaries) would enable signature verification from an export.

Not shipping in v1 — fMP4 is simpler to canonicalize and sufficient for streaming use cases. Revisit when export/download workflows need it.
