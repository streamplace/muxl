package muxl

import (
	"bytes"
	"context"
	_ "embed"
	"encoding/hex"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"sync/atomic"
	"testing/fstest"

	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/imports/wasi_snapshot_preview1"
)

// muxl.wasm bundles the full muxl CLI compiled to wasm32-wasip1 — every
// subcommand (segment, concat, unwrap, wrap, verify, sign-segment,
// sign-transcode, …). It is built by `just build-go-wasm` and committed to the
// repo so consumers need no Rust toolchain. See the package doc.
//
//go:generate just build-go-wasm
//go:embed muxl.wasm
var wasmBytes []byte

// Default wasm linear-memory tuning; override with [WithMemory]. wazero's
// default allocator reallocs+memcpys on every memory.grow page, so the custom
// allocator pre-allocates this much and grows geometrically to the max.
const (
	defaultMemoryInitial uint64 = 50 * 1024 * 1024
	defaultMemoryMax     uint64 = 1024 * 1024 * 1024
)

// WASMEngine is the default zero-config [Engine]: it runs the embedded
// muxl WebAssembly module under the pure-Go wazero runtime. Construct one
// with [NewWASM] and reuse it for the life of the process — module compilation
// happens once; each call instantiates a fresh, isolated module.
type WASMEngine struct {
	runtime          wazero.Runtime
	compiled         wazero.CompiledModule
	counter          atomic.Uint64
	signers          sync.Map // instance name -> func([]byte) ([]byte, error)
	manifestFetchers sync.Map // instance name -> func(kind uint32) ([]byte, error)
	logger           *slog.Logger
	memInitial       uint64
	memMax           uint64
}

var _ Engine = (*WASMEngine)(nil)

// Option configures a [WASMEngine].
type Option func(*WASMEngine)

// WithMemory sets the wasm linear-memory tuning: initial backing-buffer
// capacity and a hard ceiling (a module that grows past max fails its
// memory.grow). Defaults are 50 MiB / 1 GiB.
func WithMemory(initial, max uint64) Option {
	return func(e *WASMEngine) { e.memInitial, e.memMax = initial, max }
}

// WithLogger sets the slog.Logger used for the engine's internal diagnostics
// and for relaying the wasm module's stderr (errors at error level, the rest
// at debug). Defaults to slog.Default().
func WithLogger(l *slog.Logger) Option {
	return func(e *WASMEngine) { e.logger = l }
}

// NewWASM compiles the embedded muxl wasm module and returns an Engine
// backed by wazero. Compilation is the expensive step, so create one engine
// and share it. Call [WASMEngine.Close] to release runtime resources.
func NewWASM(ctx context.Context, opts ...Option) (*WASMEngine, error) {
	e := &WASMEngine{
		logger:     slog.Default(),
		memInitial: defaultMemoryInitial,
		memMax:     defaultMemoryMax,
	}
	for _, o := range opts {
		o(e)
	}

	e.runtime = wazero.NewRuntime(ctx)
	wasi_snapshot_preview1.MustInstantiate(ctx, e.runtime)

	// Register the "muxl" host module the wasm imports. The imports are
	// declared unconditionally in the binary, so the module must always exist
	// even when a given call never calls into host_sign.
	if _, err := e.runtime.NewHostModuleBuilder("muxl").
		NewFunctionBuilder().WithFunc(e.hostSign).Export("host_sign").
		NewFunctionBuilder().WithFunc(e.hostGetManifest).Export("host_get_manifest").
		Instantiate(ctx); err != nil {
		_ = e.runtime.Close(ctx)
		return nil, fmt.Errorf("muxl: registering host module: %w", err)
	}

	compiled, err := e.runtime.CompileModule(ctx, wasmBytes)
	if err != nil {
		_ = e.runtime.Close(ctx)
		return nil, fmt.Errorf("muxl: compiling wasm module: %w", err)
	}
	e.compiled = compiled
	return e, nil
}

// Close releases the engine's wasm runtime. The engine must not be used after.
func (e *WASMEngine) Close(ctx context.Context) error {
	return e.runtime.Close(ctx)
}

// SegmentEvents implements [Engine].
func (e *WASMEngine) SegmentEvents(ctx context.Context, input io.Reader, events chan<- *Event) error {
	return e.runWith(ctx, []string{"muxl", "segment", "-", "--stdout"}, nil, false, input, nil, nil, nil, nil, nil, events)
}

// UnwrapEvents implements [Engine].
func (e *WASMEngine) UnwrapEvents(ctx context.Context, input io.Reader, events chan<- *Event) error {
	return e.runWith(ctx, []string{"muxl", "unwrap", "--events", "-"}, nil, false, input, nil, nil, nil, nil, nil, events)
}

// ConcatEvents implements [Engine].
func (e *WASMEngine) ConcatEvents(ctx context.Context, input io.Reader, init, seg chan<- []byte, events chan<- *Event) error {
	return e.runWith(ctx, []string{"muxl", "concat"}, nil, false, input, nil, nil, nil, init, seg, events)
}

