package muxl_test

import (
	"bytes"
	"context"
	"testing"
	"time"

	muxl "github.com/streamplace/muxl/go"
)

func TestUnwrapCancellationUnblocksStdout(t *testing.T) {
	eng := newEngine(t) // The old captions singleton used these same defaults.
	canonical, err := eng.Canonicalize(context.Background(), readFile(t, fixtureFmp4))
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	events := make(chan *muxl.Event)
	done := make(chan error, 1)
	go func() { done <- eng.UnwrapEvents(ctx, bytes.NewReader(canonical), events) }()
	select {
	case <-events:
	case <-time.After(5 * time.Second):
		t.Fatal("unwrap did not start")
	}
	// Stop consuming while the module still has several events to write.
	cancel()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("cancelled unwrap remains blocked on stdout")
	}
	// Cancelling one module must not damage the process-wide engine.
	wrapped, err := eng.Canonicalize(context.Background(), readFile(t, fixtureFmp4))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(canonical, wrapped) {
		t.Fatal("subsequent canonical output changed after cancellation")
	}
}
