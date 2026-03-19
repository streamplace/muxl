// Example: embedding muxl as a WASI module in Go via wazero.
//
// Pipes an fMP4 stream through the muxl WASI binary and reads back
// CBOR (DRISL) events from stdout.
//
// Build the WASM binary first:
//
//	cargo build --target wasm32-wasip1 --release
//
// Then run this example:
//
//	go run ./examples/go-wasi /path/to/input.fmp4
//
// Or pipe from stdin:
//
//	cat input.fmp4 | go run ./examples/go-wasi -
package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"os"

	"github.com/hyphacoop/go-dasl/drisl"
	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/imports/wasi_snapshot_preview1"
)

// MuxlEvent represents a CBOR event from the muxl segmenter.
//
// Wire format (one CBOR map per event):
//
//	{"type": "init", "data": <bstr>}
//	{"type": "segment", "data": <bstr>}
type MuxlEvent struct {
	Type string `cbor:"type"`
	Data []byte `cbor:"data"`
}

func main() {
	if len(os.Args) < 2 {
		log.Fatal("Usage: go-wasi <input.fmp4 or ->")
	}

	// Open input
	var input io.Reader
	if os.Args[1] == "-" {
		input = os.Stdin
	} else {
		f, err := os.Open(os.Args[1])
		if err != nil {
			log.Fatal(err)
		}
		defer f.Close()
		input = f
	}

	events, err := RunMuxlSegmenter(context.Background(), input)
	if err != nil {
		log.Fatal(err)
	}

	for _, ev := range events {
		switch ev.Type {
		case "init":
			fmt.Fprintf(os.Stderr, "init segment: %d bytes\n", len(ev.Data))
		case "segment":
			fmt.Fprintf(os.Stderr, "segment: %d bytes\n", len(ev.Data))
		}
	}
}

// RunMuxlSegmenter runs the muxl WASI binary, piping the fMP4 input through
// stdin and parsing CBOR events from stdout.
//
// The WASM binary is loaded from the path in MUXL_WASM env var, or defaults
// to target/wasm32-wasip1/release/muxl.wasm.
func RunMuxlSegmenter(ctx context.Context, input io.Reader) ([]MuxlEvent, error) {
	wasmPath := os.Getenv("MUXL_WASM")
	if wasmPath == "" {
		wasmPath = "target/wasm32-wasip1/release/muxl.wasm"
	}

	wasmBytes, err := os.ReadFile(wasmPath)
	if err != nil {
		return nil, fmt.Errorf("reading wasm binary: %w (set MUXL_WASM env var)", err)
	}

	// Create wazero runtime
	r := wazero.NewRuntime(ctx)
	defer r.Close(ctx)

	// Instantiate WASI
	wasi_snapshot_preview1.MustInstantiate(ctx, r)

	// Set up stdin/stdout pipes
	stdinReader, stdinWriter := io.Pipe()
	stdoutReader, stdoutWriter := io.Pipe()

	config := wazero.NewModuleConfig().
		WithStdin(stdinReader).
		WithStdout(stdoutWriter).
		WithArgs("muxl", "segment", "-", "--stdout")

	// Compile the module
	compiled, err := r.CompileModule(ctx, wasmBytes)
	if err != nil {
		return nil, fmt.Errorf("compiling wasm: %w", err)
	}

	// Run the module in a goroutine
	errCh := make(chan error, 1)
	go func() {
		_, err := r.InstantiateModule(ctx, compiled, config)
		stdoutWriter.Close()
		errCh <- err
	}()

	// Feed input to stdin in a goroutine
	go func() {
		io.Copy(stdinWriter, input)
		stdinWriter.Close()
	}()

	// Parse CBOR events from stdout
	events, err := ParseMuxlEvents(stdoutReader)
	if err != nil {
		return nil, fmt.Errorf("parsing events: %w", err)
	}

	// Wait for WASM module to finish
	if wasmErr := <-errCh; wasmErr != nil {
		return nil, fmt.Errorf("wasm execution: %w", wasmErr)
	}

	return events, nil
}

// ParseMuxlEvents reads CBOR (DRISL) events from the muxl --stdout stream.
//
// Each event is a separate CBOR value — a map with "type", "data", and
// optionally "number" fields.
func ParseMuxlEvents(r io.Reader) ([]MuxlEvent, error) {
	var events []MuxlEvent
	decoder := drisl.NewDecoder(r)

	for {
		var ev MuxlEvent
		err := decoder.Decode(&ev)
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return events, fmt.Errorf("decoding CBOR event: %w", err)
		}
		events = append(events, ev)
	}

	return events, nil
}