// Verify implements [Engine].
func (e *WASMEngine) Verify(ctx context.Context, input io.Reader) (string, error) {
	var out bytes.Buffer
	// Verification runs against the real clock so cert-validity-window checks
	// observe wall time.
	if err := e.runWith(ctx, []string{"muxl", "verify"}, nil, true, input, &out, nil, nil, nil, nil, nil); err != nil {
		return "", err
	}
	return out.String(), nil
}

// Wrap implements [Engine].
func (e *WASMEngine) Wrap(ctx context.Context, input io.Reader, format string, out io.Writer) error {
	if format == "" {
		format = "fmp4"
	}
	return e.runWith(ctx, []string{"muxl", "wrap", "-", "-", "--format", format}, nil, false, input, out, nil, nil, nil, nil, nil)
}

// WrapInit implements [Engine].
func (e *WASMEngine) WrapInit(ctx context.Context, input io.Reader, out io.Writer) error {
	return e.runWith(ctx, []string{"muxl", "wrap", "-", "-", "--format", "fmp4", "--init-only"}, nil, false, input, out, nil, nil, nil, nil, nil)
}

// SignSegment implements [Engine]. Signing needs the real wall clock (c2pa-rs
// checks cert validity and draws COSE nonces from real randomness), so it runs
// with realClock=true and is not byte-stable across runs.
//
// Manifest source: static (TrackManifest/WrapperManifest, mounted at /keys)
// when neither callback is set; host-fetched per GoP (via the
// muxl.host_get_manifest import) when either of TrackManifestFn/WrapperManifestFn
// is set. The dynamic path is what lets a long-running streamer pick up
// mid-stream manifest changes (e.g. a livestream record going from pre-live
// to live) without restarting the wasm call.
func (e *WASMEngine) SignSegment(ctx context.Context, input io.Reader, in SignerInput, init, seg chan<- []byte, events chan<- *Event) error {
	if err := requireOneSigner(len(in.KeyPEM) > 0, in.Sign != nil); err != nil {
		return err
	}
	alg := in.Alg
	if alg == "" {
		alg = "es256k"
	}
	dynamicManifest := in.TrackManifestFn != nil || in.WrapperManifestFn != nil
	keysFS := fstest.MapFS{
		"cert.pem": {Data: in.CertPEM},
	}
	args := []string{
		"muxl", "sign-segment",
		"--cert", "/keys/cert.pem",
		"--alg", alg,
	}
	if dynamicManifest {
		args = append(args, "--host-manifest")
	} else {
		keysFS["track.json"] = &fstest.MapFile{Data: in.TrackManifest}
		keysFS["wrapper.json"] = &fstest.MapFile{Data: in.WrapperManifest}
		args = append(args,
			"--track-manifest", "/keys/track.json",
			"--wrapper-manifest", "/keys/wrapper.json",
		)
	}
	if len(in.KeyPEM) > 0 {
		keysFS["key.pem"] = &fstest.MapFile{Data: in.KeyPEM}
		args = append(args, "--key", "/keys/key.pem")
	} else {
		args = append(args, "--host-sign")
	}
	fsCfg := wazero.NewFSConfig().WithFSMount(keysFS, "/keys")
	manifestFn := manifestFetcher(in)
	return e.runWith(ctx, args, fsCfg, true, input, nil, in.Sign, manifestFn, init, seg, events)
}

// manifestFetcher returns the per-instance fetcher we register for the
// muxl.host_get_manifest import. nil means "no dynamic manifests for this
// call" — the static keys-FS mount is used instead. A missing callback for one
// kind silently falls back to the matching static field, so callers can supply
// just TrackManifestFn (the common case in Streamplace) without rewriting the
// wrapper too.
func manifestFetcher(in SignerInput) func(kind uint32) ([]byte, error) {
	if in.TrackManifestFn == nil && in.WrapperManifestFn == nil {
		return nil
	}
	return func(kind uint32) ([]byte, error) {
		switch kind {
		case 0:
			if in.TrackManifestFn != nil {
				return in.TrackManifestFn()
			}
			return in.TrackManifest, nil
		case 1:
			if in.WrapperManifestFn != nil {
				return in.WrapperManifestFn()
			}
			return in.WrapperManifest, nil
		default:
			return nil, fmt.Errorf("muxl: unknown manifest kind %d", kind)
		}
	}
}

