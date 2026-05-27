package muxl_test

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"
	"testing"

	muxl "github.com/streamplace/muxl/go"
)

// Fixtures live in the muxl repo alongside the Go module; go test runs with
// the package directory as its working dir, so they sit one level up.
const (
	fixtureFmp4 = "../samples/fixtures/h264-opus-frag.mp4"
	certPath    = "../samples/test-keys/es256k-cert.pem"
	keyPath     = "../samples/test-keys/es256k-key.pem"
)

func newEngine(t *testing.T) *muxl.WASMEngine {
	t.Helper()
	eng, err := muxl.NewWASM(context.Background())
	if err != nil {
		t.Fatalf("NewWASM: %v", err)
	}
	t.Cleanup(func() { eng.Close(context.Background()) })
	return eng
}

func readFile(t *testing.T, path string) []byte {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	return b
}

func signerInput(t *testing.T) muxl.SignerInput {
	manifest := []byte(`{
		"title": "muxl-go test",
		"assertions": [
			{"label": "c2pa.actions", "data": {"actions": [{"action": "c2pa.created"}]}}
		]
	}`)
	return muxl.SignerInput{
		CertPEM:         readFile(t, certPath),
		KeyPEM:          readFile(t, keyPath),
		TrackManifest:   manifest,
		WrapperManifest: manifest,
	}
}

// collectEvents runs a streaming engine call in the background and returns all
// events it emits, plus the call's error.
func collectEvents(run func(events chan<- *muxl.Event) error) ([]*muxl.Event, error) {
	events := make(chan *muxl.Event, 16)
	errCh := make(chan error, 1)
	go func() {
		err := run(events)
		close(events)
		errCh <- err
	}()
	var out []*muxl.Event
	for ev := range events {
		out = append(out, ev)
	}
	return out, <-errCh
}

// firstTrackBytes returns the first track's bytes (sorted by track id) from
// the first event of the given type.
func firstTrackBytes(events []*muxl.Event, typ string) []byte {
	for _, ev := range events {
		if ev.Type != typ || len(ev.Tracks) == 0 {
			continue
		}
		tids := make([]string, 0, len(ev.Tracks))
		for tid := range ev.Tracks {
			tids = append(tids, tid)
		}
		sort.Strings(tids)
		return ev.Tracks[tids[0]]
	}
	return nil
}

// firstSignedSegment signs the fixture and returns a single signed canonical
// segment (the first track of the first GoP) — the shape of one transcode
// source ingredient.
func firstSignedSegment(t *testing.T, eng *muxl.WASMEngine) []byte {
	t.Helper()
	frag := readFile(t, fixtureFmp4)
	in := signerInput(t)
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SignSegment(context.Background(), bytes.NewReader(frag), in, nil, nil, events)
	})
	if err != nil {
		t.Fatalf("SignSegment: %v", err)
	}
	seg := firstTrackBytes(events, "signed-segment")
	if len(seg) == 0 {
		t.Fatal("SignSegment produced no signed segment")
	}
	return seg
}

// signedM4sStream signs the fixture and concatenates every signed per-track
// segment into one bare .m4s byte stream — the storage/wire form.
func signedM4sStream(t *testing.T, eng *muxl.WASMEngine) []byte {
	t.Helper()
	frag := readFile(t, fixtureFmp4)
	in := signerInput(t)
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SignSegment(context.Background(), bytes.NewReader(frag), in, nil, nil, events)
	})
	if err != nil {
		t.Fatalf("SignSegment: %v", err)
	}
	var m4s []byte
	for _, ev := range events {
		if ev.Type != "signed-segment" {
			continue
		}
		tids := make([]string, 0, len(ev.Tracks))
		for tid := range ev.Tracks {
			tids = append(tids, tid)
		}
		sort.Strings(tids)
		for _, tid := range tids {
			m4s = append(m4s, ev.Tracks[tid]...)
		}
	}
	if len(m4s) == 0 {
		t.Fatal("SignSegment produced no signed track bytes")
	}
	return m4s
}

