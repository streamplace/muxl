package muxl

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"sort"
	"strings"

	"github.com/hyphacoop/go-dasl/drisl"
	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/api"
	"github.com/tetratelabs/wazero/experimental"
)

// hostSignErr is the sentinel u32 the wasm's host_sign import expects on
// failure — anything other than a real signature length.
const hostSignErr = ^uint32(0)

// hostSha256 backs the wasm's muxl.host_sha256 import: read input from wasm
// memory, hash with native (hardware-accelerated) SHA-256, write the digest
// back. Available to every instance; only exercised by the bench path today.
func (e *WASMEngine) hostSha256(ctx context.Context, mod api.Module, dataPtr, dataLen, outPtr uint32) {
	data, ok := mod.Memory().Read(dataPtr, dataLen)
	if !ok {
		e.logger.ErrorContext(ctx, "muxl host_sha256: bad data pointer", "instance", mod.Name())
		return
	}
	sum := sha256.Sum256(data)
	if !mod.Memory().Write(outPtr, sum[:]) {
		e.logger.ErrorContext(ctx, "muxl host_sha256: bad output pointer", "instance", mod.Name())
	}
}

// hostSign backs the wasm's muxl.host_sign import: look up the per-instance
// closure registered by the calling method (keyed by the unique instance
// name), hand it the bytes to sign, and write the signature back into wasm
// memory. Returns the signature length, or hostSignErr on any failure.
func (e *WASMEngine) hostSign(ctx context.Context, mod api.Module, dataPtr, dataLen, outPtr, outMax uint32) uint32 {
	v, ok := e.signers.Load(mod.Name())
	if !ok {
		e.logger.ErrorContext(ctx, "muxl host_sign: no signer registered", "instance", mod.Name())
		return hostSignErr
	}
	signFn := v.(func([]byte) ([]byte, error))

	data, ok := mod.Memory().Read(dataPtr, dataLen)
	if !ok {
		e.logger.ErrorContext(ctx, "muxl host_sign: bad data pointer", "instance", mod.Name())
		return hostSignErr
	}
	sig, err := signFn(data)
	if err != nil {
		e.logger.ErrorContext(ctx, "muxl host_sign: signer returned error", "instance", mod.Name(), "error", err)
		return hostSignErr
	}
	if uint32(len(sig)) > outMax {
		e.logger.ErrorContext(ctx, "muxl host_sign: signature too long for buffer", "instance", mod.Name(), "len", len(sig), "max", outMax)
		return hostSignErr
	}
	if !mod.Memory().Write(outPtr, sig) {
		e.logger.ErrorContext(ctx, "muxl host_sign: bad output pointer", "instance", mod.Name())
		return hostSignErr
	}
	return uint32(len(sig))
}

// muxlAllocator pre-allocates the wasm linear-memory backing buffer and grows
// it geometrically, avoiding wazero's default realloc+memcpy on every page so
// a segment that ends up at tens of MB doesn't allocate gigabytes on the way.
type muxlAllocator struct {
	logger       *slog.Logger
	instanceName string
	initialBytes uint64
	maxBytes     uint64
}

func (a *muxlAllocator) Allocate(capHint, wasmMax uint64) experimental.LinearMemory {
	effectiveMax := a.maxBytes
	if wasmMax > 0 && wasmMax < effectiveMax {
		effectiveMax = wasmMax
	}
	initial := a.initialBytes
	if initial < capHint {
		initial = capHint
	}
	if initial > effectiveMax {
		initial = effectiveMax
	}
	return &muxlLinearMemory{
		logger:       a.logger,
		instanceName: a.instanceName,
		buf:          make([]byte, 0, initial),
		max:          effectiveMax,
	}
}

type muxlLinearMemory struct {
	logger       *slog.Logger
	instanceName string
	buf          []byte
	max          uint64
}

func (m *muxlLinearMemory) Reallocate(size uint64) []byte {
	if size > m.max {
		m.logger.Error("muxl memory cap exceeded", "instance", m.instanceName, "requested_bytes", size, "max_bytes", m.max)
		return nil
	}
	if size <= uint64(cap(m.buf)) {
		m.buf = m.buf[:size]
		return m.buf
	}
	newCap := uint64(cap(m.buf)) * 2
	if newCap < size {
		newCap = size
	}
	if newCap > m.max {
		newCap = m.max
	}
	newBuf := make([]byte, size, newCap)
	copy(newBuf, m.buf)
	m.buf = newBuf
	return m.buf
}

func (m *muxlLinearMemory) Free() { m.buf = nil }

