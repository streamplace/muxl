package muxl_test

import (
    "bytes"
    "context"
    "encoding/base32"
    "reflect"
    "strings"
    "testing"

    muxl "github.com/streamplace/muxl/go"
    "lukechampine.com/blake3"
)

func textCID(data []byte) string {
    sum := blake3.Sum256(data)
    raw := append([]byte{1,0x55,0x1e,0x20},sum[:]...)
    return "b"+strings.ToLower(base32.StdEncoding.WithPadding(base32.NoPadding).EncodeToString(raw))
}

func firstUnsignedGoP(t *testing.T, e *muxl.WASMEngine) []byte {
    t.Helper()
    events,err := collectEvents(func(ch chan<- *muxl.Event) error { return e.SegmentEvents(context.Background(),bytes.NewReader(readFile(t,fixtureFmp4)),ch) })
    if err != nil { t.Fatal(err) }
    for _,ev := range events { if ev.Type=="segment" {
        var out []byte
        // Fixture has tracks 1 and 2; preserve numeric track order.
        out=append(out,ev.Tracks["1"]...);out=append(out,ev.Tracks["2"]...)
        return out
    } }
    t.Fatal("missing fixture GoP");return nil
}

func TestTextTwoCueCanonicalRoundTrip(t *testing.T) {
    ctx:=context.Background();e:=newEngine(t)
    original:=firstUnsignedGoP(t,e)
    track:=muxl.TextTrack{TrackID:3,Language:"en-US",Label:"English"}
    want:=[]muxl.TextCue{{Start:100,End:400,Text:"Hello <b>world</b>",ID:"one",Settings:"align:start"},{Start:600,End:900,Text:"Second cue\nnext line",ID:"two"}}
    added,err:=e.AddTextTrack(ctx,original,track,want);if err!=nil {t.Fatal(err)}
    if !bytes.HasPrefix(added,original) {t.Fatal("adding text changed AV bytes")}
    tracks,err:=e.TextTracks(ctx,bytes.NewReader(added));if err!=nil {t.Fatal(err)}
    if !reflect.DeepEqual(tracks,[]muxl.TextTrack{track}) {t.Fatalf("metadata: got %#v",tracks)}
    got,err:=e.ReadTextCues(ctx,bytes.NewReader(added),3);if err!=nil {t.Fatal(err)}
    if !reflect.DeepEqual(got,want) {t.Fatalf("cues: got %#v want %#v",got,want)}
    var wrapped bytes.Buffer
    if err=e.Wrap(ctx,bytes.NewReader(added),"fmp4",&wrapped);err!=nil {t.Fatal(err)}
    canonical,err:=e.Canonicalize(ctx,wrapped.Bytes());if err!=nil {t.Fatal(err)}
    again,err:=e.Canonicalize(ctx,canonical);if err!=nil {t.Fatal(err)}
    if textCID(canonical)!=textCID(again) {t.Fatalf("unstable CID: %s != %s",textCID(canonical),textCID(again))}
    got,err=e.ReadTextCues(ctx,bytes.NewReader(canonical),3);if err!=nil {t.Fatal(err)}
    if !reflect.DeepEqual(got,want) {t.Fatalf("canonical cues: got %#v",got)}
    reordered,err:=e.AddTextTrack(ctx,original,track,[]muxl.TextCue{want[1],want[0]});if err!=nil {t.Fatal(err)}
    if textCID(added)!=textCID(reordered) {t.Fatal("cue input order changed CID")}
    t.Logf("canonical CID %s; text track CID %s",textCID(canonical),textCID(added[len(original):]))
    if _,err=e.AddTextTrack(ctx,added,track,want);err==nil {t.Fatal("duplicate track id accepted")}
    var flat bytes.Buffer
    if err=e.Wrap(ctx,bytes.NewReader(added),"flat",&flat);err!=nil {t.Fatal(err)}
    got,err=e.ReadTextCues(ctx,bytes.NewReader(flat.Bytes()),3);if err!=nil {t.Fatal(err)}
    if !reflect.DeepEqual(got,want) {t.Fatalf("flat VOD cues: %#v",got)}
}
