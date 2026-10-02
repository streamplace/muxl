package muxl_test

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"sort"
	"strconv"
	"testing"
	"time"

	muxl "github.com/streamplace/muxl/go"
)

func joinTrackRuns(t *testing.T, tracks map[string][]byte) []byte {
	t.Helper()
	ids := make([]int, 0, len(tracks))
	for id := range tracks {
		n, err := strconv.Atoi(id)
		if err != nil {
			t.Fatal(err)
		}
		ids = append(ids, n)
	}
	sort.Ints(ids)
	var out []byte
	for _, id := range ids {
		out = append(out, tracks[strconv.Itoa(id)]...)
	}
	return out
}

func canonicalSignedBody(t *testing.T, run []byte) []byte {
	t.Helper()
	if len(run) < 24 || string(run[4:8]) != "uuid" {
		t.Fatal("signed run missing C2PA UUID prefix")
	}
	prefix := int(binary.BigEndian.Uint32(run[:4]))
	if prefix < 24 || prefix > len(run)-24 || string(run[prefix+4:prefix+8]) != "uuid" {
		t.Fatal("invalid signed run prefix")
	}
	return run[prefix:]
}

func TestSignTextRunsMatchesStreaming(t *testing.T) {
	ctx := context.Background()
	eng := newEngine(t)
	in := signerInput(t)
	when := time.Date(2026, 2, 3, 4, 5, 6, 789000000, time.UTC)
	in.SegmentTimeFn = func(uint64) time.Time { return when }
	var requests []muxl.TextRequest
	var attachments []muxl.TextTrackAttachment
	in.TextFn = func(_ context.Context, req muxl.TextRequest) (*muxl.TextAttachment, error) {
		requests = append(requests, req)
		track := muxl.TextTrackAttachment{
			TextTrack: muxl.TextTrack{TrackID: 100, Language: "en-US", Label: "captions"},
			Cues:      []muxl.TextCue{{Start: req.StartMs + 250, End: req.EndMs + 100, Text: "cue A", ID: "caption", Settings: "line:80%"}},
		}
		attachments = append(attachments, track)
		return &muxl.TextAttachment{Tracks: []muxl.TextTrackAttachment{track}}, nil
	}
	events, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.SignSegment(ctx, bytes.NewReader(readFile(t, fixtureFmp4)), in, nil, nil, ch)
	})
	if err != nil {
		t.Fatal(err)
	}
	var gops []*muxl.Event
	for _, ev := range events {
		if ev.Type == "signed-segment" {
			gops = append(gops, ev)
		}
	}
	if len(gops) != 2 || len(requests) != len(gops) {
		t.Fatalf("GoPs=%d requests=%d", len(gops), len(requests))
	}

	// Add an independently signed audio transcode at ID 3. This fixture is
	// Opus, but its standalone signing and parentOf binding match the AAC path.
	unsigned, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.SegmentEvents(ctx, bytes.NewReader(readFile(t, fixtureFmp4)), ch, muxl.WithSegmentTrackRemap(map[uint32]uint32{2: 3}))
	})
	if err != nil {
		t.Fatal(err)
	}
	var output []byte
	for _, ev := range unsigned {
		if ev.Type == "segment" {
			output = ev.Tracks["3"]
			break
		}
	}
	if output == nil {
		t.Fatal("remapped audio missing")
	}
	manifest := []byte(fmt.Sprintf(`{"title":"extra audio", "assertions":[{"label":"c2pa.actions","data":{"actions":[{"action":"c2pa.transcoded","parameters":{"org.cai.ingredientIds":[%q]}}]}}]}`, muxl.TranscodeIngredientLabel))
	extra, err := eng.SignTranscode(ctx, muxl.TranscodeInput{Output: output, Source: gops[0].Tracks["2"], CertPEM: in.CertPEM, KeyPEM: in.KeyPEM, Manifest: manifest})
	if err != nil {
		t.Fatal(err)
	}

	for i, gop := range gops {
		t.Run(fmt.Sprintf("GoP%d", i+1), func(t *testing.T) {
			req := requests[i]
			archiveIn := signerInput(t)
			archiveIn.TextFn = func(context.Context, muxl.TextRequest) (*muxl.TextAttachment, error) {
				t.Error("SignTextRuns invoked TextFn")
				return nil, errors.New("unused")
			}
			archiveIn.SegmentTimeFn = func(start uint64) time.Time {
				if start != req.StartMs {
					t.Errorf("segment time start=%d want %d", start, req.StartMs)
				}
				return when
			}
			// Exercise both the static and dynamic manifest paths.
			if i == 1 {
				archiveIn.TrackManifest = []byte(`not JSON`)
				archiveIn.TrackManifestFn = func() ([]byte, error) { return in.TrackManifest, nil }
			}
			runs, err := eng.SignTextRuns(ctx, req, []muxl.TextTrackAttachment{attachments[i], {TextTrack: muxl.TextTrack{TrackID: 101}}}, archiveIn)
			if err != nil {
				t.Fatal(err)
			}
			if len(runs) != 2 || runs[100] == nil || runs[101] == nil {
				t.Fatalf("unexpected runs: %v", runs)
			}
			if !bytes.Equal(canonicalSignedBody(t, runs[100]), canonicalSignedBody(t, gop.Tracks["100"])) {
				t.Fatal("standalone text differs from streaming canonical body")
			}
			tracks := make(map[string][]byte)
			for id, run := range gop.Tracks {
				tracks[id] = run
			}
			if i == 0 {
				tracks["3"] = extra
			}
			for id, run := range runs {
				tracks[strconv.FormatUint(uint64(id), 10)] = run
			}
			result := joinTrackRuns(t, tracks)
			want := attachments[i].Cues[0]
			want.End = req.EndMs
			cues, err := eng.ReadTextCues(ctx, bytes.NewReader(result), 100)
			if err != nil || !reflect.DeepEqual(cues, []muxl.TextCue{want}) {
				t.Fatalf("cues=%+v err=%v want=%+v", cues, err, want)
			}
			cues, err = eng.ReadTextCues(ctx, bytes.NewReader(result), 101)
			if err != nil || len(cues) != 0 {
				t.Fatalf("gap-only cues=%+v err=%v", cues, err)
			}
			textTracks, err := eng.TextTracks(ctx, bytes.NewReader(result))
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(textTracks, []muxl.TextTrack{attachments[i].TextTrack, {TrackID: 101, Language: "und"}}) {
				t.Fatalf("text tracks=%+v", textTracks)
			}
			report, err := eng.Verify(ctx, bytes.NewReader(result))
			if err != nil {
				t.Fatal(err)
			}
			var doc struct {
				Segments []struct {
					TrackID  uint32 `json:"track_id"`
					State    string `json:"validation_state"`
					Manifest struct {
						Ingredients []struct {
							Relationship string `json:"relationship"`
						} `json:"ingredients"`
						Assertions []struct {
							Label string                     `json:"label"`
							Data  map[string]json.RawMessage `json:"data"`
						} `json:"assertions"`
					} `json:"manifest"`
				} `json:"segments"`
			}
			if err := json.Unmarshal([]byte(report), &doc); err != nil {
				t.Fatal(err)
			}
			if len(doc.Segments) != len(tracks) {
				t.Fatalf("verified %d runs, expected %d", len(doc.Segments), len(tracks))
			}
			for _, run := range doc.Segments {
				if run.State == "Invalid" {
					t.Fatalf("run %d invalid: %s", run.TrackID, report)
				}
				if run.TrackID == 3 && (len(run.Manifest.Ingredients) != 1 || run.Manifest.Ingredients[0].Relationship != "parentOf") {
					t.Fatal("transcode parentOf lost")
				}
				if run.TrackID < 100 {
					continue
				}
				date := ""
				for _, assertion := range run.Manifest.Assertions {
					if assertion.Label == "cawg.metadata" {
						if err := json.Unmarshal(assertion.Data["dc:date"], &date); err != nil {
							t.Fatal(err)
						}
					}
				}
				if date != when.Format("2006-01-02T15:04:05.000Z") {
					t.Errorf("track %d dc:date=%q", run.TrackID, date)
				}
			}
		})
	}
}

