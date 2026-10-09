package muxl_test

import (
	"bytes"
	"context"
	"encoding/binary"
	"fmt"
	"io"
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

// numericTrackIDs returns ev.Tracks' keys as track ids in ascending numeric
// order (the keys are stringified, so a lexical sort would misorder 10 vs 2).
func numericTrackIDs(t *testing.T, ev *muxl.Event) []uint32 {
	t.Helper()
	tids := make([]uint32, 0, len(ev.Tracks))
	for key := range ev.Tracks {
		n, err := strconv.ParseUint(key, 10, 32)
		if err != nil {
			t.Fatalf("track id %q: %v", key, err)
		}
		tids = append(tids, uint32(n))
	}
	sort.Slice(tids, func(i, j int) bool { return tids[i] < tids[j] })
	return tids
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
	var maxTID uint32
	for _, ev := range events {
		for _, tid := range numericTrackIDs(t, ev) {
			if tid > maxTID {
				maxTID = tid
			}
		}
	}
	textTID := maxTID + 1
	for _, ev := range events {
		if ev.Type != "segment" {
			continue
		}
		// Canonical interleave order: tracks ascend numerically within a GoP.
		for _, tid := range numericTrackIDs(t, ev) {
			seg := ev.Tracks[strconv.FormatUint(uint64(tid), 10)]
			baseline = append(baseline, seg...)
			withText = append(withText, seg...)
		}
		withText = append(withText, textTrackSegment(t, textTID, uint64(gops)*2000, 2000, fmt.Appendf(nil, "gop %d words", gops))...)
		gops++
	}
	if gops < 2 {
		t.Fatalf("fixture must span several GoPs, got %d", gops)
	}
	return baseline, withText, gops
}

// streamBytes concatenates every segment event's per-track bytes in canonical
// interleave order — the layout a consumer summing event bytes for byte-range
// offsets assumes the stored blob has.
func streamBytes(t *testing.T, events []*muxl.Event) []byte {
	t.Helper()
	var out []byte
	for _, ev := range events {
		if ev.Type != "segment" {
			continue
		}
		for _, tid := range numericTrackIDs(t, ev) {
			out = append(out, ev.Tracks[strconv.FormatUint(uint64(tid), 10)]...)
		}
	}
	return out
}

// The embedded wasm must apply the forward-compatibility rule (spec § uuid
// Body): a segment of a track group it doesn't decode is an opaque track —
// carried through events, metafiles, and the flat envelope, left out only of
// the moov and the appendable fMP4.
func TestReadersCarryUnknownTrackGroupSegments(t *testing.T) {
	eng := newEngine(t)
	ctx := context.Background()
	baseline, withText, gops := bareStreams(t, eng)

	unwrapEvents := func(name string, input []byte) []*muxl.Event {
		t.Helper()
		events, err := collectEvents(func(events chan<- *muxl.Event) error {
			return eng.UnwrapEvents(ctx, bytes.NewReader(input), events)
		})
		if err != nil {
			t.Fatalf("UnwrapEvents %s: %v", name, err)
		}
		return events
	}

	wantEvents := unwrapEvents("baseline", baseline)
	gotEvents := unwrapEvents("with text track", withText)
	if got := streamBytes(t, gotEvents); !bytes.Equal(got, withText) {
		t.Errorf("UnwrapEvents: segment bytes reassemble to %d bytes, want the %d-byte input", len(got), len(withText))
	}
	if !bytes.Equal(gotEvents[0].Data, wantEvents[0].Data) {
		t.Error("UnwrapEvents: Init moov changed; it must declare only known tracks")
	}
	if len(gotEvents[0].TrackInits) != len(wantEvents[0].TrackInits) {
		t.Errorf("UnwrapEvents: %d track inits, want %d (none for the text track)",
			len(gotEvents[0].TrackInits), len(wantEvents[0].TrackInits))
	}

	var fmp4Want, fmp4Got bytes.Buffer
	if err := eng.Wrap(ctx, bytes.NewReader(baseline), "fmp4", &fmp4Want); err != nil {
		t.Fatalf("Wrap(fmp4) baseline: %v", err)
	}
	if err := eng.Wrap(ctx, bytes.NewReader(withText), "fmp4", &fmp4Got); err != nil {
		t.Fatalf("Wrap(fmp4) with text track: %v", err)
	}
	if !bytes.Equal(fmp4Got.Bytes(), fmp4Want.Bytes()) {
		t.Error("Wrap(fmp4): text segments must be left out of the appendable fMP4")
	}

	var flat bytes.Buffer
	if err := eng.Wrap(ctx, bytes.NewReader(withText), "flat", &flat); err != nil {
		t.Fatalf("Wrap(flat) with text track: %v", err)
	}
	if got := streamBytes(t, unwrapEvents("flat", flat.Bytes())); !bytes.Equal(got, withText) {
		t.Error("Wrap(flat): unwrapping the flat MP4 must recover every segment, text included")
	}

	var metas bytes.Buffer
	if err := eng.Metafiles(ctx, bytes.NewReader(withText), &metas); err != nil {
		t.Fatalf("Metafiles with text track: %v", err)
	}
	dec := drisl.NewDecoder(bytes.NewReader(metas.Bytes()))
	var n, text int
	for {
		var m map[string]any
		if err := dec.Decode(&m); err == io.EOF {
			break
		} else if err != nil {
			t.Fatalf("decode metafile %d: %v", n, err)
		}
		n++
		if cat, _ := m["catalog"].(map[string]any); cat["text"] != nil {
			text++
		}
	}
	if wantN := len(wantEvents[0].TrackInits)*gops + gops; n != wantN {
		t.Errorf("Metafiles: got %d metafiles, want %d (known tracks × GoPs + one text segment per GoP)", n, wantN)
	}
	if text != gops {
		t.Errorf("Metafiles: %d metafiles carry the text group, want %d", text, gops)
	}
	var header bytes.Buffer
	if err := eng.SynthesizeFlatHeader(ctx, bytes.NewReader(metas.Bytes()), &header); err != nil {
		t.Fatalf("SynthesizeFlatHeader over metafiles with a text track: %v", err)
	}
}
