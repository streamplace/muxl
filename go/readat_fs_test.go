package muxl

import (
	"context"
	"io"
	"os"
	"sync/atomic"
	"testing"
)

// TestReadSegmentsRealBlob exercises ReadSegments against a finalized flat-MP4
// VOD blob on disk: fragment-relative offsets in, verbatim canonical segments
// out, with only the requested bytes read.
func TestReadSegmentsRealBlob(t *testing.T) {
	path := os.Getenv("MUXL_TEST_BLOB")
	if path == "" {
		t.Skip("set MUXL_TEST_BLOB to a flat-MP4 VOD blob")
	}
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		t.Fatal(err)
	}

	ctx := context.Background()
	e, err := NewWASM(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer e.Close(ctx)

	// Offsets/sizes taken from the blob's own metafile index.
	for _, tc := range []struct {
		name       string
		offset     int64
		count      int
		wantLength int
	}{
		{"first video segment", 0, 1, 791951},
		{"midpoint video segment", 794624166, 1, 740326},
		{"first GoP, all three tracks", 0, 3, 893448},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := e.ReadSegments(ctx, f, info.Size(), tc.offset, tc.count)
			if err != nil {
				t.Fatalf("ReadSegments: %v", err)
			}
			if len(got) != tc.wantLength {
				t.Errorf("got %d bytes, want %d", len(got), tc.wantLength)
			}
			// Canonical segments begin with a uuid box (c2pa prefix or MUXL).
			if len(got) < 8 || string(got[4:8]) != "uuid" {
				t.Errorf("expected a leading uuid box, got %q", got[4:min(8, len(got))])
			}
		})
	}

	// An absolute offset (fragment offset + flat header) must fail loudly
	// rather than return the garbage that silently broke thumbnailing.
	t.Run("absolute offset rejected", func(t *testing.T) {
		if _, err := e.ReadSegments(ctx, f, info.Size(), 794624166+5112085, 1); err == nil {
			t.Error("expected an error for an absolute (non-fragment-relative) offset")
		}
	})

	// The whole point of the mount: pulling one GoP from the middle of a
	// multi-gigabyte blob must read that GoP, not the blob.
	t.Run("reads only the requested segment", func(t *testing.T) {
		counter := &countingReaderAt{inner: f}
		got, err := e.ReadSegments(ctx, counter, info.Size(), 794624166, 1)
		if err != nil {
			t.Fatalf("ReadSegments: %v", err)
		}
		read := counter.bytes.Load()
		t.Logf("blob %d bytes; returned %d; host read %d", info.Size(), len(got), read)
		if budget := int64(len(got)) * 4; read > budget {
			t.Errorf("read %d bytes to return %d — expected under %d", read, len(got), budget)
		}
	})
}

// countingReaderAt tallies bytes actually pulled from the underlying blob.
type countingReaderAt struct {
	inner io.ReaderAt
	bytes atomic.Int64
}

func (c *countingReaderAt) ReadAt(p []byte, off int64) (int, error) {
	n, err := c.inner.ReadAt(p, off)
	c.bytes.Add(int64(n))
	return n, err
}
