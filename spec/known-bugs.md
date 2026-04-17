# Known Bugs

## tkhd flags missing track_in_movie bit

**Status**: open
**Affects**: init segment output (both MUXL fMP4 and flat MP4)

The tkhd flags should be `0x03` (`track_enabled | track_in_movie`), but mp4-atom's `Tkhd` struct only exposes an `enabled` field (maps to `track_enabled`, bit 0). Setting `enabled: true` produces flags `0x01`. The `track_in_movie` bit (bit 1) is not settable through the public API — `encode_body_ext` returns `..Default::default()` which leaves it false.

Most encoders (ffmpeg, GStreamer, MP4Box) produce `0x03`. With `track_in_movie` unset, a strict ISOBMFF player could skip the track during normal playback.

### Fix options

1. **Upstream a PR to mp4-atom** adding a `track_in_movie` field to `Tkhd`. Cleanest fix.
2. **Fork mp4-atom** and add the field ourselves.
3. **Post-hoc byte patch**: after encoding the init segment, find the tkhd boxes and flip the bit. Fragile and ugly.
