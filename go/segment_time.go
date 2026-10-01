package muxl

import (
	"context"
	"math"
	"time"

	"github.com/tetratelabs/wazero/api"
)

type signerCallbacks struct {
	text        func(context.Context, TextRequest) (*TextAttachment, error)
	segmentTime func(uint64) time.Time
}

func (e *WASMEngine) hostGetSegmentTime(_ context.Context, mod api.Module, startMs uint64) int64 {
	fn, ok := e.segmentTimes.Load(mod.Name())
	if !ok {
		return math.MinInt64
	}
	return fn.(func(uint64) time.Time)(startMs).UnixMilli()
}
