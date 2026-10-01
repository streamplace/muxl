package muxl

import (
	"bytes"
	"context"
	"math/rand/v2"
	"os"
	"testing"

	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/sys"
	"testing/fstest"
)

// Signing normally uses fresh time/randomness. Fix both in this compatibility
// test so equality means whole signed event-stream equality, not just AV bytes.
func fixedNoTextSign(t *testing.T, host bool) []byte {
	t.Helper()
	ctx := context.Background()
	e, err := NewWASM(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer e.Close(ctx)
	cert, err := os.ReadFile("../samples/test-keys/es256k-cert.pem")
	if err != nil {
		t.Fatal(err)
	}
	key, err := os.ReadFile("../samples/test-keys/es256k-key.pem")
	if err != nil {
		t.Fatal(err)
	}
	input, err := os.ReadFile("../samples/fixtures/h264-opus-frag.mp4")
	if err != nil {
		t.Fatal(err)
	}
	manifest := []byte(`{"title":"byte compatibility","assertions":[{"label":"c2pa.actions","data":{"actions":[{"action":"c2pa.created"}]}}]}`)
	fs := fstest.MapFS{"cert.pem": {Data: cert}, "key.pem": {Data: key}, "track.json": {Data: manifest}, "wrapper.json": {Data: manifest}}
	args := []string{"muxl", "sign-segment", "--cert", "/keys/cert.pem", "--key", "/keys/key.pem", "--alg", "es256k"}
	if host {
		args = append(args, "--host-manifest")
		e.manifestFetchers.Store("nil-text", func(uint32) ([]byte, error) { return manifest, nil })
	} else {
		args = append(args, "--track-manifest", "/keys/track.json", "--wrapper-manifest", "/keys/wrapper.json")
	}
	var out bytes.Buffer
	cfg := wazero.NewModuleConfig().WithName("nil-text").WithArgs(args...).WithFSConfig(wazero.NewFSConfig().WithFSMount(fs, "/keys")).WithStdin(bytes.NewReader(input)).WithStdout(&out).WithStderr(os.Stderr).
		WithWalltime(func() (int64, int32) { return 1800000000, 0 }, sys.ClockResolution(1000000)).
		WithNanotime(func() int64 { return 0 }, sys.ClockResolution(1)).WithRandSource(rand.NewChaCha8([32]byte{1}))
	instance, err := e.runtime.InstantiateModule(ctx, e.compiled, cfg)
	if instance != nil {
		instance.Close(ctx)
	}
	if err != nil {
		t.Fatal(err)
	}
	return out.Bytes()
}

func TestSignSegmentNilTextByteIdentical(t *testing.T) {
	original := fixedNoTextSign(t, false)
	host := fixedNoTextSign(t, true)
	if !bytes.Equal(original, host) {
		t.Fatalf("nil text changed signed bytes: static=%d host=%d", len(original), len(host))
	}
}