func TestCanonicalize(t *testing.T) {
	eng := newEngine(t)
	in := readFile(t, fixtureFmp4)

	canon, err := eng.Canonicalize(context.Background(), in)
	if err != nil {
		t.Fatalf("Canonicalize: %v", err)
	}
	if len(canon) == 0 {
		t.Fatal("Canonicalize returned no bytes")
	}
	if !bytes.Contains(canon[:64], []byte("ftyp")) {
		t.Errorf("canonical output should start with an ftyp box")
	}

	// The canonical fMP4 must segment cleanly (proving it's valid fMP4).
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SegmentEvents(context.Background(), bytes.NewReader(canon), events)
	})
	if err != nil {
		t.Fatalf("SegmentEvents on canonicalized output: %v", err)
	}
	var segs int
	for _, ev := range events {
		if ev.Type == "segment" {
			segs++
		}
	}
	if segs == 0 {
		t.Error("canonicalized output produced no segments")
	}
}

func TestCanonicalizeWithTrackRemap(t *testing.T) {
	eng := newEngine(t)
	in := readFile(t, fixtureFmp4)

	// Baseline catalog: the fixture has one video + one audio track.
	audio0, video0 := canonicalTrackIDs(t, eng, in, nil)
	if len(audio0) != 1 || len(video0) != 1 {
		t.Fatalf("fixture should have one audio + one video track, got audio=%v video=%v", audio0, video0)
	}
	srcAudio := audio0[0]

	// Remap the audio track to a free high id (the Streamplace transcode flow:
	// give a freshly canonicalized rendition a free id before transcode-signing).
	const newAudio uint32 = 99
	audio1, video1 := canonicalTrackIDs(t, eng, in, map[uint32]uint32{srcAudio: newAudio})
	if len(audio1) != 1 || audio1[0] != newAudio {
		t.Errorf("audio should be remapped to %d, got %v", newAudio, audio1)
	}
	if len(video1) != 1 || video1[0] != video0[0] {
		t.Errorf("video id should be unchanged: before=%v after=%v", video0, video1)
	}
	if video1[0] == newAudio {
		t.Errorf("video id %d collides with the remapped audio id", video1[0])
	}
}

// canonicalTrackIDs canonicalizes mp4 (optionally remapping track ids) and
// returns the audio and video track ids read from the resulting init catalog.
func canonicalTrackIDs(t *testing.T, eng *muxl.WASMEngine, mp4 []byte, remap map[uint32]uint32) (audio, video []uint32) {
	t.Helper()
	var opts []muxl.CanonicalizeOption
	if remap != nil {
		opts = append(opts, muxl.WithTrackRemap(remap))
	}
	canon, err := eng.Canonicalize(context.Background(), mp4, opts...)
	if err != nil {
		t.Fatalf("Canonicalize: %v", err)
	}
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SegmentEvents(context.Background(), bytes.NewReader(canon), events)
	})
	if err != nil {
		t.Fatalf("SegmentEvents: %v", err)
	}
	for _, ev := range events {
		if ev.Type != "init" || ev.Catalog == nil {
			continue
		}
		if ev.Catalog.Audio != nil {
			for _, c := range ev.Catalog.Audio.Renditions {
				audio = append(audio, c.TrackID())
			}
		}
		if ev.Catalog.Video != nil {
			for _, c := range ev.Catalog.Video.Renditions {
				video = append(video, c.TrackID())
			}
		}
	}
	return audio, video
}

func TestSegmentEventsUnsigned(t *testing.T) {
	eng := newEngine(t)
	frag := readFile(t, fixtureFmp4)
	events, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SegmentEvents(context.Background(), bytes.NewReader(frag), events)
	})
	if err != nil {
		t.Fatalf("SegmentEvents: %v", err)
	}

	var inits, segs int
	for _, ev := range events {
		switch ev.Type {
		case "init":
			inits++
			if ev.Catalog == nil {
				t.Error("init event missing catalog")
			}
		case "segment":
			segs++
			if len(ev.Tracks) == 0 {
				t.Errorf("segment %d has no tracks", ev.Number)
			}
		}
	}
	if inits != 1 {
		t.Errorf("want exactly 1 init event, got %d", inits)
	}
	if segs < 2 {
		t.Errorf("want >= 2 segment events, got %d", segs)
	}
}

