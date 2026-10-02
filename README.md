# MUXL

Deterministic MP4 canonicalization. Like [DRISL](https://dasl.ing/drisl.html), but for video.

MUXL produces byte-identical MP4 output from the same logical content, enabling stable content-addressed identifiers (CIDs) for video. Given the same encoded frames, any MUXL implementation will always produce the same bytes.

Part of the [DASL](https://dasl.ing) ecosystem.

## This Repo

This repo contains:

1. The Rust implementation of MUXL, and tools for compiling to WASM. (`src`)
2. An example of how to embed MUXL's WASM into a Go library. (`examples/go-wasi`)
3. An example TypeScript worker, providing tooling for working with MUXL HLS playlists.

## How it works

Video encoders produce identical encoded frames, but different muxers (ffmpeg, GStreamer, MP4Box) wrap them in different container structures — different field orderings, timestamps, metadata. MUXL defines a single canonical container form and converts any fMP4 input to it.

```
fMP4 stream ──► MUXL ──► init segment + MUXL segments
                              │              │
                         ftyp+moov     per-GoP moof+mdat
                        (canonical)      (canonical)
```

The same source frames always produce the same segment bytes, regardless of which muxer originally packaged them. This means you can compute a CID over a segment and it will be stable — the same content always gets the same hash.

### Tracks

MUXL handles video (H.264, AV1), audio (AAC, Opus), and WebVTT timed text (ISO/IEC 14496-30 `wvtt`) tracks. Every track gets its own canonical segment per GoP, so captions are hashed, signed, verified, and stored exactly like the media tracks, and can be dropped or replaced independently.

Text tracks appear in the catalog under a `text` group (`codec`, `language`, optional `label`, and the WebVTT header as `config`, on a 1000 Hz timescale). Their samples are canonicalized so that the same captions always produce the same bytes. Cues active at the same time are sorted, overlapping cues are split into disjoint samples, gaps are explicit empty-cue (`vtte`) samples, and each GoP's text segment is clipped and padded to cover exactly that GoP. Flat MP4s carry the text track as a normal `text`-handler track. The native HLS emitter retains text metadata but emits only audio/video playlists; applications can serve plain WebVTT subtitles separately. See [`spec/canonical-form.md` § Timed Text (WebVTT)](spec/canonical-form.md#timed-text-webvtt).

## Install

```bash
cargo install --git https://github.com/streamplace/s2pa-muxl
```

Or build from source:

```bash
git clone https://github.com/streamplace/s2pa-muxl
cd s2pa-muxl
cargo build --release
```

## CLI

```bash
# Extract track metadata from an MP4
muxl catalog input.mp4

# Build a canonical init segment
muxl init input.mp4 init.mp4

# Segment an fMP4 into a directory of .m4s files
muxl segment input.fmp4 --dir output/

# Build a single MUXL fMP4 file (init + all segments)
muxl segment input.fmp4 --fmp4 output.mp4

# Stream CBOR events to stdout (for piping to other programs)
muxl segment input.fmp4 --stdout

# Read from stdin
cat input.fmp4 | muxl segment - --stdout
```

## Library

muxl is both a CLI and a Rust library.

```rust
use muxl::{Segmenter, SegmenterEvent};

// Push-based: feed fMP4 chunks, get segments back
let mut segmenter = Segmenter::new();

for chunk in fmp4_stream {
    for event in segmenter.feed(&chunk)? {
        match event {
            SegmenterEvent::InitSegment { catalog, data } => {
                // Canonical ftyp+moov init segment
            }
            SegmenterEvent::Segment(seg) => {
                // One GOP of canonical moof+mdat pairs
            }
        }
    }
}

// Flush remaining data at end of stream
for event in segmenter.flush()? {
    // handle final segment
}
```

There's also a pull-based API for when you have the complete input:

```rust
use muxl::{segment_fmp4, build_init_segment};

let catalog = segment_fmp4(&mut reader, |segment| {
    println!("segment: {} bytes", segment.data.len());
    Ok(())
})?;

let init = build_init_segment(&catalog)?;
```

## WebAssembly

muxl compiles to both WASM targets with zero platform-specific code.

### Browser (wasm-bindgen)

```bash
cargo build --target wasm32-unknown-unknown --lib --features wasm
```

```javascript
import { WasmSegmenter } from "./muxl.js";

const segmenter = new WasmSegmenter();
const response = await fetch("stream.mp4");
const reader = response.body.getReader();

while (true) {
  const { done, value } = await reader.read();
  if (done) break;
  const events = segmenter.feed(value);
  for (const event of events) {
    if (event.type === "init") {
      // event.data is a Uint8Array with the canonical init segment
    } else if (event.type === "segment") {
      // event.data is a Uint8Array with the segment bytes
    }
  }
}
const finalEvents = segmenter.flush();
```

### Go / WASI

The CLI compiles to WASI and runs in any WASI runtime. For Go, use [wazero](https://wazero.io):

```bash
cargo build --target wasm32-wasip1 --release
# Output: target/wasm32-wasip1/release/muxl.wasm (1.4 MB)
```

Pipe fMP4 through stdin, read CBOR events from stdout:

```go
stdinReader, stdinWriter := io.Pipe()
stdoutReader, stdoutWriter := io.Pipe()

config := wazero.NewModuleConfig().
    WithStdin(stdinReader).
    WithStdout(stdoutWriter).
    WithArgs("muxl", "segment", "-", "--stdout")

// Feed fMP4 data to stdinWriter, decode CBOR events from stdoutReader
decoder := drisl.NewDecoder(stdoutReader)
var event MuxlEvent
decoder.Decode(&event) // {"type": "init", "data": <bytes>}
```

See [`examples/go-wasi/`](examples/go-wasi/) for a complete working example.

The embedded Go engine's streaming calls terminate on context cancellation even
when the caller has stopped consuming its event channel. Cancelling one call
does not close the engine or affect other operations.

The embedded Go module exposes WebVTT through `TextEngine` (implemented by
`WASMEngine`, without changing the existing `Engine` interface):

```go
tracks, err := engine.TextTracks(ctx, vodReader)
cues, err := engine.ReadTextCues(ctx, vodReader, tracks[0].TrackID)
segment, err := engine.AddTextTrack(ctx, unsignedGoP, muxl.TextTrack{
    TrackID: 3, Language: "en-US", Label: "English",
}, []muxl.TextCue{
    {Start: 100, End: 400, Text: "Hello"},
    {Start: 600, End: 900, Text: "World", ID: "cue-2"},
})
```

Cue timestamps are absolute stream milliseconds (exclusive end). Adding text
preserves existing unsigned AV track bytes; it rejects signed input and track-id
collisions. Wrap the result as fMP4 before `SignSegment`. Enumeration and cue
reading accept canonical segments, concatenated VOD segments, and MP4 files.
Adjacent identical cue pieces merge on read; distinct adjacent cues should have
distinct IDs. Text fragments restart sequence numbers at 1 for each GoP so their
CID depends only on the GoP span, cue content, and track configuration.
The small `samples/text-track.mp4` fixture contains video, audio, and two English
WebVTT cues, including empty timeline intervals.

For signed archive copies, `TextEngine.SignTextRuns(ctx, req, tracks, in)`
mints and streamer-signs only standalone WebVTT runs for the recorded live
`TextRequest`. It never receives or re-signs AV bytes and does not call
`in.TextFn`. The archive caller replaces existing text runs, checks for
non-text track-ID collisions, and concatenates all runs in ascending numeric
track-ID order. Non-text runs, including separately signed transcodes, stay
byte-identical. See [the Go API](go/README.md#signed-archive-text-runs) for the
span, manifest, timestamp, and signer rules and the `sign-text-runs` CLI format.

To keep normal pre-commit WASM rebuilding inside a builder container, install
the repo hooks with `just install-hooks` and set `MUXL_BUILD_CONTAINER` to its
name when committing.


## Status

Early development. The canonical form spec and implementation are functional but provisional — expect changes after broader real-world playback testing. See the [open questions](spec/open-questions.md).

## License

Apache-2.0