func TestSignTextRunsErrors(t *testing.T) {
	ctx := context.Background()
	eng := newEngine(t)
	in := signerInput(t)
	for _, tc := range []struct {
		name string
		req  muxl.TextRequest
		ids  []uint32
	}{
		{"zero ID", muxl.TextRequest{StartMs: 0, EndMs: 1000}, []uint32{0}},
		{"duplicate ID", muxl.TextRequest{StartMs: 0, EndMs: 1000}, []uint32{100, 100}},
		{"zero duration", muxl.TextRequest{StartMs: 1000, EndMs: 1000}, []uint32{100}},
		{"negative duration", muxl.TextRequest{StartMs: 1000, EndMs: 999}, []uint32{100}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			var tracks []muxl.TextTrackAttachment
			for _, id := range tc.ids {
				tracks = append(tracks, muxl.TextTrackAttachment{TextTrack: muxl.TextTrack{TrackID: id}})
			}
			if _, err := eng.SignTextRuns(ctx, tc.req, tracks, in); err == nil {
				t.Fatal("invalid request accepted")
			}
		})
	}
	req := muxl.TextRequest{StartMs: 0, EndMs: 1000}
	tracks := []muxl.TextTrackAttachment{{TextTrack: muxl.TextTrack{TrackID: 100}}}
	in.Sign = func([]byte) ([]byte, error) { return nil, nil }
	if _, err := eng.SignTextRuns(ctx, req, tracks, in); err == nil {
		t.Fatal("two signers accepted")
	}
	in.Sign = nil
	in.KeyPEM = nil
	if _, err := eng.SignTextRuns(ctx, req, tracks, in); err == nil {
		t.Fatal("missing signer accepted")
	}
	in = signerInput(t)
	tracks[0].Cues = []muxl.TextCue{{Start: 200, End: 100, Text: "backwards"}}
	if _, err := eng.SignTextRuns(ctx, req, tracks, in); err == nil {
		t.Fatal("backwards cue accepted")
	}
	tracks[0].Cues = nil
	in.TrackManifestFn = func() ([]byte, error) { return nil, errors.New("manifest unavailable") }
	if _, err := eng.SignTextRuns(ctx, req, tracks, in); err == nil {
		t.Fatal("manifest failure swallowed")
	}
	in = signerInput(t)
	runs, err := eng.SignTextRuns(ctx, req, nil, in)
	if err != nil || len(runs) != 0 {
		t.Fatalf("empty track list=%v err=%v", runs, err)
	}
}