// runWith instantiates the precompiled module with the given args and optional
// read-only FS mount. If any of initCh/segCh/eventCh is non-nil, stdout is
// parsed as a DRISL event stream and routed to them; otherwise stdout (if
// non-nil) receives the module's raw stdout. input (if non-nil) is piped to
// stdin. signFn (if non-nil) is registered for the call's duration so wasm
// calls into muxl.host_sign route to it. realClock=true exposes the host wall
// clock + real randomness, which c2pa-rs needs for signing/verification.
func (e *WASMEngine) runWith(
	ctx context.Context,
	args []string,
	fsCfg wazero.FSConfig,
	realClock bool,
	input io.Reader,
	stdout io.Writer,
	signFn func([]byte) ([]byte, error),
	initCh, segCh chan<- []byte,
	eventCh chan<- *Event,
) error {
	instanceID := e.counter.Add(1)
	instanceName := fmt.Sprintf("muxl-%d", instanceID)

	if signFn != nil {
		e.signers.Store(instanceName, signFn)
		defer e.signers.Delete(instanceName)
	}

	cfg := wazero.NewModuleConfig().
		WithName(instanceName).
		WithStderr(&logWriter{logger: e.logger, instance: instanceName}).
		WithArgs(args...)
	if realClock {
		cfg = cfg.
			WithSysWalltime().
			WithSysNanotime().
			WithSysNanosleep().
			WithRandSource(rand.Reader)
	}
	if fsCfg != nil {
		cfg = cfg.WithFSConfig(fsCfg)
	}

	var stdinWriter *io.PipeWriter
	if input != nil {
		var stdinReader *io.PipeReader
		stdinReader, stdinWriter = io.Pipe()
		cfg = cfg.WithStdin(stdinReader)
	}

	var stdoutReader *io.PipeReader
	var stdoutWriter *io.PipeWriter
	parse := initCh != nil || segCh != nil || eventCh != nil
	if parse {
		stdoutReader, stdoutWriter = io.Pipe()
		cfg = cfg.WithStdout(stdoutWriter)
	} else if stdout != nil {
		cfg = cfg.WithStdout(stdout)
	}

	allocator := &muxlAllocator{
		logger:       e.logger,
		instanceName: instanceName,
		initialBytes: e.memInitial,
		maxBytes:     e.memMax,
	}

	errCh := make(chan error, 1)
	go func() {
		instCtx := experimental.WithMemoryAllocator(ctx, allocator)
		instance, err := e.runtime.InstantiateModule(instCtx, e.compiled, cfg)
		if err != nil {
			e.logger.ErrorContext(ctx, "muxl: instantiate module", "instance", instanceName, "error", err)
		}
		// wazero leaves the module registered on clean exit; close it to free
		// the wasm memory (otherwise each call leaks ~10MB).
		if instance != nil {
			if closeErr := instance.Close(ctx); closeErr != nil {
				e.logger.ErrorContext(ctx, "muxl: close instance", "instance", instanceName, "error", closeErr)
			}
		}
		if stdoutWriter != nil {
			stdoutWriter.Close()
		}
		errCh <- err
	}()

	if input != nil {
		go func() {
			_, err := io.Copy(stdinWriter, input)
			if err != nil && !errors.Is(err, io.ErrClosedPipe) {
				e.logger.ErrorContext(ctx, "muxl: copy input to stdin", "instance", instanceName, "error", err)
			}
			stdinWriter.Close()
		}()
	}

	if parse {
		if err := parseEvents(ctx, stdoutReader, initCh, segCh, eventCh); err != nil {
			return fmt.Errorf("muxl: parsing events: %w", err)
		}
	}

	if wasmErr := <-errCh; wasmErr != nil {
		return fmt.Errorf("muxl: wasm execution: %w", wasmErr)
	}
	return nil
}

// parseEvents decodes the wasm's DRISL event stream and dispatches each event.
// Bytes route to initCh/segCh; the full *Event also goes to eventCh when set.
// For segment/signed-segment events the per-track bytes are concatenated in
// sorted track-ID order so the byte layout is deterministic. Any/all channels
// may be nil. None are closed here — the caller owns them.
func parseEvents(ctx context.Context, r io.Reader, initCh, segCh chan<- []byte, eventCh chan<- *Event) error {
	decoder := drisl.NewDecoder(r)
	for {
		var ev Event
		err := decoder.Decode(&ev)
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return fmt.Errorf("decode muxl event: %w", err)
		}
		switch ev.Type {
		case "init":
			if initCh != nil {
				select {
				case <-ctx.Done():
					return nil
				case initCh <- ev.Data:
				}
			}
		case "segment", "signed-segment":
			if segCh != nil {
				keys := make([]string, 0, len(ev.Tracks))
				for k := range ev.Tracks {
					keys = append(keys, k)
				}
				sort.Strings(keys)
				var combined []byte
				for _, k := range keys {
					combined = append(combined, ev.Tracks[k]...)
				}
				select {
				case <-ctx.Done():
					return nil
				case segCh <- combined:
				}
			}
		default:
			return fmt.Errorf("unknown event type: %s", ev.Type)
		}
		if eventCh != nil {
			select {
			case <-ctx.Done():
				return nil
			case eventCh <- &ev:
			}
		}
	}
	return nil
}

// logWriter adapts the wasm's stderr to slog, one record per line. Lines
// tagged "Error:" / "thread '" surface at error level; the rest at debug.
type logWriter struct {
	logger   *slog.Logger
	instance string
	buf      []byte
}

func (w *logWriter) Write(p []byte) (int, error) {
	w.buf = append(w.buf, p...)
	for {
		i := bytes.IndexByte(w.buf, '\n')
		if i < 0 {
			break
		}
		line := string(w.buf[:i])
		w.buf = w.buf[i+1:]
		if strings.HasPrefix(line, "Error:") || strings.HasPrefix(line, "thread '") {
			w.logger.Error("muxl wasm", "instance", w.instance, "msg", line)
		} else {
			w.logger.Debug("muxl wasm", "instance", w.instance, "msg", line)
		}
	}
	return len(p), nil
}
