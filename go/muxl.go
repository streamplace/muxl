// Package muxl is a Go library for MUXL — a deterministic, content-addressable
// subset of ISO-BMFF (MP4) — and its S2PA (C2PA-over-DID) signing and
// verification, exposed through a single [Engine] interface.
//
// The default engine, [NewWASM], runs the muxl toolchain compiled to
// WebAssembly under the pure-Go wazero runtime. Consumers get the full
// MUXL/S2PA feature set with zero native build steps — no Rust toolchain, no
// cgo, just `go get`. The muxl.wasm artifact is embedded in the package
// and committed to the repo, so building this library never shells out to
// cargo.
//
// The [Engine] interface exists so the WASM backend can be swapped, behind the
// same API, for a natively-linked Rust build later (for a throughput bump)
// without touching callers.
package muxl

import (
	"context"
	"io"
)

// Engine is the MUXL/S2PA backend: canonical segmentation, S2PA signing and
// verification, and presentation-format (fMP4 / flat MP4) synthesis over MUXL
// segments. [NewWASM] returns the default, zero-config implementation; a
// future natively-linked implementation can satisfy the same interface.
//
// Streaming methods deliver results on caller-supplied channels and never
// close them: the caller owns the channels and closes them once the method
// returns. Each call runs an independent wasm instance, so an Engine is safe
// for concurrent use.
type Engine interface {
	// SegmentEvents segments an fMP4 stream into per-GoP canonical MUXL
	// events — one init event then one segment event per GoP — unsigned.
	// Options (e.g. [WithSegmentTrackRemap]) tune the output.
	SegmentEvents(ctx context.Context, input io.Reader, events chan<- *Event, opts ...SegmentEventsOption) error

	// Canonicalize converts a flat or fragmented MP4 into a canonical MUXL
	// fMP4. Unlike SegmentEvents it accepts a faststart (flat) MP4 — handy for
	// turning an arbitrary transcoder output into MUXL before segmenting/signing.
	// Options (e.g. [WithTrackRemap]) tune the output.
	Canonicalize(ctx context.Context, mp4 []byte, opts ...CanonicalizeOption) ([]byte, error)

	// UnwrapEvents re-derives the per-track event stream from a stored MUXL
	// wrapper (bare .m4s, fMP4, or flat MP4). The per-track segment bytes —
	// and any C2PA/S2PA signature over them — are preserved verbatim.
	UnwrapEvents(ctx context.Context, input io.Reader, events chan<- *Event) error

	// SignSegment segments an fMP4 stream and S2PA-signs each canonical
	// segment in place. Bytes routed to seg are the signed
	// [c2pa-uuid][muxl-uuid][moof][mdat] per track; events carries the full
	// per-segment metadata. Exactly one of in.KeyPEM or in.Sign must be set.
	SignSegment(ctx context.Context, input io.Reader, in SignerInput, init, seg chan<- []byte, events chan<- *Event) error

	// SignTranscode signs in.Output — an unsigned canonical MUXL segment, the
	// result of transcoding — as a standalone asset that declares in.Source
	// (the canonical MUXL segment it was transcoded from) as a C2PA parentOf
	// ingredient, bound to a c2pa.transcoded action. Returns the signed
	// segment. Exactly one of in.KeyPEM or in.Sign must be set.
	SignTranscode(ctx context.Context, in TranscodeInput) ([]byte, error)

	// GenCert issues an S2PA leaf certificate for the secp256k1 identity in
	// in.PubKey, signing the certificate via in.Sign — the private key stays
	// with the caller. Returns a PEM cert chain for use as the CertPEM of
	// SignerInput / TranscodeInput. This lets a Livepeer orchestrator mint a
	// did:pkh cert bound to its on-chain key without the key leaving its
	// keystore.
	GenCert(ctx context.Context, in CertInput) ([]byte, error)

	// Verify validates the C2PA/S2PA signatures on a signed MUXL wrapper and
	// returns the per-segment manifest+cert+validation JSON document
	// ({"segments":[{track_id,manifest,cert,validation_results,validation_state}]}).
	// Cryptographic failures surface inside that document's validation fields;
	// Verify itself errors only on structural problems.
	Verify(ctx context.Context, input io.Reader) (string, error)

	// Wrap synthesizes a presentation MP4 from a MUXL wrapper. format is
	// "fmp4" (appendable: ftyp+moov(init) + verbatim segments) or "flat"
	// (finalized faststart); "" defaults to fmp4. The segment bytes and their
	// signatures pass through untouched — only the header is synthesized.
	Wrap(ctx context.Context, input io.Reader, format string, out io.Writer) error

	// WrapInit synthesizes only the per-stream init segment (ftyp+moov) — the
	// HLS EXT-X-MAP target — from a MUXL wrapper's embedded catalogs.
	WrapInit(ctx context.Context, input io.Reader, out io.Writer) error
}

