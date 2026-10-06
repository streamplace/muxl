package muxl_test

import (
	"bytes"
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"reflect"
	"sort"
	"strconv"
	"testing"

	"github.com/hyphacoop/go-dasl/drisl"
	muxl "github.com/streamplace/muxl/go"
)

// MUXL canonical-segment uuid (spec § MUXL Canonical Segment).
var muxlUUID = []byte{
	0xe6, 0x40, 0x4e, 0xa2, 0x8f, 0x01, 0x43, 0x05,
	0x98, 0xda, 0x7b, 0xec, 0x3c, 0x2a, 0x91, 0x73,
}

func isoBox(typ string, payload ...[]byte) []byte {
	size := 8
	for _, p := range payload {
		size += len(p)
	}
	out := make([]byte, 8, size)
	binary.BigEndian.PutUint32(out, uint32(size))
	copy(out[4:], typ)
	for _, p := range payload {
		out = append(out, p...)
	}
	return out
}

func isoFullBox(typ string, version byte, flags uint32, payload []byte) []byte {
	vf := make([]byte, 4)
	binary.BigEndian.PutUint32(vf, flags&0x00ffffff)
	vf[0] = version
	return isoBox(typ, vf, payload)
}

func be32(v uint32) []byte { b := make([]byte, 4); binary.BigEndian.PutUint32(b, v); return b }
func be64(v uint64) []byte { b := make([]byte, 8); binary.BigEndian.PutUint64(b, v); return b }

// textTrackSegment mints one canonical segment for a track group no current
// muxl understands (Hang's `text` group): a MUXL uuid whose catalog has only
// `text`, then one single-sample moof+mdat for trackID. Mirrors the Rust
// test helper so the embedded wasm is exercised on the same shape.
func textTrackSegment(t *testing.T, trackID uint32, decodeTimeMs uint64, durationMs uint32, payload []byte) []byte {
	t.Helper()
	catalog, err := drisl.Marshal(map[string]any{
		"text": map[string]any{
			"renditions": map[string]any{
				fmt.Sprintf("text%d", trackID): map[string]any{
					"format": "muxl-transcript",
					"role":   "caption",
					"lang":   "en",
					"container": map[string]any{
						"kind":      "cmaf",
						"timescale": uint32(1000),
						"trackId":   trackID,
					},
				},
			},
		},
	})
	if err != nil {
		t.Fatalf("drisl.Marshal: %v", err)
	}
	seg := isoBox("uuid", muxlUUID, catalog)

	const (
		trunDataOffset     = 0x000001
		trunSampleDuration = 0x000100
		trunSampleSize     = 0x000200
		trunSampleFlags    = 0x000400
		syncSampleFlags    = 0x02000000
	)
	buildMoof := func(dataOffset int32) []byte {
		mfhd := isoFullBox("mfhd", 0, 0, be32(1))
		tfhd := isoFullBox("tfhd", 0, 0, be32(trackID))
		tfdt := isoFullBox("tfdt", 1, 0, be64(decodeTimeMs))
		var trunBody []byte
		trunBody = append(trunBody, be32(1)...)
		trunBody = append(trunBody, be32(uint32(dataOffset))...)
		trunBody = append(trunBody, be32(durationMs)...)
		trunBody = append(trunBody, be32(uint32(len(payload)))...)
		trunBody = append(trunBody, be32(syncSampleFlags)...)
		trun := isoFullBox("trun", 0, trunDataOffset|trunSampleDuration|trunSampleSize|trunSampleFlags, trunBody)
		return isoBox("moof", mfhd, isoBox("traf", tfhd, tfdt, trun))
	}
	moofLen := len(buildMoof(0))
	seg = append(seg, buildMoof(int32(moofLen+8))...)
	seg = append(seg, isoBox("mdat", payload)...)
	return seg
}