func TestSignVerifyWrap(t *testing.T) {
	eng := newEngine(t)
	m4s := signedM4sStream(t, eng)

	// Verify: each canonical segment validates standalone.
	out, err := eng.Verify(context.Background(), bytes.NewReader(m4s))
	if err != nil {
		t.Fatalf("Verify: %v", err)
	}
	var doc verifyDoc
	if err := json.Unmarshal([]byte(out), &doc); err != nil {
		t.Fatalf("verify output is not JSON: %v\n%s", err, out)
	}
	if len(doc.Segments) < 2 {
		t.Fatalf("want >= 2 verified segments (video+audio), got %d", len(doc.Segments))
	}
	for i, seg := range doc.Segments {
		if !strings.Contains(seg.Cert, "BEGIN CERTIFICATE") {
			t.Errorf("segment %d missing PEM cert", i)
		}
		if seg.ValidationState == "Invalid" {
			t.Errorf("segment %d did not validate", i)
		}
	}

	// Wrap: bare .m4s -> playable fMP4 with the segment bytes carried verbatim.
	var wrapped bytes.Buffer
	if err := eng.Wrap(context.Background(), bytes.NewReader(m4s), "fmp4", &wrapped); err != nil {
		t.Fatalf("Wrap: %v", err)
	}
	if wrapped.Len() <= len(m4s) {
		t.Errorf("wrapped fMP4 (%d) should be larger than bare segments (%d)", wrapped.Len(), len(m4s))
	}
	if !bytes.Contains(wrapped.Bytes()[:64], []byte("ftyp")) {
		t.Errorf("wrapped output should start with an ftyp box")
	}
	if !bytes.Contains(wrapped.Bytes(), m4s) {
		t.Errorf("wrapped fMP4 must contain the verbatim signed segment bytes")
	}

	var initSeg bytes.Buffer
	if err := eng.WrapInit(context.Background(), bytes.NewReader(m4s), &initSeg); err != nil {
		t.Fatalf("WrapInit: %v", err)
	}
	if initSeg.Len() == 0 || initSeg.Len() >= wrapped.Len() {
		t.Errorf("init-only (%d) should be non-empty and smaller than full wrap (%d)", initSeg.Len(), wrapped.Len())
	}
}

func TestSignTranscode(t *testing.T) {
	eng := newEngine(t)

	// Source: a single fully-signed canonical segment (the parent ingredient,
	// with its own provenance) — one GoP/track, as a real transcode consumes.
	source := firstSignedSegment(t, eng)

	// Output: an unsigned canonical segment — stand-in for the transcoded result.
	frag := readFile(t, fixtureFmp4)
	segEvents, err := collectEvents(func(events chan<- *muxl.Event) error {
		return eng.SegmentEvents(context.Background(), bytes.NewReader(frag), events)
	})
	if err != nil {
		t.Fatalf("SegmentEvents: %v", err)
	}
	output := firstTrackBytes(segEvents, "segment")
	if len(output) == 0 {
		t.Fatal("no unsigned canonical output segment produced")
	}

	manifest := []byte(fmt.Sprintf(`{
		"title": "muxl-go transcode output",
		"assertions": [
			{"label": "c2pa.actions", "data": {"actions": [
				{"action": "c2pa.opened",     "parameters": {"org.cai.ingredientIds": [%q]}},
				{"action": "c2pa.transcoded", "parameters": {"org.cai.ingredientIds": [%q]}}
			]}}
		]
	}`, muxl.TranscodeIngredientLabel, muxl.TranscodeIngredientLabel))

	signed, err := eng.SignTranscode(context.Background(), muxl.TranscodeInput{
		Output:   output,
		Source:   source,
		CertPEM:  readFile(t, certPath),
		KeyPEM:   readFile(t, keyPath),
		Manifest: manifest,
	})
	if err != nil {
		t.Fatalf("SignTranscode: %v", err)
	}
	if len(signed) <= len(output) {
		t.Fatalf("signed output (%d) should be larger than unsigned (%d)", len(signed), len(output))
	}

	// The signed output's manifest must name a parentOf ingredient and carry a
	// c2pa.transcoded action.
	out, err := eng.Verify(context.Background(), bytes.NewReader(signed))
	if err != nil {
		t.Fatalf("Verify signed transcode: %v", err)
	}
	var doc verifyDoc
	if err := json.Unmarshal([]byte(out), &doc); err != nil {
		t.Fatalf("verify output is not JSON: %v\n%s", err, out)
	}
	if len(doc.Segments) != 1 {
		t.Fatalf("want exactly 1 verified segment, got %d", len(doc.Segments))
	}
	seg := doc.Segments[0]
	if seg.ValidationState == "Invalid" {
		t.Errorf("signed transcode did not validate: %s", out)
	}

	parents := 0
	for _, ing := range seg.Manifest.Ingredients {
		if ing.Relationship == "parentOf" {
			parents++
		}
	}
	if parents != 1 {
		t.Errorf("want exactly 1 parentOf ingredient, got %d\n%s", parents, out)
	}

	if !hasAction(seg.Manifest, "c2pa.transcoded") {
		t.Errorf("manifest missing a c2pa.transcoded action\n%s", out)
	}
}

