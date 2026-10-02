package muxl

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"strconv"
	"testing/fstest"

	"github.com/hyphacoop/go-dasl/drisl"
	"github.com/tetratelabs/wazero"
)

// TextEngine adds timed-text operations without extending the existing Engine
// interface, so alternate Engine implementations remain source-compatible.
type TextEngine interface {
	Engine
	TextTracks(ctx context.Context, input io.Reader) ([]TextTrack, error)
	ReadTextCues(ctx context.Context, input io.Reader, trackID uint32) ([]TextCue, error)
	AddTextTrack(ctx context.Context, segment []byte, track TextTrack, cues []TextCue) ([]byte, error)
	SignTextRuns(ctx context.Context, req TextRequest, tracks []TextTrackAttachment, in SignerInput) (map[uint32][]byte, error)
}

var _ TextEngine = (*WASMEngine)(nil)

// TextTrack identifies a WebVTT track. TrackID must be unused and nonzero when
// adding a track. Language is a BCP 47 tag; Label is an optional player label.
type TextTrack struct {
	TrackID  uint32 `json:"trackId"`
	Language string `json:"language"`
	Label    string `json:"label,omitempty"`
}

// TextCue uses absolute stream milliseconds, with an exclusive End. Cues may
// overlap; canonical samples split overlaps and cover gaps with empty samples.
// A cue crossing a GoP boundary is clipped when added, and reconstructed when
// contiguous GoPs are read together. Adjacent identical cues are indistinguishable
// unless they have different IDs.
type TextCue struct {
	Start    uint64 `json:"start"`
	End      uint64 `json:"end"`
	Text     string `json:"text"`
	ID       string `json:"id,omitempty"`
	Settings string `json:"settings,omitempty"`
}

// TextTracks enumerates WebVTT tracks in canonical segments, concatenated VOD
// segments, or flat/fragmented MP4 files, including their language and label.
func (e *WASMEngine) TextTracks(ctx context.Context, input io.Reader) ([]TextTrack, error) {
	var out bytes.Buffer
	if err := e.runWith(ctx, []string{"muxl", "text", "tracks"}, nil, false, input, &out, nil, nil, nil, nil, nil); err != nil {
		return nil, err
	}
	var tracks []TextTrack
	if err := json.Unmarshal(out.Bytes(), &tracks); err != nil {
		return nil, fmt.Errorf("muxl: decoding text tracks: %w", err)
	}
	return tracks, nil
}

// ReadTextCues reads cues for trackID, coalescing pieces split by overlaps and
// GoP boundaries. Timestamps stay on the absolute stream timeline.
func (e *WASMEngine) ReadTextCues(ctx context.Context, input io.Reader, trackID uint32) ([]TextCue, error) {
	var out bytes.Buffer
	args := []string{"muxl", "text", "cues", "--track-id", strconv.FormatUint(uint64(trackID), 10)}
	if err := e.runWith(ctx, args, nil, false, input, &out, nil, nil, nil, nil, nil); err != nil {
		return nil, err
	}
	var cues []TextCue
	if err := json.Unmarshal(out.Bytes(), &cues); err != nil {
		return nil, fmt.Errorf("muxl: decoding text cues: %w", err)
	}
	return cues, nil
}

