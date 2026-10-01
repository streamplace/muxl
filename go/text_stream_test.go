package muxl_test

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"reflect"
	"testing"

	muxl "github.com/streamplace/muxl/go"
)

func TestSignSegmentText(t *testing.T) {
	eng := newEngine(t)
	in := signerInput(t)
	var want muxl.TextCue
	var track muxl.TextTrack
	calls := 0
	in.TextFn = func(ctx context.Context, req muxl.TextRequest) (*muxl.TextAttachment, error) {
		calls++
		if calls == 1 {
			track = muxl.TextTrack{TrackID: 100, Language: "en-US", Label: "ingest"}
			want = muxl.TextCue{Start: req.EndMs - 100, End: req.EndMs + 100, Text: "across the boundary", ID: "crossing"}
		}
		return &muxl.TextAttachment{Tracks: []muxl.TextTrackAttachment{{TextTrack: track, Cues: []muxl.TextCue{want}}}}, nil
	}
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SignSegment(context.Background(), bytes.NewReader(readFile(t, fixtureFmp4)), in, nil, nil, events)
	})
	if err != nil {
		t.Fatal(err)
	}
	var signed bytes.Buffer
	gops := 0
	for _, ev := range events {
		if ev.Type != "signed-segment" {
			continue
		}
		gops++
		if _, ok := ev.Tracks[fmt.Sprint(track.TrackID)]; !ok {
			t.Fatalf("GoP %d lost text track", gops)
		}
		for _, tid := range []string{"1", "2", fmt.Sprint(track.TrackID)} {
			data, ok := ev.Tracks[tid]
			if !ok {
				t.Fatalf("missing track %s", tid)
			}
			report, err := eng.Verify(context.Background(), bytes.NewReader(data))
			if err != nil {
				t.Fatal(err)
			}
			var doc verifyDoc
			if err := json.Unmarshal([]byte(report), &doc); err != nil {
				t.Fatal(err)
			}
			if len(doc.Segments) != 1 || doc.Segments[0].ValidationState == "Invalid" {
				t.Fatalf("track %s fails validation: %s", tid, report)
			}
			signed.Write(data)
		}
	}
	if calls != gops || gops != 2 {
		t.Fatalf("calls=%d GoPs=%d", calls, gops)
	}
	tracks, err := eng.TextTracks(context.Background(), bytes.NewReader(signed.Bytes()))
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(tracks, []muxl.TextTrack{track}) {
		t.Fatalf("tracks: %+v", tracks)
	}
	got, err := eng.ReadTextCues(context.Background(), bytes.NewReader(signed.Bytes()), track.TrackID)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, []muxl.TextCue{want}) {
		t.Fatalf("reconstructed cues: %+v want %+v", got, want)
	}
}

func TestSignSegmentTextErrorKeepsTrackContinuous(t *testing.T) {
	eng := newEngine(t)
	in := signerInput(t)
	calls := 0
	in.TextFn = func(ctx context.Context, req muxl.TextRequest) (*muxl.TextAttachment, error) {
		calls++
		if calls > 1 {
			return nil, fmt.Errorf("speech unavailable")
		}
		return &muxl.TextAttachment{Tracks: []muxl.TextTrackAttachment{{TextTrack: muxl.TextTrack{TrackID: 100, Language: "en", Label: "auto"}, Cues: []muxl.TextCue{{Start: req.StartMs, End: req.StartMs + 100, Text: "first"}}}}}, nil
	}
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SignSegment(context.Background(), bytes.NewReader(readFile(t, fixtureFmp4)), in, nil, nil, events)
	})
	if err != nil {
		t.Fatal(err)
	}
	for _, ev := range events {
		if ev.Type != "signed-segment" || ev.Number != 2 {
			continue
		}
		data, ok := ev.Tracks["100"]
		if !ok {
			t.Fatal("callback error removed declared text track")
		}
		cues, err := eng.ReadTextCues(context.Background(), bytes.NewReader(data), 100)
		if err != nil || len(cues) != 0 {
			t.Fatalf("error GoP must carry gaps: %+v %v", cues, err)
		}
		report, err := eng.Verify(context.Background(), bytes.NewReader(data))
		if err != nil {
			t.Fatal(err)
		}
		var doc verifyDoc
		if err := json.Unmarshal([]byte(report), &doc); err != nil {
			t.Fatal(err)
		}
		if len(doc.Segments) != 1 || doc.Segments[0].ValidationState == "Invalid" {
			t.Fatalf("empty track fails verification: %s", report)
		}
		return
	}
	t.Fatal("second GoP missing")
}
