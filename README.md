# MUXL

Deterministic MP4 canonicalization. Like [DRISL](https://dasl.ing/drisl.html), but for video.

MUXL produces byte-identical MP4 output from the same logical content, enabling stable content-addressed identifiers (CIDs) for video. Given the same encoded frames, any MUXL implementation will always produce the same bytes.

Part of the [DASL](https://dasl.ing) ecosystem.

## How it works

Video encoders produce identical encoded frames, but different muxers (ffmpeg, GStreamer, MP4Box) wrap them in different container structures — different field orderings, timestamps, metadata. MUXL defines a single canonical container form and converts any fMP4 input to it.

```
fMP4 stream ──► MUXL ──► init segment + MUXL segments
                              │              │
                         ftyp+moov     per-GoP moof+mdat
                        (canonical)      (canonical)
```

The same source frames always produce the same segment bytes, regardless of which muxer originally packaged them. This means you can compute a CID over a segment and it will be stable — the same content always gets the same hash.

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

# Build a single archive file (init + all segments)
muxl segment input.fmp4 --archive output.mp4

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

## Status

Early development. The canonical form spec and implementation are functional but provisional — expect changes after broader real-world playback testing. See the [open questions](spec/open-questions.md).

## License

Apache-2.0