// AddTextTrack attaches a canonical WebVTT track to one unsigned GoP's
// canonical segment stream, before signing. The original AV bytes are unchanged.
// The first video track (otherwise audio) defines the span. Cues are clipped to
// that span; every instant is covered, including gaps. Signatures are rejected
// rather than invalidated. The result can be wrapped for SignSegment.
func (e *WASMEngine) AddTextTrack(ctx context.Context, segment []byte, track TextTrack, cues []TextCue) ([]byte, error) {
	data, err := json.Marshal(cues)
	if err != nil {
		return nil, err
	}
	if cues == nil {
		data = []byte("[]")
	}
	fsCfg := wazero.NewFSConfig().WithFSMount(fstest.MapFS{"cues.json": {Data: data}}, "/text")
	language := track.Language
	if language == "" {
		language = "und"
	}
	args := []string{"muxl", "text", "add", "--track-id", strconv.FormatUint(uint64(track.TrackID), 10), "--language", language, "--cues", "/text/cues.json"}
	if track.Label != "" {
		args = append(args, "--label", track.Label)
	}
	var out bytes.Buffer
	if err := e.runWith(ctx, args, fsCfg, false, bytes.NewReader(segment), &out, nil, nil, nil, nil, nil); err != nil {
		return nil, err
	}
	return out.Bytes(), nil
}

// SignTextRuns mints and signs one standalone WebVTT run per track for the
// GoP span req, without the GoP's AV bytes. Cues are clipped to the span;
// tracks without cues receive a gap-only run. IDs must be nonzero and unique.
// The caller replaces existing text runs and checks for non-text ID collisions
// when splicing these runs into a GoP in ascending numeric track order.
// TextFn is not used. Exactly one of in.KeyPEM and in.Sign must be set.
func (e *WASMEngine) SignTextRuns(ctx context.Context, req TextRequest, tracks []TextTrackAttachment, in SignerInput) (map[uint32][]byte, error) {
	if err := requireOneSigner(len(in.KeyPEM) > 0, in.Sign != nil); err != nil {
		return nil, err
	}
	if req.EndMs <= req.StartMs {
		return nil, fmt.Errorf("muxl: text range must have positive duration")
	}
	ids := make(map[uint32]struct{}, len(tracks))
	for _, track := range tracks {
		if track.TrackID == 0 {
			return nil, fmt.Errorf("muxl: text track id must be nonzero")
		}
		if _, exists := ids[track.TrackID]; exists {
			return nil, fmt.Errorf("muxl: duplicate text track id %d", track.TrackID)
		}
		ids[track.TrackID] = struct{}{}
	}
	input, err := json.Marshal(struct {
		TextRequest
		Tracks []TextTrackAttachment `json:"tracks,omitempty"`
	}{req, tracks})
	if err != nil {
		return nil, err
	}
	alg := in.Alg
	if alg == "" {
		alg = "es256k"
	}
	keysFS := fstest.MapFS{"cert.pem": {Data: in.CertPEM}}
	args := []string{"muxl", "sign-text-runs", "--cert", "/keys/cert.pem", "--alg", alg}
	if in.TrackManifestFn != nil {
		args = append(args, "--host-manifest")
	} else {
		keysFS["track.json"] = &fstest.MapFile{Data: in.TrackManifest}
		args = append(args, "--track-manifest", "/keys/track.json")
	}
	if len(in.KeyPEM) > 0 {
		keysFS["key.pem"] = &fstest.MapFile{Data: in.KeyPEM}
		args = append(args, "--key", "/keys/key.pem")
	} else {
		args = append(args, "--host-sign")
	}
	var out bytes.Buffer
	if err := e.runWith(ctx, args, wazero.NewFSConfig().WithFSMount(keysFS, "/keys"), true,
		bytes.NewReader(input), &out, in.Sign, manifestFetcher(in), nil, nil, nil,
		signerCallbacks{segmentTime: in.SegmentTimeFn}); err != nil {
		return nil, err
	}
	var encoded map[string][]byte
	if err := drisl.NewDecoder(&out).Decode(&encoded); err != nil {
		return nil, fmt.Errorf("muxl: decoding signed text runs: %w", err)
	}
	runs := make(map[uint32][]byte, len(encoded))
	for key, run := range encoded {
		id, err := strconv.ParseUint(key, 10, 32)
		if err != nil {
			return nil, fmt.Errorf("muxl: invalid text track id %q: %w", key, err)
		}
		runs[uint32(id)] = run
	}
	return runs, nil
}