// CanonicalizeOption configures [Engine.Canonicalize].
type CanonicalizeOption func(*canonicalizeConfig)

type canonicalizeConfig struct {
	trackRemap map[uint32]uint32
}

// WithTrackRemap reassigns track IDs in the canonical output: each source
// track id (key) is rewritten to a new id (value) in both the moov and every
// minted moof. Use it to mint a freshly canonicalized single-track segment —
// e.g. a transcoded audio rendition — at a chosen free id, so it can be
// concatenated alongside the tracks it derives from in one multi-track
// segment without a track-id collision. Source ids absent from the map are
// left as-is.
func WithTrackRemap(remap map[uint32]uint32) CanonicalizeOption {
	return func(c *canonicalizeConfig) { c.trackRemap = remap }
}

// SegmentEventsOption configures [Engine.SegmentEvents].
type SegmentEventsOption func(*segmentEventsConfig)

type segmentEventsConfig struct {
	trackRemap map[uint32]uint32
}

// WithSegmentTrackRemap relabels track IDs while segmenting: each source id
// (key) is minted at a new id (value) in the emitted catalog and every moof.
// Same semantics as [WithTrackRemap], applied in the streaming segmenter — so
// a transcoded rendition lands at a collision-free id without a second
// canonicalize pass. Source ids absent from the map are left as-is.
func WithSegmentTrackRemap(remap map[uint32]uint32) SegmentEventsOption {
	return func(c *segmentEventsConfig) { c.trackRemap = remap }
}

// SignerInput is the signing bundle for [Engine.SignSegment]. Exactly one of
// KeyPEM or Sign must be set:
//
//   - KeyPEM: a PKCS#8-PEM private key. With the WASM engine it is mounted
//     read-only into the sandbox and signing happens inside wasm. Use this for
//     software keys.
//   - Sign: a host-side callback that signs per c2pa's CallbackSigner
//     contract (for ECDSA: it receives the unhashed bytes and returns raw
//     r‖s). Use this for hardware- or keystore-backed signers whose key bytes
//     never leave the host — e.g. a Livepeer orchestrator's Ethereum keystore.
//     See [SignerToCallback].
//
// CertPEM is the cert chain (PEM, leaf first). TrackManifest/WrapperManifest
// are C2PA manifest JSON bodies. Alg defaults to "es256k" when empty.
//
// Dynamic manifests: SignSegment is a long-lived streaming call (one wasm
// invocation per RTMP session), so if the manifest depends on state that can
// change mid-stream (e.g. a livestream record's published flag), the static
// TrackManifest/WrapperManifest bytes freeze that state at connection time.
// Set TrackManifestFn / WrapperManifestFn instead: each is invoked from the
// wasm host once per GoP, so every segment is signed against a freshly built
// manifest. When either Fn is set, the static field of the same name is used
// only as a fallback if the other Fn is nil.
type SignerInput struct {
	CertPEM           []byte
	KeyPEM            []byte
	Sign              func(data []byte) ([]byte, error)
	TrackManifest     []byte
	TrackManifestFn   func() ([]byte, error)
	WrapperManifest   []byte
	WrapperManifestFn func() ([]byte, error)
	Alg               string
}

// TranscodeInput is the signing bundle for [Engine.SignTranscode]. Exactly one
// of KeyPEM or Sign must be set (same backends as [SignerInput]).
//
// Output is the unsigned canonical MUXL .m4s to sign (the transcoded result);
// Source is the canonical MUXL .m4s it was transcoded from, recorded as a
// parentOf ingredient. Manifest is the output's C2PA manifest JSON, which
// should carry a c2pa.transcoded action referencing the source ingredient.
type TranscodeInput struct {
	Output   []byte
	Source   []byte
	CertPEM  []byte
	KeyPEM   []byte
	Sign     func(data []byte) ([]byte, error)
	Manifest []byte
	Alg      string
}

// CertInput is the input bundle for [Engine.GenCert]: issue an S2PA leaf cert
// for the secp256k1 identity behind PubKey, signing the certificate's
// TBSCertificate via Sign. The private key never enters the engine.
type CertInput struct {
	// PubKey is the 65-byte uncompressed SEC1 public key (0x04 || X || Y).
	PubKey []byte
	// DID is the cert's commonName. Empty defaults to the did:key of PubKey;
	// a Livepeer orchestrator passes did:pkh:eip155:<chainId>:<address>.
	DID string
	// Org is an optional organizationName attribute (some C2PA libraries
	// expect a DN attribute beyond commonName).
	Org string
	// Sign signs the certificate: it receives the (unhashed) TBSCertificate
	// DER and must return the ECDSA-SHA256 signature as raw r‖s (or the
	// 65-byte r‖s‖v Ethereum form). Wrap a keystore with RawSignerToCallback.
	Sign func(data []byte) ([]byte, error)
}

