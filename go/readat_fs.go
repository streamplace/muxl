package muxl

import (
	"errors"
	"io"
	"io/fs"
	"time"
)

// readAtFS presents a single io.ReaderAt to the wasm sandbox as a one-file
// read-only filesystem, so muxl's random-access paths can address bytes inside
// it without the host first materializing them.
//
// This is what makes "extract one GoP from a multi-gigabyte blob" cheap over
// storage the guest can't reach: wazero implements the guest's pread/seek
// against whatever the fs.File provides (see readAtFile), so a read inside the
// sandbox becomes one ReadAt on the host — an S3 range GET for a blob store, a
// pread for a local file. Contrast WithFSMount over an in-memory fs.FS, which
// would require holding the whole blob in RAM.
type readAtFS struct {
	name string
	src  io.ReaderAt
	size int64
}

func (f readAtFS) Open(name string) (fs.File, error) {
	switch name {
	// wazero resolves the mount root before the file, so "." must open as a
	// directory or the mount itself looks nonexistent.
	case ".":
		return &readAtDir{fs: f}, nil
	case f.name:
		return &readAtFile{fs: f}, nil
	default:
		return nil, &fs.PathError{Op: "open", Path: name, Err: fs.ErrNotExist}
	}
}

// readAtDir is the mount root: a directory holding exactly one file.
type readAtDir struct {
	fs   readAtFS
	read bool
}

func (d *readAtDir) Stat() (fs.FileInfo, error) { return readAtDirInfo{d.fs}, nil }
func (d *readAtDir) Close() error               { return nil }

func (d *readAtDir) Read([]byte) (int, error) {
	return 0, &fs.PathError{Op: "read", Path: ".", Err: errors.New("is a directory")}
}

func (d *readAtDir) ReadDir(n int) ([]fs.DirEntry, error) {
	if d.read {
		if n <= 0 {
			return nil, nil
		}
		return nil, io.EOF
	}
	d.read = true
	return []fs.DirEntry{fs.FileInfoToDirEntry(readAtInfo{d.fs})}, nil
}

type readAtDirInfo struct{ fs readAtFS }

func (i readAtDirInfo) Name() string       { return "." }
func (i readAtDirInfo) Size() int64        { return 0 }
func (i readAtDirInfo) Mode() fs.FileMode  { return fs.ModeDir | 0o555 }
func (i readAtDirInfo) ModTime() time.Time { return time.Time{} }
func (i readAtDirInfo) IsDir() bool        { return true }
func (i readAtDirInfo) Sys() any           { return nil }

// readAtFile is the opened handle. It implements io.ReaderAt and io.Seeker in
// addition to fs.File: wazero prefers ReaderAt for the guest's pread and falls
// back to Seek+Read, and muxl's own FileReadAt uses seek+read on wasip1 (where
// positioned reads are still nightly-only), so both must work.
type readAtFile struct {
	fs  readAtFS
	pos int64
}

func (r *readAtFile) Stat() (fs.FileInfo, error) { return readAtInfo{r.fs}, nil }
func (r *readAtFile) Close() error               { return nil }

func (r *readAtFile) Read(p []byte) (int, error) {
	if r.pos >= r.fs.size {
		return 0, io.EOF
	}
	if rem := r.fs.size - r.pos; int64(len(p)) > rem {
		p = p[:rem]
	}
	n, err := r.fs.src.ReadAt(p, r.pos)
	r.pos += int64(n)
	return n, err
}

func (r *readAtFile) ReadAt(p []byte, off int64) (int, error) {
	if off >= r.fs.size {
		return 0, io.EOF
	}
	if rem := r.fs.size - off; int64(len(p)) > rem {
		p = p[:rem]
		n, err := r.fs.src.ReadAt(p, off)
		if err == nil {
			err = io.EOF
		}
		return n, err
	}
	return r.fs.src.ReadAt(p, off)
}

func (r *readAtFile) Seek(offset int64, whence int) (int64, error) {
	var abs int64
	switch whence {
	case io.SeekStart:
		abs = offset
	case io.SeekCurrent:
		abs = r.pos + offset
	case io.SeekEnd:
		abs = r.fs.size + offset
	default:
		return 0, errors.New("muxl: invalid whence")
	}
	if abs < 0 {
		return 0, errors.New("muxl: negative seek position")
	}
	r.pos = abs
	return abs, nil
}

type readAtInfo struct{ fs readAtFS }

func (i readAtInfo) Name() string       { return i.fs.name }
func (i readAtInfo) Size() int64        { return i.fs.size }
func (i readAtInfo) Mode() fs.FileMode  { return 0o444 }
func (i readAtInfo) ModTime() time.Time { return time.Time{} }
func (i readAtInfo) IsDir() bool        { return false }
func (i readAtInfo) Sys() any           { return nil }
