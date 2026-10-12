package muxl

import (
	"reflect"
	"testing"
)

func TestTrackMetaArgsSorted(t *testing.T) {
	meta := map[uint32]TrackMeta{
		9: {AlternateGroup: 2, Label: "Music: 音楽"},
		3: {},
	}
	got := appendTrackMetaArgs([]string{"muxl"}, meta)
	want := []string{"muxl", "--track-meta", "3:0:", "--track-meta", "9:2:Music: 音楽"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("args = %q, want %q", got, want)
	}
}
