#!/usr/bin/env bash
#
# Generate synthetic MP4 test fixtures for muxl canonicalization testing.
# Requires ffmpeg with libx264, libsvtav1, libopus, and aac encoders.
#
set -euo pipefail

OUTDIR="${1:-samples/fixtures}"
mkdir -p "$OUTDIR"

# Shared source: 2 seconds of synthetic video + audio
# Using lavfi sources so no input file needed
VSRC="testsrc2=duration=2:size=320x240:rate=30"
ASRC="sine=frequency=440:duration=2:sample_rate=48000"

echo "Generating test fixtures in $OUTDIR..."

# 1. H.264 + AAC (the most common combo)
echo "  h264-aac.mp4"
ffmpeg -y -f lavfi -i "$VSRC" -f lavfi -i "$ASRC" \
  -c:v libx264 -preset ultrafast -crf 28 \
  -c:a aac -b:a 64k \
  -movflags +faststart \
  "$OUTDIR/h264-aac.mp4" 2>/dev/null

# 2. H.264 + Opus
echo "  h264-opus.mp4"
ffmpeg -y -f lavfi -i "$VSRC" -f lavfi -i "$ASRC" \
  -c:v libx264 -preset ultrafast -crf 28 \
  -c:a libopus -b:a 64k \
  "$OUTDIR/h264-opus.mp4" 2>/dev/null

# 3. AV1 + Opus
echo "  av1-opus.mp4"
ffmpeg -y -f lavfi -i "$VSRC" -f lavfi -i "$ASRC" \
  -c:v libsvtav1 -preset 8 -crf 45 \
  -c:a libopus -b:a 64k \
  "$OUTDIR/av1-opus.mp4" 2>/dev/null

# 4. AV1 + AAC
echo "  av1-aac.mp4"
ffmpeg -y -f lavfi -i "$VSRC" -f lavfi -i "$ASRC" \
  -c:v libsvtav1 -preset 8 -crf 45 \
  -c:a aac -b:a 64k \
  -movflags +faststart \
  "$OUTDIR/av1-aac.mp4" 2>/dev/null

# 5. H.264 + Opus, variable framerate (VFR) — simulates screen recording / WebRTC
echo "  h264-opus-vfr.mp4"
ffmpeg -y -f lavfi -i "testsrc2=duration=2:size=320x240:rate=60" -f lavfi -i "$ASRC" \
  -vf "setpts='if(mod(N,3),PTS,PTS+0.01/TB)'" \
  -c:v libx264 -preset ultrafast -crf 28 -vsync vfr \
  -c:a libopus -b:a 64k \
  "$OUTDIR/h264-opus-vfr.mp4" 2>/dev/null

# 6. H.264 + AAC, different timescale (gstreamer-style: timescale=6000 for ~30fps)
# We can't control ffmpeg's timescale directly, but we can remux with a different one via MP4Box if available.
# For now, create a 25fps variant which naturally uses timescale=12800
echo "  h264-aac-25fps.mp4"
ffmpeg -y -f lavfi -i "testsrc2=duration=2:size=320x240:rate=25" -f lavfi -i "$ASRC" \
  -c:v libx264 -preset ultrafast -crf 28 \
  -c:a aac -b:a 64k \
  "$OUTDIR/h264-aac-25fps.mp4" 2>/dev/null

# 7. H.264 + AAC, portrait orientation (rotated via metadata — simulates phone)
echo "  h264-aac-portrait.mp4"
ffmpeg -y -f lavfi -i "testsrc2=duration=2:size=240x320:rate=30" -f lavfi -i "$ASRC" \
  -c:v libx264 -preset ultrafast -crf 28 \
  -c:a aac -b:a 64k \
  -metadata:s:v rotate=90 \
  "$OUTDIR/h264-aac-portrait.mp4" 2>/dev/null

# 8. Video only (no audio track)
echo "  h264-video-only.mp4"
ffmpeg -y -f lavfi -i "$VSRC" \
  -c:v libx264 -preset ultrafast -crf 28 \
  "$OUTDIR/h264-video-only.mp4" 2>/dev/null

# 9. Audio only (no video track)
echo "  opus-audio-only.mp4"
ffmpeg -y -f lavfi -i "$ASRC" \
  -c:a libopus -b:a 64k \
  "$OUTDIR/opus-audio-only.mp4" 2>/dev/null

# 10. Fragmented MP4 (fMP4) — simulates livestream ingest
echo "  h264-opus-frag.mp4"
ffmpeg -y -f lavfi -i "$VSRC" -f lavfi -i "$ASRC" \
  -c:v libx264 -preset ultrafast -crf 28 -g 30 \
  -c:a libopus -b:a 64k \
  -movflags +frag_keyframe+empty_moov+default_base_moof \
  "$OUTDIR/h264-opus-frag.mp4" 2>/dev/null

# Two AAC alternatives with ffmpeg's raw trak/udta/name title metadata.
echo "  aac-two-track-labeled-frag.mp4"
ffmpeg -y -f lavfi -i "$ASRC" \
  -f lavfi -i "sine=frequency=880:duration=2:sample_rate=48000" \
  -map 0:a -map 1:a -t 2 -c:a aac \
  -metadata:s:a:1 title=Music \
  -movflags +frag_keyframe+empty_moov \
  "$OUTDIR/aac-two-track-labeled-frag.mp4" 2>/dev/null

echo "Done. Generated $(ls "$OUTDIR"/*.mp4 | wc -l) fixtures in $OUTDIR"