// TranscodeIngredientLabel is the C2PA ingredient label that
// [Engine.SignTranscode] assigns to the source segment. A
// [TranscodeInput.Manifest] references the source by listing this label in an
// action's "org.cai.ingredientIds" parameter — typically on a c2pa.transcoded
// action, e.g.:
//
//	{"label":"c2pa.actions","data":{"actions":[
//	  {"action":"c2pa.opened",    "parameters":{"org.cai.ingredientIds":["muxl.source"]}},
//	  {"action":"c2pa.transcoded","parameters":{"org.cai.ingredientIds":["muxl.source"]}}
//	]}}
//
// It must match muxl's TRANSCODE_INGREDIENT_LABEL.
const TranscodeIngredientLabel = "muxl.source"

// Event is one event from the muxl wasm's DRISL output stream. The wire format
// is the Rust side's tagged union; fields not relevant to a given Type are
// left zero. Type is "init", "segment", or "signed-segment".
type Event struct {
	Type   string `cbor:"type"`
	Number uint32 `cbor:"number,omitempty"`

	// Init event:
	// Data is the full ftyp+moov for all tracks combined.
	Data []byte `cbor:"data,omitempty"`
	// Catalog describes per-track codec/dimensions/timescale.
	Catalog *Catalog `cbor:"catalog,omitempty"`
	// TrackInits is per-track standalone ftyp+moov bytes, keyed by
	// stringified track ID (for HLS per-track init segments).
	TrackInits map[string][]byte `cbor:"track_inits,omitempty"`

	// Segment / signed-segment event:
	// Tracks is per-track moof+mdat bytes keyed by stringified track ID.
	Tracks map[string][]byte `cbor:"tracks,omitempty"`
	// Durations is per-track segment duration in timescale ticks.
	Durations map[string]uint64 `cbor:"durations,omitempty"`
	// SampleCounts is per-track sample (frame) count for this segment.
	SampleCounts map[string]uint32 `cbor:"sample_counts,omitempty"`
	// BodySize is the total bytes this segment contributes to the
	// concatenated output (sum of len(Tracks[*])).
	BodySize uint64 `cbor:"body_size,omitempty"`
	// DurationUs is the segment's playable wall duration in microseconds.
	DurationUs uint64 `cbor:"duration_us,omitempty"`
}

// Catalog mirrors the Rust authoritative type; only the fields consumers need
// are mirrored. A canonical segment's catalog is single-track (Video xor
// Audio).
type Catalog struct {
	Video *CatalogVideo `cbor:"video,omitempty"`
	Audio *CatalogAudio `cbor:"audio,omitempty"`
}

// CatalogVideo holds the video renditions, keyed by rendition name.
type CatalogVideo struct {
	Renditions map[string]VideoConfig `cbor:"renditions"`
}

// CatalogAudio holds the audio renditions, keyed by rendition name.
type CatalogAudio struct {
	Renditions map[string]AudioConfig `cbor:"renditions"`
}

// VideoConfig is one video rendition's codec configuration.
type VideoConfig struct {
	Codec       string    `cbor:"codec"`
	Container   Container `cbor:"container"`
	CodedWidth  uint32    `cbor:"codedWidth"`
	CodedHeight uint32    `cbor:"codedHeight"`
}

// AudioConfig is one audio rendition's codec configuration.
type AudioConfig struct {
	Codec            string    `cbor:"codec"`
	Container        Container `cbor:"container"`
	SampleRate       uint32    `cbor:"sampleRate"`
	NumberOfChannels uint32    `cbor:"numberOfChannels"`
}

// Container is the tagged-union container descriptor. Kind is "cmaf" (the
// default, with Timescale/TrackID populated) or "legacy" (both zero).
type Container struct {
	Kind      string `cbor:"kind"`
	Timescale uint32 `cbor:"timescale,omitempty"`
	TrackID   uint32 `cbor:"trackId,omitempty"`
}

// TrackID returns the configured CMAF track ID, or 0 for legacy.
func (c VideoConfig) TrackID() uint32 { return c.Container.TrackID }

// TrackID returns the configured CMAF track ID, or 0 for legacy.
func (c AudioConfig) TrackID() uint32 { return c.Container.TrackID }

// Timescale returns the media timescale (ticks per second), or 0 for legacy.
func (c VideoConfig) Timescale() uint32 { return c.Container.Timescale }

// Timescale returns the media timescale (ticks per second), or 0 for legacy.
func (c AudioConfig) Timescale() uint32 { return c.Container.Timescale }
