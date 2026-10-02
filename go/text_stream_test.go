package muxl_test

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"reflect"
	"testing"
	"time"

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

func TestSignSegmentLateTextRefreshesInit(t *testing.T) {
	eng := newEngine(t)
	in := signerInput(t)
	calls := 0
	track := muxl.TextTrack{TrackID: 100, Language: "en-US", Label: "late"}
	var want muxl.TextCue
	in.TextFn = func(_ context.Context, req muxl.TextRequest) (*muxl.TextAttachment, error) {
		calls++
		if calls == 1 {
			return nil, nil
		}
		want = muxl.TextCue{Start: req.StartMs, End: req.EndMs, Text: "declared later"}
		return &muxl.TextAttachment{Tracks: []muxl.TextTrackAttachment{{TextTrack: track, Cues: []muxl.TextCue{want}}}}, nil
	}
	events, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.SignSegment(context.Background(), bytes.NewReader(readFile(t, fixtureFmp4)), in, nil, nil, ch)
	})
	if err != nil {
		t.Fatal(err)
	}
	var current *muxl.Event
	var stream bytes.Buffer
	for _, ev := range events {
		if ev.Type == "init" {
			current = ev
			continue
		}
		if ev.Type != "signed-segment" {
			continue
		}
		if data, ok := ev.Tracks["100"]; ok {
			if current == nil || current.Catalog == nil || current.Catalog.Text == nil {
				t.Fatal("late text segment has no preceding text Init catalog")
			}
			config := current.Catalog.Text.Renditions["text100"]
			if config.Language != track.Language || config.Label != track.Label {
				t.Fatalf("late Init config=%+v", config)
			}
			init := current.TrackInits["100"]
			playable := append(append([]byte(nil), init...), data...)
			gotTracks, err := eng.TextTracks(context.Background(), bytes.NewReader(init))
			if err != nil || !reflect.DeepEqual(gotTracks, []muxl.TextTrack{track}) {
				t.Fatalf("late per-track HLS init cannot initialize captions: %+v %v", gotTracks, err)
			}
			combinedTracks, err := eng.TextTracks(context.Background(), bytes.NewReader(current.Data))
			if err != nil || !reflect.DeepEqual(combinedTracks, []muxl.TextTrack{track}) {
				t.Fatalf("combined refreshed Init lost late text: %+v %v", combinedTracks, err)
			}
			got, err := eng.ReadTextCues(context.Background(), bytes.NewReader(playable), 100)
			if err != nil || !reflect.DeepEqual(got, []muxl.TextCue{want}) {
				t.Fatalf("late initialized captions=%+v %v", got, err)
			}
		}
		for _, id := range []string{"1", "2", "100"} {
			stream.Write(ev.Tracks[id])
		}
	}
	if calls != 2 {
		t.Fatalf("callback calls=%d", calls)
	}
	// muxl's unwrap path discovers late tracks from their own catalogs.
	gotTracks, err := eng.TextTracks(context.Background(), bytes.NewReader(stream.Bytes()))
	if err != nil || !reflect.DeepEqual(gotTracks, []muxl.TextTrack{track}) {
		t.Fatalf("late canonical track unreachable: %+v %v", gotTracks, err)
	}
}

func TestSignSegmentMissingReferenceSkipsMediaCallbacks(t *testing.T) {
	eng := newEngine(t)
	ctx := context.Background()
	original, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.SegmentEvents(ctx, bytes.NewReader(readFile(t, fixtureFmp4)), ch)
	})
	if err != nil {
		t.Fatal(err)
	}
	var sparse bytes.Buffer
	for _, ev := range original {
		if ev.Type == "init" {
			sparse.Write(ev.Data) // Catalog still declares the absent video reference.
		} else if ev.Type == "segment" {
			sparse.Write(ev.Tracks["2"])
		}
	}
	for _, withText := range []bool{false, true} {
		t.Run(fmt.Sprint("text=", withText), func(t *testing.T) {
			in := signerInput(t)
			if withText {
				in.TextFn = func(context.Context, muxl.TextRequest) (*muxl.TextAttachment, error) {
					t.Error("TextFn received an invented reference span")
					return nil, nil
				}
			}
			in.SegmentTimeFn = func(uint64) time.Time {
				t.Error("SegmentTimeFn received an invented reference start")
				return time.Time{}
			}
			events, err := collectEvents(func(ch chan<- *muxl.Event) error {
				return eng.SignSegment(ctx, bytes.NewReader(sparse.Bytes()), in, nil, nil, ch)
			})
			if err != nil {
				t.Fatal(err)
			}
			var signed []byte
			for _, ev := range events {
				if ev.Type == "signed-segment" {
					signed = append(signed, ev.Tracks["2"]...)
					if _, ok := ev.Tracks["100"]; ok {
						t.Fatal("invented a text track without a reference span")
					}
				}
			}
			report, err := eng.Verify(ctx, bytes.NewReader(signed))
			if err != nil {
				t.Fatal(err)
			}
			var doc verifyDoc
			if err := json.Unmarshal([]byte(report), &doc); err != nil {
				t.Fatal(err)
			}
			if len(doc.Segments) != 1 || doc.Segments[0].ValidationState == "Invalid" {
				t.Fatalf("sparse audio signing failed: %s", report)
			}
		})
	}
}
