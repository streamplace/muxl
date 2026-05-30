package muxl

import (
	"context"
	"io"
)

// Concatenator is a push-style wrapper over an [Engine]: write whole fMP4
// archives, receive processed output on channels. It drives
// [Engine.SignSegment] (segment + S2PA-sign each canonical segment).
//
//	c := muxl.NewSigningSegmenter(ctx, eng, in)
//	go func() { c.Write(archive); c.Close() }()
//	initSeg := <-c.InitCh
//	for seg := range c.SegCh { /* append to output */ }
type Concatenator struct {
	stdinWriter *io.PipeWriter
	// InitCh receives an init segment only when the track configuration
	// changes. SegCh receives concatenable segment bodies (signed, for the
	// signing segmenter). EventCh receives the full *Event with per-segment
	// metadata. All three are closed when processing finishes.
	InitCh  chan []byte
	SegCh   chan []byte
	EventCh chan *Event
	done    chan error
}

// NewSigningSegmenter starts an [Engine.SignSegment] pipeline in the
// background: each canonical segment is S2PA-signed, so SegCh carries
// [c2pa-uuid][muxl-uuid][moof][mdat] per track. Exactly one of in.KeyPEM or
// in.Sign must be set.
func NewSigningSegmenter(ctx context.Context, eng Engine, in SignerInput) *Concatenator {
	return newConcatenator(func(c *Concatenator, r io.Reader) error {
		return eng.SignSegment(ctx, r, in, c.InitCh, c.SegCh, c.EventCh)
	})
}

func newConcatenator(run func(*Concatenator, io.Reader) error) *Concatenator {
	stdinReader, stdinWriter := io.Pipe()
	c := &Concatenator{
		stdinWriter: stdinWriter,
		InitCh:      make(chan []byte, 1),
		SegCh:       make(chan []byte, 16),
		EventCh:     make(chan *Event, 16),
		done:        make(chan error, 1),
	}
	go func() {
		err := run(c, stdinReader)
		close(c.InitCh)
		close(c.SegCh)
		close(c.EventCh)
		c.done <- err
	}()
	return c
}

// Write feeds a full fMP4 archive (init+segments) to the pipeline.
func (c *Concatenator) Write(data []byte) error {
	_, err := c.stdinWriter.Write(data)
	return err
}

// Close signals end of input and waits for the pipeline to finish; the output
// channels are closed by the time it returns.
func (c *Concatenator) Close() error {
	c.stdinWriter.Close()
	return <-c.done
}