// bareStreams segments the fixture and returns the canonical interleaved
// stream, plus the same stream with one text-track segment appended to every
// GoP, and the GoP count.
func bareStreams(t *testing.T, eng *muxl.WASMEngine) (baseline, withText []byte, gops int) {
	t.Helper()
	frag := readFile(t, fixtureFmp4)
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SegmentEvents(context.Background(), bytes.NewReader(frag), events)
	})
	if err != nil {
		t.Fatalf("SegmentEvents: %v", err)
	}
	var maxTID uint64
	for _, ev := range events {
		for tid := range ev.Tracks {
			n, err := strconv.ParseUint(tid, 10, 32)
			if err != nil {
				t.Fatalf("track id %q: %v", tid, err)
			}
			if n > maxTID {
				maxTID = n
			}
		}
	}
	textTID := uint32(maxTID + 1)
	for _, ev := range events {
		if ev.Type != "segment" {
			continue
		}
		tids := make([]string, 0, len(ev.Tracks))
		for tid := range ev.Tracks {
			tids = append(tids, tid)
		}
		sort.Strings(tids)
		for _, tid := range tids {
			baseline = append(baseline, ev.Tracks[tid]...)
			withText = append(withText, ev.Tracks[tid]...)
		}
		withText = append(withText, textTrackSegment(t, textTID, uint64(gops)*2000, 2000, fmt.Appendf(nil, "gop %d words", gops))...)
		gops++
	}
	if gops < 2 {
		t.Fatalf("fixture must span several GoPs, got %d", gops)
	}
	return baseline, withText, gops
}

// The embedded wasm must apply the forward-compatibility rule (spec § uuid
// Body): segments for a track group it doesn't know are ignored by wrap and
// the event stream, while the metafile path keeps them so a synthesized flat
// header still accounts for their bytes.
func TestReadersIgnoreUnknownTrackGroupSegments(t *testing.T) {
	eng := newEngine(t)
	ctx := context.Background()
	baseline, withText, gops := bareStreams(t, eng)

	for _, format := range []string{"fmp4", "flat"} {
		var want, got bytes.Buffer
		if err := eng.Wrap(ctx, bytes.NewReader(baseline), format, &want); err != nil {
			t.Fatalf("Wrap(%s) baseline: %v", format, err)
		}
		if err := eng.Wrap(ctx, bytes.NewReader(withText), format, &got); err != nil {
			t.Fatalf("Wrap(%s) with text track: %v", format, err)
		}
		if !bytes.Equal(want.Bytes(), got.Bytes()) {
			t.Errorf("Wrap(%s): output with ignored text segments differs from baseline", format)
		}
	}

	wantEvents, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.UnwrapEvents(ctx, bytes.NewReader(baseline), events)
	})
	if err != nil {
		t.Fatalf("UnwrapEvents baseline: %v", err)
	}
	gotEvents, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.UnwrapEvents(ctx, bytes.NewReader(withText), events)
	})
	if err != nil {
		t.Fatalf("UnwrapEvents with text track: %v", err)
	}
	if len(gotEvents) != len(wantEvents) {
		t.Fatalf("UnwrapEvents: %d events with text track, %d baseline", len(gotEvents), len(wantEvents))
	}
	for i := range wantEvents {
		if !reflect.DeepEqual(gotEvents[i], wantEvents[i]) {
			t.Errorf("UnwrapEvents: event %d differs with text track present", i)
		}
	}

	var metas bytes.Buffer
	if err := eng.Metafiles(ctx, bytes.NewReader(withText), &metas); err != nil {
		t.Fatalf("Metafiles with text track: %v", err)
	}
	dec := drisl.NewDecoder(bytes.NewReader(metas.Bytes()))
	var n, empty int
	for {
		var m map[string]any
		if err := dec.Decode(&m); err == io.EOF {
			break
		} else if err != nil {
			t.Fatalf("decode metafile %d: %v", n, err)
		}
		n++
		if cat, _ := m["catalog"].(map[string]any); len(cat) == 0 {
			empty++
		}
	}
	if wantN := len(wantEvents[0].TrackInits)*gops + gops; n != wantN {
		t.Errorf("Metafiles: got %d metafiles, want %d (known tracks × GoPs + one text segment per GoP)", n, wantN)
	}
	if empty != gops {
		t.Errorf("Metafiles: %d metafiles with an empty catalog, want %d (one per text segment)", empty, gops)
	}
	var header bytes.Buffer
	if err := eng.SynthesizeFlatHeader(ctx, bytes.NewReader(metas.Bytes()), &header); err != nil {
		t.Fatalf("SynthesizeFlatHeader over metafiles with a text track: %v", err)
	}
	if header.Len() == 0 {
		t.Error("SynthesizeFlatHeader produced no header")
	}
}