// SignTranscode implements [Engine]. The output to sign is piped via stdin;
// the source ingredient, cert, (optional) key, and manifest are mounted
// read-only at /keys; the signed segment comes back on stdout.
func (e *WASMEngine) SignTranscode(ctx context.Context, in TranscodeInput) ([]byte, error) {
	if err := requireOneSigner(len(in.KeyPEM) > 0, in.Sign != nil); err != nil {
		return nil, err
	}
	alg := in.Alg
	if alg == "" {
		alg = "es256k"
	}
	keysFS := fstest.MapFS{
		"cert.pem":      {Data: in.CertPEM},
		"source.m4s":    {Data: in.Source},
		"manifest.json": {Data: in.Manifest},
	}
	args := []string{
		"muxl", "sign-transcode",
		"--cert", "/keys/cert.pem",
		"--alg", alg,
		"--source", "/keys/source.m4s",
		"--manifest", "/keys/manifest.json",
	}
	if len(in.KeyPEM) > 0 {
		keysFS["key.pem"] = &fstest.MapFile{Data: in.KeyPEM}
		args = append(args, "--key", "/keys/key.pem")
	} else {
		args = append(args, "--host-sign")
	}
	fsCfg := wazero.NewFSConfig().WithFSMount(keysFS, "/keys")
	var out bytes.Buffer
	if err := e.runWith(ctx, args, fsCfg, true, bytes.NewReader(in.Output), &out, in.Sign, nil, nil, nil, nil); err != nil {
		return nil, err
	}
	return out.Bytes(), nil
}

// GenCert implements [Engine]. The pubkey is passed as a hex arg; the
// TBSCertificate is signed via the host callback (in.Sign); the PEM cert comes
// back on stdout. Runs against the real clock (the cert carries a notBefore).
func (e *WASMEngine) GenCert(ctx context.Context, in CertInput) ([]byte, error) {
	if in.Sign == nil {
		return nil, fmt.Errorf("muxl: GenCert requires a Sign callback")
	}
	if len(in.PubKey) != 65 || in.PubKey[0] != 0x04 {
		return nil, fmt.Errorf("muxl: GenCert requires a 65-byte uncompressed secp256k1 public key (0x04||X||Y), got %d bytes", len(in.PubKey))
	}
	args := []string{
		"muxl", "gen-cert",
		"--pubkey", hex.EncodeToString(in.PubKey),
		"--host-sign",
		"--out", "-",
	}
	if in.DID != "" {
		args = append(args, "--did", in.DID)
	}
	if in.Org != "" {
		args = append(args, "--organization", in.Org)
	}
	var out bytes.Buffer
	if err := e.runWith(ctx, args, nil, true, nil, &out, in.Sign, nil, nil, nil, nil); err != nil {
		return nil, err
	}
	return out.Bytes(), nil
}

// Canonicalize converts a flat or fragmented MP4 into a canonical MUXL fMP4
// (ftyp+moov + canonical moof/mdat fragments). Unlike the streaming
// [WASMEngine.SegmentEvents], it accepts a faststart (flat) MP4: canonical-form
// derivation needs random access, so the bytes are staged through a temp file
// mounted read-write into the sandbox rather than piped on stdin. This turns an
// arbitrary transcoder output into MUXL that SegmentEvents/SignTranscode accept.
func (e *WASMEngine) Canonicalize(ctx context.Context, mp4 []byte, opts ...CanonicalizeOption) ([]byte, error) {
	var cfg canonicalizeConfig
	for _, opt := range opts {
		opt(&cfg)
	}

	dir, err := os.MkdirTemp("", "muxl-canon-")
	if err != nil {
		return nil, fmt.Errorf("muxl: temp dir: %w", err)
	}
	defer os.RemoveAll(dir)

	if err := os.WriteFile(filepath.Join(dir, "in.mp4"), mp4, 0o600); err != nil {
		return nil, fmt.Errorf("muxl: staging canonicalize input: %w", err)
	}
	fsCfg := wazero.NewFSConfig().WithDirMount(dir, "/work")
	args := []string{"muxl", "fmp4", "/work/in.mp4", "/work/out.fmp4"}
	// Track remaps are deterministic given the map; sort by source id so the
	// CLI args (and any logging) are stable.
	for _, src := range sortedU32Keys(cfg.trackRemap) {
		args = append(args, "--remap-track", fmt.Sprintf("%d:%d", src, cfg.trackRemap[src]))
	}
	if err := e.runWith(ctx, args, fsCfg, false, nil, nil, nil, nil, nil, nil, nil); err != nil {
		return nil, err
	}
	out, err := os.ReadFile(filepath.Join(dir, "out.fmp4"))
	if err != nil {
		return nil, fmt.Errorf("muxl: reading canonicalize output: %w", err)
	}
	return out, nil
}

func requireOneSigner(hasKey, hasSign bool) error {
	if hasKey == hasSign {
		return fmt.Errorf("muxl: exactly one of KeyPEM or Sign must be set")
	}
	return nil
}

// sortedU32Keys returns m's keys in ascending order (nil map → nil slice), so
// derived command-line args are deterministic.
func sortedU32Keys(m map[uint32]uint32) []uint32 {
	keys := make([]uint32, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool { return keys[i] < keys[j] })
	return keys
}