func TestSignerAdapters(t *testing.T) {
	// SignerToCallback: a stdlib ECDSA P-256 signer returns DER; the adapter
	// must convert to a fixed 64-byte r‖s.
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	cb := muxl.SignerToCallback(key, 32)
	sig, err := cb([]byte("provenance"))
	if err != nil {
		t.Fatalf("SignerToCallback: %v", err)
	}
	if len(sig) != 64 {
		t.Errorf("want 64-byte r‖s from P-256, got %d", len(sig))
	}

	// RawSignerToCallback: 65-byte r‖s‖v (Ethereum keystore shape) -> 64-byte
	// r‖s; 64-byte passes through; wrong lengths error.
	raw := muxl.RawSignerToCallback(func(digest []byte) ([]byte, error) {
		return make([]byte, 65), nil
	}, 32)
	if out, err := raw([]byte("x")); err != nil || len(out) != 64 {
		t.Errorf("raw 65->64: got len=%d err=%v", len(out), err)
	}
	raw64 := muxl.RawSignerToCallback(func(digest []byte) ([]byte, error) {
		return make([]byte, 64), nil
	}, 32)
	if out, err := raw64([]byte("x")); err != nil || len(out) != 64 {
		t.Errorf("raw 64 passthrough: got len=%d err=%v", len(out), err)
	}
	rawBad := muxl.RawSignerToCallback(func(digest []byte) ([]byte, error) {
		return make([]byte, 33), nil
	}, 32)
	if _, err := rawBad([]byte("x")); err == nil {
		t.Error("raw adapter should reject a bogus signature length")
	}
}

func TestRequireOneSigner(t *testing.T) {
	eng := newEngine(t)
	// Neither key nor callback set -> error before touching wasm.
	_, err := eng.SignTranscode(context.Background(), muxl.TranscodeInput{
		Output: []byte("x"), Source: []byte("y"), CertPEM: []byte("z"),
	})
	if err == nil {
		t.Error("SignTranscode must require exactly one signing backend")
	}
}

// --- verify JSON shapes ---

type verifyDoc struct {
	Segments []struct {
		Manifest        manifestDoc `json:"manifest"`
		Cert            string      `json:"cert"`
		ValidationState string      `json:"validation_state"`
	} `json:"segments"`
}

type manifestDoc struct {
	Ingredients []struct {
		Relationship string `json:"relationship"`
	} `json:"ingredients"`
	Assertions []struct {
		Label string `json:"label"`
		Data  struct {
			Actions []struct {
				Action string `json:"action"`
			} `json:"actions"`
		} `json:"data"`
	} `json:"assertions"`
}

func hasAction(m manifestDoc, action string) bool {
	for _, a := range m.Assertions {
		if !strings.HasPrefix(a.Label, "c2pa.actions") {
			continue
		}
		for _, act := range a.Data.Actions {
			if act.Action == action {
				return true
			}
		}
	}
	return false
}
