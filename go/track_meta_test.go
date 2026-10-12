package muxl_test

import (
	"bytes"
	"context"
	"crypto/sha256"
	"fmt"
	"testing"

	"github.com/hyphacoop/go-dasl/drisl"
	muxl "github.com/streamplace/muxl/go"
)

func checkAudioMeta(t *testing.T, c *muxl.Catalog, id, group uint32, label string) {
	t.Helper()
	if c == nil || c.Audio == nil {
		t.Fatal("missing audio catalog")
	}
	for _, audio := range c.Audio.Renditions {
		if audio.TrackID() == id {
			if audio.AlternateGroup != group || audio.Label != label {
				t.Fatalf("track %d metadata: got group=%d label=%q, want group=%d label=%q", id, audio.AlternateGroup, audio.Label, group, label)
			}
			return
		}
	}
	t.Fatalf("missing audio track %d", id)
}

func TestDefaultTrackMetadataBytesUnchanged(t *testing.T) {
	// SHA-256 goldens captured from main/v0.3.6 before this change. Segment
	// hashes cover catalog UUIDs as well as frames, thus also preserving CIDs.
	eng := newEngine(t)
	in := readFile(t, fixtureFmp4)
	want := map[string]string{
		"init": "fe6c5c1ed798a29acd8921167d72e6feaf01162a2d47085e02b47ec40cc85971",
		"1:1":  "62cc4b110b16b96479a830cecf4a2c5222362e82338982efdd14e0d77b63d1cc",
		"2:1":  "a2abe9936d9e1b1369931ba2a68e460aa277fc2f82ce72cecc5a19b9e81681e9",
		"1:2":  "6a44a9d4be89d4f043a16cd1ccadcb8124378832ad8fbe310894c3dc2babc030",
		"2:2":  "25c5652d809577f36e2dfbcf5819743d659df46800eb080b49ad786a48c2a4ee",
	}
	events, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.SegmentEvents(context.Background(), bytes.NewReader(in), ch)
	})
	if err != nil {
		t.Fatal(err)
	}
	check := func(key string, data []byte) {
		t.Helper()
		got := fmt.Sprintf("%x", sha256.Sum256(data))
		if got != want[key] {
			t.Fatalf("%s SHA-256 = %s, want %s", key, got, want[key])
		}
		delete(want, key)
	}
	var number uint32
	for _, ev := range events {
		if ev.Type == "init" {
			check("init", ev.Data)
			checkAudioMeta(t, ev.Catalog, 2, 0, "")
		}
		if ev.Type == "segment" {
			number++
		}
		for id, data := range ev.Tracks {
			check(fmt.Sprintf("%d:%s", number, id), data)
		}
	}
	if len(want) != 0 {
		t.Fatalf("missing golden outputs: %v", want)
	}
	canon, err := eng.Canonicalize(context.Background(), in)
	if err != nil {
		t.Fatal(err)
	}
	if got := fmt.Sprintf("%x", sha256.Sum256(canon)); got != "42995096a9505b5155ed30aa4e40dc81d911428aaf9dfcf2708d9fc0c8d760f3" {
		t.Fatalf("canonical output changed: %s", got)
	}
}

