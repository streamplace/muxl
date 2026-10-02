package muxl_test

import (
	"bytes"
	"context"
	"testing"

	muxl "github.com/streamplace/muxl/go"
)

func TestSignSegmentCanonicalNumericTrackOrder(t *testing.T) {
	engine := newEngine(t)
	in := signerInput(t)
	in.TextFn = func(_ context.Context, req muxl.TextRequest) (*muxl.TextAttachment, error) {
		return &muxl.TextAttachment{Tracks: []muxl.TextTrackAttachment{
			{TextTrack: muxl.TextTrack{TrackID: 3, Language: "en", Label: "human"}, Cues: []muxl.TextCue{{Start: req.StartMs, End: req.EndMs, Text: "third"}}},
			{TextTrack: muxl.TextTrack{TrackID: 10, Language: "es", Label: "human"}, Cues: []muxl.TextCue{{Start: req.StartMs, End: req.EndMs, Text: "tenth"}}},
		}}, nil
	}
	segments := make(chan []byte, 16)
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return engine.SignSegment(context.Background(), bytes.NewReader(readFile(t, fixtureFmp4)), in, nil, segments, events)
	})
	if err != nil {
		t.Fatal(err)
	}
	for _, event := range events {
		if event.Type == "signed-segment" {
			segment := <-segments
			for _, id := range []string{"1", "2", "3", "10"} {
				track := event.Tracks[id]
				if !bytes.HasPrefix(segment, track) {
					t.Fatalf("GoP %d is not in canonical numeric order at track %s", event.Number, id)
				}
				segment = segment[len(track):]
			}
		}
	}
}
