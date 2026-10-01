# muxl (Go)

Go bindings for [MUXL](https://dasl.ing/muxl.html) — deterministic,
content-addressable MP4 — and its [S2PA](https://dasl.ing/s2pa.html) signing and
verification.

```go
import muxl "github.com/streamplace/muxl/go"
```

The default engine runs the `muxl` toolchain compiled to WebAssembly under
the pure-Go [wazero](https://wazero.io) runtime. **No Rust toolchain, no cgo —
just `go get`.** The `muxl.wasm` artifact is embedded in the package and
committed to the repo.

## Usage

```go
ctx := context.Background()
eng, err := muxl.NewWASM(ctx)
if err != nil { panic(err) }
defer eng.Close(ctx)

// Segment + S2PA-sign an fMP4 stream, collecting per-GoP events.
events := make(chan *muxl.Event, 16)
go func() {
    defer close(events)
    err = eng.SignSegment(ctx, fmp4Reader, muxl.SignerInput{
        CertPEM:         certPEM,           // S2PA leaf cert chain (PEM)
        KeyPEM:          keyPEM,            // or use Sign for a host/keystore signer
        TrackManifest:   manifestJSON,
        WrapperManifest: manifestJSON,
    }, nil, nil, events)
}()
for ev := range events { /* ev.Tracks holds the signed canonical segments */ }

// Verify a stored signed wrapper (bare .m4s, fMP4, or flat MP4).
report, err := eng.Verify(ctx, signedReader) // per-segment manifest+cert JSON
```

All operations live behind the [`Engine`](muxl.go) interface, so the WASM
backend can be swapped for a natively-linked Rust build later without touching
callers.

### Streaming text tracks

`SignerInput.TextFn` is an optional per-GoP callback. It receives `TextRequest`
with the reference AV GoP's absolute stream-millisecond interval `[StartMs,
EndMs)`. Return `TextAttachment` containing `TextTrackAttachment` entries: an
immutable `TextTrack` configuration (`TrackID`, BCP 47 `Language`, `Label`) and
its overlapping `TextCue`s. Reserve a text ID namespace distinct from input AV
and downstream renditions; do not reuse another track's ID.

The signer encodes WebVTT and gap-covering segments before signing, without
changing AV bytes or playable duration. Once declared, a text track appears in
every subsequent GoP, even when omitted from the callback result or when the
callback fails. Language/label changes require a new track ID. Callbacks may
block to await source captions; callers should decouple their media producer.
A nil callback preserves the pre-text signed bytes. Cue IDs should identify the
session and remain stable across GoP boundaries so readers can coalesce them.

`SignerInput.SegmentTimeFn` optionally maps each absolute media start in
milliseconds to its signed UTC start time. Nil retains the existing signing-time
stamp. It is independent of text and can be used for AV-only origins as well.

### Transcode provenance

`SignTranscode` signs a transcoded output segment so its C2PA manifest names the
segment it was transcoded from as a `parentOf` ingredient, via a
`c2pa.transcoded` action — the provenance step a Livepeer orchestrator runs
after transcoding a signed MUXL segment. For host/keystore-held keys (e.g. an
orchestrator's Ethereum keystore, which signs raw secp256k1 digests), wrap the
signer with [`RawSignerToCallback`](sign.go) and pass it as `Sign`.

## Rebuilding the embedded wasm

Only needed when the Rust changes — consumers never do this:

```sh
just build-go-wasm   # builds muxl -> wasm32-wasip1, copies to go/muxl.wasm
```

then commit `go/muxl.wasm`. Requires `clang` on PATH (see the recipe for the
containerized alternative).
