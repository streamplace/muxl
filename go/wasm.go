package muxl

import (
	"bytes"
	"context"
	_ "embed"
	"fmt"
	"io"
	"log/slog"
	"sync"
	"sync/atomic"
	"testing/fstest"

	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/imports/wasi_snapshot_preview1"
)

// muxl.wasm bundles the full muxl-sign CLI compiled to wasm32-wasip1 — every
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
// muxl-sign WebAssembly module under the pure-Go wazero runtime. Construct one
// with [NewWASM] and reuse it for the life of the process — module compilation
// happens once; each call instantiates a fresh, isolated module.
type WASMEngine struct {
	runtime    wazero.Runtime
	compiled   wazero.CompiledModule
	counter    atomic.Uint64
	signers    sync.Map // instance name -> func([]byte) ([]byte, error)
	logger     *slog.Logger
	memInitial uint64
	memMax     uint64
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

// NewWASM compiles the embedded muxl-sign wasm module and returns an Engine
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
	// even when a given call never calls into host_sign / host_sha256.
	if _, err := e.runtime.NewHostModuleBuilder("muxl").
		NewFunctionBuilder().WithFunc(e.hostSign).Export("host_sign").
		NewFunctionBuilder().WithFunc(e.hostSha256).Export("host_sha256").
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
	return e.runWith(ctx, []string{"muxl-sign", "segment", "-", "--stdout"}, nil, false, input, nil, nil, nil, nil, events)
}

// UnwrapEvents implements [Engine].
func (e *WASMEngine) UnwrapEvents(ctx context.Context, input io.Reader, events chan<- *Event) error {
	return e.runWith(ctx, []string{"muxl-sign", "unwrap", "--events", "-"}, nil, false, input, nil, nil, nil, nil, events)
}

// ConcatEvents implements [Engine].
func (e *WASMEngine) ConcatEvents(ctx context.Context, input io.Reader, init, seg chan<- []byte, events chan<- *Event) error {
	return e.runWith(ctx, []string{"muxl-sign", "concat"}, nil, false, input, nil, nil, init, seg, events)
}

// Verify implements [Engine].
func (e *WASMEngine) Verify(ctx context.Context, input io.Reader) (string, error) {
	var out bytes.Buffer
	// Verification runs against the real clock so cert-validity-window checks
	// observe wall time.
	if err := e.runWith(ctx, []string{"muxl-sign", "verify"}, nil, true, input, &out, nil, nil, nil, nil); err != nil {
		return "", err
	}
	return out.String(), nil
}

// Wrap implements [Engine].
func (e *WASMEngine) Wrap(ctx context.Context, input io.Reader, format string, out io.Writer) error {
	if format == "" {
		format = "fmp4"
	}
	return e.runWith(ctx, []string{"muxl-sign", "wrap", "-", "-", "--format", format}, nil, false, input, out, nil, nil, nil, nil)
}

// WrapInit implements [Engine].
func (e *WASMEngine) WrapInit(ctx context.Context, input io.Reader, out io.Writer) error {
	return e.runWith(ctx, []string{"muxl-sign", "wrap", "-", "-", "--format", "fmp4", "--init-only"}, nil, false, input, out, nil, nil, nil, nil)
}

// SignSegment implements [Engine]. Signing needs the real wall clock (c2pa-rs
// checks cert validity and draws COSE nonces from real randomness), so it runs
// with realClock=true and is not byte-stable across runs.
func (e *WASMEngine) SignSegment(ctx context.Context, input io.Reader, in SignerInput, init, seg chan<- []byte, events chan<- *Event) error {
	if err := requireOneSigner(len(in.KeyPEM) > 0, in.Sign != nil); err != nil {
		return err
	}
	alg := in.Alg
	if alg == "" {
		alg = "es256k"
	}
	keysFS := fstest.MapFS{
		"cert.pem":     {Data: in.CertPEM},
		"track.json":   {Data: in.TrackManifest},
		"wrapper.json": {Data: in.WrapperManifest},
	}
	args := []string{
		"muxl-sign", "sign-segment",
		"--cert", "/keys/cert.pem",
		"--alg", alg,
		"--track-manifest", "/keys/track.json",
		"--wrapper-manifest", "/keys/wrapper.json",
	}
	if len(in.KeyPEM) > 0 {
		keysFS["key.pem"] = &fstest.MapFile{Data: in.KeyPEM}
		args = append(args, "--key", "/keys/key.pem")
	} else {
		args = append(args, "--host-sign")
	}
	fsCfg := wazero.NewFSConfig().WithFSMount(keysFS, "/keys")
	return e.runWith(ctx, args, fsCfg, true, input, nil, in.Sign, init, seg, events)
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
		"muxl-sign", "sign-transcode",
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
	if err := e.runWith(ctx, args, fsCfg, true, bytes.NewReader(in.Output), &out, in.Sign, nil, nil, nil); err != nil {
		return nil, err
	}
	return out.Bytes(), nil
}

func requireOneSigner(hasKey, hasSign bool) error {
	if hasKey == hasSign {
		return fmt.Errorf("muxl: exactly one of KeyPEM or Sign must be set")
	}
	return nil
}
