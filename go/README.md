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

### Audio labels and alternative groups

Audio catalogs expose `AudioConfig.AlternateGroup` and `AudioConfig.Label`.
By default source MP4 groups/titles are ignored, preserving canonical bytes
across muxers. Opt in with `WithSegmentSourceTrackMeta()` for `SegmentEvents`,
or `SignerInput{SourceTrackMeta: true}` for `SignSegment`.

To set metadata explicitly, use `WithSegmentTrackMeta` or `WithTrackMeta`
(`Canonicalize`):

```go
meta := map[uint32]muxl.TrackMeta{
    2: {AlternateGroup: 1, Label: "Full mix"},
    3: {AlternateGroup: 2, Label: "Music"},
}
err = eng.SegmentEvents(ctx, fmp4Reader, events, muxl.WithSegmentTrackMeta(meta))
canonical, err := eng.Canonicalize(ctx, mp4Bytes, muxl.WithTrackMeta(meta))
```

IDs refer to output audio tracks after any track remapping. Explicit entries
replace source metadata; zero group/empty label clears the corresponding field.
Unknown/non-audio IDs and groups that do not fit `tkhd`'s 16-bit field are errors.
Catalog-derived groups and `trak/udta/name` labels survive wrap, unwrap and flat
header synthesis. Reading the synthesized init back with
`WithSegmentSourceTrackMeta()` recovers them.

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