func TestTrackMetaOptionsAndRoundtrips(t *testing.T) {
	eng := newEngine(t)
	ctx := context.Background()
	in := readFile(t, fixtureFmp4)
	meta := map[uint32]muxl.TrackMeta{9: {AlternateGroup: 2, Label: "Music: 音楽"}}
	canon, err := eng.Canonicalize(ctx, in, muxl.WithTrackRemap(map[uint32]uint32{2: 9}), muxl.WithTrackMeta(meta))
	if err != nil {
		t.Fatal(err)
	}
	events, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.SegmentEvents(ctx, bytes.NewReader(in), ch,
			muxl.WithSegmentTrackRemap(map[uint32]uint32{2: 9}), muxl.WithSegmentTrackMeta(meta))
	})
	if err != nil {
		t.Fatal(err)
	}
	var stream bytes.Buffer
	for _, ev := range events {
		if ev.Type == "init" {
			checkAudioMeta(t, ev.Catalog, 9, 2, "Music: 音楽")
			stream.Write(ev.Data)
		}
		for _, id := range []string{"1", "9"} {
			if data, ok := ev.Tracks[id]; ok {
				stream.Write(data)
				if id == "9" {
					mf, err := eng.Metafile(ctx, data)
					if err != nil {
						t.Fatal(err)
					}
					var decoded muxl.Event
					if err := drisl.NewDecoder(bytes.NewReader(mf)).Decode(&decoded); err != nil {
						t.Fatal(err)
					}
					checkAudioMeta(t, decoded.Catalog, 9, 2, "Music: 音楽")
				}
			}
		}
	}
	if !bytes.Equal(stream.Bytes(), canon) {
		t.Fatal("streaming and Canonicalize bytes differ")
	}
	readBack := func(data []byte) {
		t.Helper()
		events, err := collectEvents(func(ch chan<- *muxl.Event) error {
			return eng.SegmentEvents(ctx, bytes.NewReader(data), ch, muxl.WithSegmentSourceTrackMeta())
		})
		if err != nil {
			t.Fatal(err)
		}
		if len(events) == 0 || events[0].Type != "init" {
			t.Fatal("missing init")
		}
		checkAudioMeta(t, events[0].Catalog, 9, 2, "Music: 音楽")
	}
	readBack(canon)
	for _, format := range []string{"fmp4", "flat"} {
		var wrapped bytes.Buffer
		if err := eng.Wrap(ctx, bytes.NewReader(canon), format, &wrapped); err != nil {
			t.Fatal(err)
		}
		unwrapped, err := collectEvents(func(ch chan<- *muxl.Event) error {
			return eng.UnwrapEvents(ctx, bytes.NewReader(wrapped.Bytes()), ch)
		})
		if err != nil {
			t.Fatal(err)
		}
		checkAudioMeta(t, unwrapped[0].Catalog, 9, 2, "Music: 音楽")
		readBack(unwrapped[0].Data)
		if format == "fmp4" {
			readBack(wrapped.Bytes())
		}
	}
	var init bytes.Buffer
	if err := eng.WrapInit(ctx, bytes.NewReader(canon), &init); err != nil {
		t.Fatal(err)
	}
	readBack(init.Bytes())
	var metafiles, header bytes.Buffer
	if err := eng.Metafiles(ctx, bytes.NewReader(canon), &metafiles); err != nil {
		t.Fatal(err)
	}
	if err := eng.SynthesizeFlatHeader(ctx, bytes.NewReader(metafiles.Bytes()), &header); err != nil {
		t.Fatal(err)
	}
	// Flat synthesis has no moofs; UnwrapEvents uses the verbatim body catalogs.
	header.Write(canon[len(events[0].Data):])
	unwrapped, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.UnwrapEvents(ctx, bytes.NewReader(header.Bytes()), ch)
	})
	if err != nil {
		t.Fatal(err)
	}
	checkAudioMeta(t, unwrapped[0].Catalog, 9, 2, "Music: 音楽")

	// Explicit zero/empty fields override and clear opt-in source values.
	cleared, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.SegmentEvents(ctx, bytes.NewReader(canon), ch,
			muxl.WithSegmentSourceTrackMeta(), muxl.WithSegmentTrackMeta(map[uint32]muxl.TrackMeta{9: {}}))
	})
	if err != nil {
		t.Fatal(err)
	}
	checkAudioMeta(t, cleared[0].Catalog, 9, 0, "")
	for _, id := range []uint32{1, 404} {
		bad := map[uint32]muxl.TrackMeta{id: {Label: "invalid"}}
		if _, err := eng.Canonicalize(ctx, in, muxl.WithTrackMeta(bad)); err == nil {
			t.Fatalf("accepted invalid track %d", id)
		}
		if _, err := collectEvents(func(ch chan<- *muxl.Event) error {
			return eng.SegmentEvents(ctx, bytes.NewReader(in), ch, muxl.WithSegmentTrackMeta(bad))
		}); err == nil {
			t.Fatalf("SegmentEvents accepted invalid track %d", id)
		}
	}
}

func TestSignerSourceTrackMeta(t *testing.T) {
	eng := newEngine(t)
	ctx := context.Background()
	in, err := eng.Canonicalize(ctx, readFile(t, fixtureFmp4), muxl.WithTrackMeta(map[uint32]muxl.TrackMeta{2: {AlternateGroup: 2, Label: "Music"}}))
	if err != nil {
		t.Fatal(err)
	}
	signer := signerInput(t)
	signer.SourceTrackMeta = true
	events, err := collectEvents(func(ch chan<- *muxl.Event) error {
		return eng.SignSegment(ctx, bytes.NewReader(in), signer, nil, nil, ch)
	})
	if err != nil {
		t.Fatal(err)
	}
	checkAudioMeta(t, events[0].Catalog, 2, 2, "Music")
	var signed int
	for _, ev := range events {
		if data, ok := ev.Tracks["2"]; ok {
			decoded, err := collectEvents(func(ch chan<- *muxl.Event) error {
				return eng.UnwrapEvents(ctx, bytes.NewReader(data), ch)
			})
			if err != nil {
				t.Fatal(err)
			}
			checkAudioMeta(t, decoded[0].Catalog, 2, 2, "Music")
			signed++
		}
	}
	if signed == 0 {
		t.Fatal("no signed audio segments")
	}
}
