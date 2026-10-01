package muxl

import (
	"context"
	"encoding/json"

	"github.com/tetratelabs/wazero/api"
)

// TextRequest describes the GoP's [StartMs, EndMs) absolute media span.
// MaxTrackID is the highest input track ID; use MaxTrackID+1 for a new track.
type TextRequest struct {
	StartMs    uint64 `json:"startMs"`
	EndMs      uint64 `json:"endMs"`
	MaxTrackID uint32 `json:"maxTrackId"`
}

// TextAttachment declares new tracks and supplies cues for this GoP.
// Omitted previously declared tracks still receive an empty segment.
type TextAttachment struct {
	Tracks []TextTrackAttachment `json:"tracks"`
}

// TextTrackAttachment carries a stable track configuration and absolute cues.
type TextTrackAttachment struct {
	TextTrack
	Cues []TextCue `json:"cues,omitempty"`
}

type textFetcher struct {
	fn      func(context.Context, TextRequest) (*TextAttachment, error)
	request TextRequest
	pending []byte
}

// A size retry must not invoke a blocking callback twice or consume cues twice.
func (e *WASMEngine) hostGetText(ctx context.Context, mod api.Module, start, end uint64, maxTrackID, outPtr, outMax uint32) uint32 {
	v, ok := e.textFetchers.Load(mod.Name())
	if !ok {
		return 0
	}
	f := v.(*textFetcher)
	req := TextRequest{StartMs: start, EndMs: end, MaxTrackID: maxTrackID}
	if f.pending == nil || f.request != req {
		attachment, err := f.fn(ctx, req)
		if err != nil {
			e.logger.ErrorContext(ctx, "muxl host_get_text: callback failed; attaching gaps", "error", err)
			f.pending = nil
			return hostGetManifestErr
		}
		if attachment == nil {
			return 0
		}
		f.pending, err = json.Marshal(attachment)
		if err != nil {
			return hostGetManifestErr
		}
		f.request = req
	}
	n := uint32(len(f.pending))
	if n > outMax {
		return n
	}
	if !mod.Memory().Write(outPtr, f.pending) {
		return hostGetManifestErr
	}
	f.pending = nil
	return n
}
