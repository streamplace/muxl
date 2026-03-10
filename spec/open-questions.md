# Open Questions

Issues that need further investigation before the canonical form is finalized.

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
3. **Does the flat MP4 export need to handle multi-init?** In flat MP4, multiple stsd entries in a single moov handle this naturally. The question is whether the init→flat→re-segment round trip is lossless when init changes mid-stream.

## Content hashing details

When computing per-track content hashes for signing (by S2PA or any other system), the hash input is each track's moof+mdat bytes within a MUXL segment.

Questions:
1. **Hash boundary**: does the hash cover the full box bytes (headers included) or just payloads? Full box bytes is simpler and more robust.
2. **Hash algorithm**: BLAKE3 is the natural choice for content addressing (used elsewhere in DASL/AT Protocol ecosystem), but this is ultimately a decision for the signing layer, not MUXL.
