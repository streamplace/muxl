//! `unwrap` — recover canonical MUXL segments from any storage wrapper.
//!
//! The inverse of the presentation/`wrap` layer. Given a bare m4s stream, a
//! MUXL fMP4 (`ftyp+moov+segments`), or a flat MP4 (`ftyp+moov+mdat`
//! envelope), [`unwrap`] fast-forwards over the container framing — *without*
//! parsing the `moov` or any sample table — and splits the canonical-segment
//! stream on `uuid` boundaries.
//!
//! The returned [`Segment::data`] slices are verbatim sub-slices of the
//! input: a hash or signature taken over a canonical segment survives the
//! round trip through any storage format unchanged (spec
//! `canonical-form.md § Round-Trip Property`). This is the goal-#3 consumer
//! primitive — `concat` and the signing layer are byte-passthrough on top of
//! it rather than re-minting fragments.

use std::io::Read;

use crate::catalog::{self, Catalog};
use crate::error::{Error, Result};
use crate::segment::MUXL_UUID;

/// One canonical segment recovered from a storage wrapper.
pub struct Segment<'a> {
    /// Track this segment belongs to (from its catalog's CMAF container).
    pub track_id: u32,
    /// The single-track catalog carried in the segment's MUXL `uuid` box.
    pub catalog: Catalog,
    /// Verbatim segment bytes: `[uuid-c2pa?][uuid-muxl][moof][mdat]…`.
    pub data: &'a [u8],
}

/// Recover the canonical MUXL segments from `bytes`.
///
/// Accepts a bare m4s stream, a MUXL fMP4, or a flat MP4 interchangeably; the
/// container header is fast-forwarded over and the segment stream is split on
/// MUXL `uuid` boundaries. A leading C2PA/S2PA `uuid` box (signed segments)
/// stays attached to the segment it prefixes.
pub fn unwrap(bytes: &[u8]) -> Result<Vec<Segment<'_>>> {
    let stream = locate_segment_stream(bytes)?;
    scan_segments(stream)
}

/// Locate the contiguous canonical-segment byte range inside a storage
/// wrapper: skip a leading `ftyp`/`moov` (and other framing), and descend
/// into a flat MP4's outer `mdat` envelope whose payload *is* the segment
/// stream. A bare segment stream is returned unchanged.
fn locate_segment_stream(bytes: &[u8]) -> Result<&[u8]> {
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let (kind, body_start, box_end) = read_box_header(bytes, pos)?;
        match &kind {
            // Container framing — skip past it.
            b"ftyp" | b"moov" | b"free" | b"skip" | b"styp" | b"sidx" => pos = box_end,
            // Flat MP4 outer envelope: its payload is the verbatim segment stream.
            b"mdat" => return Ok(&bytes[body_start..box_end]),
            // Start of a bare / fMP4 segment stream.
            b"moof" => return Ok(&bytes[pos..]),
            b"uuid" => {
                let is_muxl =
                    body_start + 16 <= box_end && bytes[body_start..body_start + 16] == MUXL_UUID;
                if is_muxl {
                    // A MUXL uuid heads the canonical segment stream.
                    return Ok(&bytes[pos..]);
                }
                // A non-MUXL uuid is either a signed segment's leading c2pa/S2PA
                // box — immediately followed by its MUXL uuid, so the bare
                // segment stream starts here — or a wrapper-level box that
                // sign_per_track places after ftyp, *before* moov, which is
                // container framing to skip.
                if next_box_is_muxl(bytes, box_end) {
                    return Ok(&bytes[pos..]);
                }
                pos = box_end;
            }
            // Unknown leading box — skip conservatively.
            _ => pos = box_end,
        }
    }
    // No recognizable framing: treat the whole input as the segment stream.
    Ok(bytes)
}

/// Whether the box at `pos` is a MUXL `uuid` box — used to tell a signed
/// segment's leading c2pa prefix (followed by its MUXL uuid) apart from a
/// wrapper-level c2pa box (followed by `moov`).
fn next_box_is_muxl(bytes: &[u8], pos: usize) -> bool {
    match read_box_header(bytes, pos) {
        Ok((kind, body_start, box_end)) => {
            &kind == b"uuid"
                && body_start + 16 <= box_end
                && bytes[body_start..body_start + 16] == MUXL_UUID
        }
        Err(_) => false,
    }
}

/// Split a segment stream on MUXL `uuid` boundaries into verbatim segments.
fn scan_segments(stream: &[u8]) -> Result<Vec<Segment<'_>>> {
    let mut segs = Vec::new();
    let mut seg_start: Option<usize> = None;
    let mut seg_has_muxl = false;
    let mut pos = 0usize;

    while pos + 8 <= stream.len() {
        let (kind, body_start, box_end) = read_box_header(stream, pos)?;
        if &kind == b"uuid" {
            let is_muxl = body_start + 16 <= box_end && stream[body_start..body_start + 16] == MUXL_UUID;
            // A uuid box opens a new segment when either: it's a second muxl
            // uuid (the previous segment's fragments are complete), or it's a
            // non-muxl (c2pa) uuid, which always heads a fresh segment's
            // signing prefix.
            let boundary = seg_start.is_some() && (!is_muxl || seg_has_muxl);
            if boundary {
                push_segment(&mut segs, stream, seg_start.unwrap(), pos)?;
                seg_start = None;
                seg_has_muxl = false;
            }
            seg_start.get_or_insert(pos);
            seg_has_muxl |= is_muxl;
        } else if seg_start.is_none() {
            return Err(Error::InvalidMp4(
                "segment stream did not begin with a uuid box".into(),
            ));
        }
        pos = box_end;
    }
    if let Some(start) = seg_start {
        push_segment(&mut segs, stream, start, stream.len())?;
    }
    Ok(segs)
}

fn push_segment<'a>(
    out: &mut Vec<Segment<'a>>,
    stream: &'a [u8],
    start: usize,
    end: usize,
) -> Result<()> {
    let data = &stream[start..end];
    let catalog = catalog::from_segment(data)?;
    let track_id = catalog_tracks(&catalog)
        .map(|t| t.track_id)
        .next()
        .ok_or_else(|| Error::InvalidMp4("segment catalog describes no track".into()))?;
    out.push(Segment {
        track_id,
        catalog,
        data,
    });
    Ok(())
}

/// One rendition of a [`Catalog`], reduced to what the track-generic layers
/// (unwrap, event re-derivation, flat-MP4 synthesis) need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CatalogTrack {
    pub track_id: u32,
    pub timescale: u32,
    /// The rendition is a timed-text (WebVTT) track. Text tracks are sparse
    /// and never drive GoP boundaries.
    pub is_text: bool,
}

/// Every rendition in `catalog` — video, then audio, then text — as a
/// [`CatalogTrack`]. This is the one place that enumerates track types, so
/// a code path that walks "all tracks" treats text exactly like video and
/// audio.
pub(crate) fn catalog_tracks(catalog: &Catalog) -> impl Iterator<Item = CatalogTrack> + '_ {
    let av = catalog
        .video_configs()
        .map(|v| (v.track_id(), v.timescale()))
        .chain(catalog.audio_configs().map(|a| (a.track_id(), a.timescale())))
        .map(|(track_id, timescale)| CatalogTrack {
            track_id,
            timescale,
            is_text: false,
        });
    let text = catalog.text_configs().map(|t| CatalogTrack {
        track_id: t.track_id(),
        timescale: t.timescale(),
        is_text: true,
    });
    av.chain(text)
}

/// Look up one track by id across every rendition type.
pub(crate) fn catalog_track(catalog: &Catalog, track_id: u32) -> Option<CatalogTrack> {
    catalog_tracks(catalog).find(|t| t.track_id == track_id)
}

/// Merge the single-track catalogs of unwrapped `segments` into one
/// multi-track [`Catalog`] — what [`crate::present::init`] needs to build a
/// combined `moov` when re-wrapping. Renditions dedupe by name (same track →
/// same config); video `display`/`rotation`/`flip` are taken from the first
/// segment that carries them.
pub fn aggregate_catalog(segments: &[Segment<'_>]) -> Catalog {
    let mut agg = Catalog::default();
    for seg in segments {
        merge_segment_catalog(&mut agg, &seg.catalog);
    }
    agg
}

/// Fold one segment's single-track catalog into a running aggregate. Renditions
/// of every type (video, audio, text) dedupe by name (same track → same
/// config); video `display`/`rotation`/`flip` are taken from the first segment
/// that carries them.
pub(crate) fn merge_segment_catalog(agg: &mut Catalog, cat: &Catalog) {
    if let Some(v) = &cat.video {
        for (name, cfg) in &v.renditions {
            agg.insert_video(name.clone(), cfg.clone());
        }
        if let Some(av) = agg.video.as_mut() {
            av.display = av.display.or(v.display);
            av.rotation = av.rotation.or(v.rotation);
            av.flip = av.flip.or(v.flip);
        }
    }
    if let Some(a) = &cat.audio {
        for (name, cfg) in &a.renditions {
            agg.insert_audio(name.clone(), cfg.clone());
        }
    }
    if let Some(t) = &cat.text {
        for (name, cfg) in &t.renditions {
            agg.insert_text(name.clone(), cfg.clone());
        }
    }
}

/// Re-derive the canonical per-track event stream from a stored MUXL wrapper
/// (bare m4s, fMP4, or flat MP4). The per-track segment bytes are returned
/// **verbatim** — any C2PA/S2PA signature is preserved — while per-track
/// durations and sample counts are recomputed by parsing each segment's
/// moofs.
///
/// Emits the same [`crate::cbor::CborEvent`] shape the live segmenter
/// produces (one `Init`, then one `Segment` per GoP), so stored segments
/// drive live HLS / live-to-VOD / DVR exactly like freshly-segmented ones —
/// without re-minting (and thus invalidating the signatures on) the bytes.
pub fn segment_events(bytes: &[u8]) -> Result<Vec<crate::cbor::CborEvent>> {
    let mut events = Vec::new();
    segment_events_streaming(bytes, |ev| {
        events.push(ev);
        Ok(())
    })?;
    Ok(events)
}

/// In-memory streaming form of [`segment_events`]: each event is handed to
/// `emit` the moment it is finalized, so a caller serializing straight to a sink
/// never holds the whole event `Vec` — a full second copy of the content — at
/// once. Still slurps `bytes` (it borrows verbatim slices); for arbitrarily
/// large inputs use [`segment_events_stream`], which holds neither.
pub fn segment_events_streaming<F>(bytes: &[u8], emit: F) -> Result<()>
where
    F: FnMut(crate::cbor::CborEvent) -> Result<()>,
{
    let mut builder = StreamEventBuilder::new(emit);
    for seg in unwrap(bytes)? {
        builder.push_segment(seg.data)?;
    }
    builder.finish()
}

/// Fully streaming form of [`segment_events`] over a plain `Read`: the input is
/// consumed front-to-back and never held in full, so peak memory is one GoP
/// regardless of total size. This is the path for arbitrarily large VODs — the
/// slurp-based forms hold the whole input (and, for the `Vec` form, a copy of
/// it) in memory.
///
/// Accepts the same wrappers as [`unwrap`] (bare m4s, MUXL fMP4, flat MP4) but
/// requires no random access. The `Init`/catalog is built from the first GoP —
/// canonical interleave puts every track's first segment there — and re-emitted
/// if a later GoP introduces a new track, matching the live segmenter and the
/// mid-stream-init-swap path the consumer already tolerates.
pub fn segment_events_stream<R: Read, F>(reader: R, emit: F) -> Result<()>
where
    F: FnMut(crate::cbor::CborEvent) -> Result<()>,
{
    let mut builder = StreamEventBuilder::new(emit);
    scan_wrapper_stream(reader, |seg| builder.push_segment(seg))?;
    builder.finish()
}

/// One GoP's worth of per-track event data, accumulated before being emitted as
/// a [`CborEvent::Segment`](crate::cbor::CborEvent::Segment).
#[derive(Default)]
struct GopAccum {
    tracks: std::collections::BTreeMap<String, crate::cbor::ByteString>,
    durations: std::collections::BTreeMap<String, u64>,
    sample_counts: std::collections::BTreeMap<String, u32>,
    samples: std::collections::BTreeMap<String, crate::cbor::CborTrackSamples>,
    track_byte_sizes: std::collections::BTreeMap<String, u64>,
    first_decode_times: std::collections::BTreeMap<String, u64>,
    body_size: u64,
    /// Longest non-text (video/audio) track span in this GoP.
    duration_us: u64,
    /// Longest text track span in this GoP. Only used when the GoP has no
    /// video or audio track: a text track is clipped/padded to the GoP span
    /// the AV tracks define, so it must not stretch `duration_us`.
    text_duration_us: u64,
    has_non_text: bool,
}

struct PendingSegment {
    data: Vec<u8>,
    catalog: Catalog,
    tid: u32,
    samples: crate::segment::TrackSamples,
    dts: u64,
}

/// Turns a forward stream of verbatim canonical segments into the live event
/// shape, holding the current AV group and text received ahead of its AV.
/// Shared by the slurp ([`segment_events_streaming`]) and fully-streaming
/// ([`segment_events_stream`]) paths so the two emit byte-identical events.
struct StreamEventBuilder<F> {
    emit: F,
    /// Aggregate catalog over every segment seen so far (grows as tracks appear).
    running: Catalog,
    /// Per-track media timescale and text flag, recorded as each track
    /// first appears.
    timescales: std::collections::BTreeMap<u32, (u32, bool)>,
    /// Catalog last exposed to consumers, including source/language changes.
    emitted_catalog: Option<Catalog>,
    cur: Option<GopAccum>,
    /// AV ordering retains the legacy numeric-wrap grouping for flat blobs,
    /// including sparse leading/trailing non-reference tracks.
    last_av_tid: Option<u32>,
    /// Text tracks received ahead of the next AV boundary.
    pending: Vec<PendingSegment>,
}

impl<F> StreamEventBuilder<F>
where
    F: FnMut(crate::cbor::CborEvent) -> Result<()>,
{
    fn new(emit: F) -> Self {
        StreamEventBuilder {
            emit,
            running: Catalog::default(),
            timescales: std::collections::BTreeMap::new(),
            emitted_catalog: None,
            cur: None,
            last_av_tid: None,
            pending: Vec::new(),
        }
    }

    /// AV numeric wraps close a GoP, as in the legacy reader. Text order does
    /// not: text may precede AV without stealing the prior GoP's reference.
    fn push_segment(&mut self, seg: &[u8]) -> Result<()> {
        let (tid, samples, dts) = crate::present::segment_index(seg)?;
        let catalog = crate::catalog::from_segment(seg)?;
        let reference = self.running.video_configs().chain(catalog.video_configs())
            .min_by_key(|c| c.track_id()).map(|c| (c.track_id(),c.timescale()))
            .or_else(|| self.running.audio_configs().chain(catalog.audio_configs())
                .min_by_key(|c| c.track_id()).map(|c| (c.track_id(),c.timescale())))
            .or_else(|| self.running.text_configs().chain(catalog.text_configs())
                .min_by_key(|c| c.track_id()).map(|c| (c.track_id(),c.timescale())))
            .ok_or_else(|| Error::InvalidMp4("segment has no reference track".into()))?;
        let reference_key = reference.0.to_string();
        let is_text = catalog_track(&catalog,tid).is_some_and(|t| t.is_text);
        let has_av = self.running.video_configs().next().is_some()
            || self.running.audio_configs().next().is_some()
            || catalog.video_configs().next().is_some()
            || catalog.audio_configs().next().is_some();
        let boundary = if is_text {
            !has_av && tid == reference.0 && self.cur.as_ref()
                .is_some_and(|g| g.tracks.contains_key(&reference_key))
        } else {
            self.last_av_tid.is_some_and(|last| tid <= last)
        };
        let ahead = is_text && !boundary && self.cur.as_ref().is_some_and(|g| {
            if g.tracks.contains_key(&tid.to_string()) { return true; }
            let Some(&start) = g.first_decode_times.get(&reference_key) else { return false; };
            let Some(&duration) = g.durations.get(&reference_key) else { return false; };
            let Some(track) = catalog_track(&catalog,tid) else { return false; };
            g.has_non_text && tid != reference.0 && track.timescale > 0 && reference.1 > 0 &&
                crate::segment::ticks_to_ms(dts,track.timescale) >= crate::segment::ticks_to_ms(start+duration,reference.1)
        });
        let segment = PendingSegment {data:seg.to_vec(),catalog,tid,samples,dts};
        if ahead {
            self.pending.push(segment);
            return Ok(());
        }
        if boundary {
            if let Some(g) = self.cur.take() { self.flush_gop(g)?; }
            self.drain_pending()?;
        }
        if !is_text { self.last_av_tid = Some(tid); }
        self.append_segment(segment);
        Ok(())
    }

    /// Sparse inputs may end without another AV reference. Retain every
    /// pending segment, splitting repeated IDs rather than overwriting bytes.
    fn drain_pending(&mut self) -> Result<()> {
        for pending in std::mem::take(&mut self.pending) {
            if self.cur.as_ref().is_some_and(|g| g.tracks.contains_key(&pending.tid.to_string())) {
                if let Some(g) = self.cur.take() { self.flush_gop(g)?; }
            }
            self.append_segment(pending);
        }
        Ok(())
    }

    fn append_segment(&mut self, segment: PendingSegment) {
        let PendingSegment {data,catalog,tid,samples:ts,dts} = segment;
        merge_segment_catalog(&mut self.running,&catalog);
        for t in catalog_tracks(&catalog) { self.timescales.insert(t.track_id,(t.timescale,t.is_text)); }
        let dur_ticks: u64 = ts.durations.iter().map(|&d| d as u64).sum();
        let gop = self.cur.get_or_insert_with(GopAccum::default);
        if let Some(&(tsc,is_text)) = self.timescales.get(&tid) {
            gop.has_non_text |= !is_text;
            if tsc > 0 {
                let us = dur_ticks*1_000_000/tsc as u64;
                if is_text { gop.text_duration_us=gop.text_duration_us.max(us); }
                else { gop.duration_us=gop.duration_us.max(us); }
            }
        }
        let key=tid.to_string();
        gop.body_size+=data.len() as u64;
        gop.durations.insert(key.clone(),dur_ticks);
        gop.sample_counts.insert(key.clone(),ts.durations.len() as u32);
        gop.samples.insert(key.clone(),(&ts).into());
        gop.track_byte_sizes.insert(key.clone(),data.len() as u64);
        gop.first_decode_times.insert(key.clone(),dts);
        gop.tracks.insert(key,crate::cbor::ByteString(data));
    }

    /// Emit one completed GoP, with a fresh Init for any catalog change.
    fn flush_gop(&mut self, g: GopAccum) -> Result<()> {
        use crate::cbor::{ByteString, CborEvent};
        if self.emitted_catalog.as_ref() != Some(&self.running) {
            let track_inits: std::collections::BTreeMap<String, ByteString> =
                crate::init::build_track_init_segments(&self.running)?
                    .into_iter()
                    .map(|(tid, b)| (tid.to_string(), ByteString(b)))
                    .collect();
            (self.emit)(CborEvent::Init {
                version: crate::cbor::METAFILE_VERSION,
                data: crate::init::build_init_segment(&self.running)?,
                catalog: Some(self.running.clone()),
                track_inits,
            })?;
            self.emitted_catalog = Some(self.running.clone());
        }
        let duration_us = if g.has_non_text {
            g.duration_us
        } else {
            g.text_duration_us
        };
        (self.emit)(CborEvent::Segment {
            tracks: g.tracks,
            durations: g.durations,
            sample_counts: g.sample_counts,
            samples: g.samples,
            track_byte_sizes: g.track_byte_sizes,
            first_decode_times: g.first_decode_times,
            body_size: g.body_size,
            duration_us,
        })
    }

    /// Flush the final GoP. No segments ⇒ no events (matches `segment_events`).
    fn finish(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            if let Some(g) = self.cur.take() { self.flush_gop(g)?; }
            self.drain_pending()?;
        }
        if let Some(g) = self.cur.take() {
            self.flush_gop(g)?;
        }
        Ok(())
    }
}

/// A small forward-only read buffer over an arbitrary `Read`. Holds at most a
/// single box at a time; consumed bytes are dropped as the cursor advances.
struct ScanBuf<R> {
    reader: R,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<R: Read> ScanBuf<R> {
    fn new(reader: R) -> Self {
        ScanBuf {
            reader,
            buf: Vec::with_capacity(1 << 16),
            pos: 0,
            eof: false,
        }
    }

    fn avail(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Ensure at least `need` bytes are buffered at the cursor, reading more as
    /// required. Returns the count actually available (may be `< need` at EOF).
    fn ensure(&mut self, need: usize) -> Result<usize> {
        if self.avail() >= need {
            return Ok(self.avail());
        }
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let mut tmp = [0u8; 1 << 16];
        while self.avail() < need && !self.eof {
            let n = self.reader.read(&mut tmp)?;
            if n == 0 {
                self.eof = true;
                break;
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
        Ok(self.avail())
    }

    fn data(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

/// `(fourcc, header_len, body_len)` for the next box, or `None` at clean EOF.
/// `body_len` is `None` for the size-0 (to-EOF) form.
type StreamBoxHeader = ([u8; 4], usize, Option<u64>);

fn next_box_header<R: Read>(sb: &mut ScanBuf<R>) -> Result<Option<StreamBoxHeader>> {
    if sb.ensure(8)? < 8 {
        if sb.avail() == 0 {
            return Ok(None);
        }
        return Err(Error::InvalidMp4("truncated box header".into()));
    }
    let d = sb.data();
    let size32 = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&d[4..8]);
    let oor = || Error::InvalidMp4("box size out of range".into());
    match size32 {
        1 => {
            if sb.ensure(16)? < 16 {
                return Err(Error::InvalidMp4("truncated 64-bit box header".into()));
            }
            let d = sb.data();
            let large = u64::from_be_bytes(d[8..16].try_into().unwrap());
            Ok(Some((kind, 16, Some(large.checked_sub(16).ok_or_else(oor)?))))
        }
        0 => Ok(Some((kind, 8, None))),
        n => Ok(Some((kind, 8, Some((n as u64).checked_sub(8).ok_or_else(oor)?)))),
    }
}

/// Whether the `uuid` box at the cursor is a MUXL `uuid` (its first 16 payload
/// bytes are [`MUXL_UUID`]).
fn uuid_is_muxl<R: Read>(sb: &mut ScanBuf<R>, header_len: usize, body: Option<u64>) -> Result<bool> {
    if matches!(body, Some(b) if b < 16) {
        return Ok(false);
    }
    let need = header_len + 16;
    if sb.ensure(need)? < need {
        return Ok(false);
    }
    Ok(sb.data()[header_len..need] == MUXL_UUID)
}

/// Whether the box *following* the non-MUXL `uuid` at the cursor is a MUXL
/// `uuid` — i.e. this is a signed segment's leading c2pa/S2PA prefix (the stream
/// starts here) rather than wrapper framing before `moov`. The streaming twin of
/// [`next_box_is_muxl`].
fn next_box_is_muxl_stream<R: Read>(
    sb: &mut ScanBuf<R>,
    header_len: usize,
    body: Option<u64>,
) -> Result<bool> {
    let body = match body {
        Some(b) => b as usize,
        None => return Ok(false),
    };
    let uuid_total = header_len + body;
    if sb.ensure(uuid_total + 8)? < uuid_total + 8 {
        return Ok(false);
    }
    let nh = &sb.data()[uuid_total..];
    let next_hdr_len = if u32::from_be_bytes([nh[0], nh[1], nh[2], nh[3]]) == 1 {
        16
    } else {
        8
    };
    let need = uuid_total + next_hdr_len + 16;
    if sb.ensure(need)? < need {
        return Ok(false);
    }
    let d = sb.data();
    Ok(&d[uuid_total + 4..uuid_total + 8] == b"uuid"
        && d[uuid_total + next_hdr_len..uuid_total + next_hdr_len + 16] == MUXL_UUID)
}

/// Skip `n` bytes of framing, reading and discarding in bounded chunks.
fn skip_n<R: Read>(sb: &mut ScanBuf<R>, mut n: usize) -> Result<()> {
    while n > 0 {
        let want = n.min(1 << 16);
        if sb.ensure(want)? == 0 {
            return Err(Error::InvalidMp4("truncated box body".into()));
        }
        let take = sb.avail().min(n);
        sb.consume(take);
        n -= take;
    }
    Ok(())
}

/// Append the box at the cursor (header + body) to `out`, returning its total
/// length. `body == None` reads to EOF (the size-0 form).
fn capture_box<R: Read>(
    sb: &mut ScanBuf<R>,
    header_len: usize,
    body: Option<u64>,
    out: &mut Vec<u8>,
) -> Result<usize> {
    match body {
        Some(body) => {
            let total = header_len + body as usize;
            let mut left = total;
            while left > 0 {
                let want = left.min(1 << 16);
                if sb.ensure(want)? == 0 {
                    return Err(Error::InvalidMp4("truncated box body".into()));
                }
                let take = sb.avail().min(left);
                out.extend_from_slice(&sb.data()[..take]);
                sb.consume(take);
                left -= take;
            }
            Ok(total)
        }
        None => {
            let mut total = 0;
            while sb.ensure(1 << 16)? > 0 {
                let take = sb.avail();
                out.extend_from_slice(&sb.data()[..take]);
                sb.consume(take);
                total += take;
            }
            Ok(total)
        }
    }
}

/// Forward-scan a MUXL storage wrapper, invoking `on_segment` with each verbatim
/// canonical segment. The streaming twin of [`locate_segment_stream`] +
/// [`scan_segments`]: it skips a leading `ftyp`/`moov` (and other framing),
/// descends a flat MP4's outer `mdat` envelope, and splits the canonical-segment
/// stream on MUXL `uuid` boundaries — without seeking or holding more than one box.
pub(crate) fn scan_wrapper_stream<R: Read, F>(reader: R, mut on_segment: F) -> Result<()>
where
    F: FnMut(&[u8]) -> Result<()>,
{
    let mut sb = ScanBuf::new(reader);
    let mut in_stream = false;
    // Bytes left in the flat MP4 `mdat` envelope; `None` = unbounded (to EOF).
    let mut envelope_remaining: Option<u64> = None;
    let mut seg_buf: Vec<u8> = Vec::new();
    let mut seg_started = false;
    let mut seg_has_muxl = false;

    loop {
        if in_stream && envelope_remaining == Some(0) {
            break;
        }
        let (kind, header_len, body) = match next_box_header(&mut sb)? {
            Some(h) => h,
            None => break,
        };

        if !in_stream {
            // Locating: skip framing, find the segment stream / mdat envelope.
            match &kind {
                b"ftyp" | b"moov" | b"free" | b"skip" | b"styp" | b"sidx" => {
                    skip_n(&mut sb, header_len + body.map_or(0, |b| b as usize))?;
                }
                b"mdat" => {
                    // Flat MP4 envelope: its body *is* the segment stream.
                    sb.consume(header_len);
                    in_stream = true;
                    envelope_remaining = body;
                }
                // Stream starts at this box: re-read it in the in-stream branch
                // (no consume) so its bytes join the first segment.
                b"moof" | b"uuid" => {
                    if &kind == b"uuid"
                        && !uuid_is_muxl(&mut sb, header_len, body)?
                        && !next_box_is_muxl_stream(&mut sb, header_len, body)?
                    {
                        // A non-MUXL uuid not heading a signed segment is wrapper
                        // framing (e.g. a c2pa box before moov) — skip it.
                        skip_n(&mut sb, header_len + body.map_or(0, |b| b as usize))?;
                    } else {
                        in_stream = true;
                        envelope_remaining = None;
                    }
                }
                _ => {
                    skip_n(&mut sb, header_len + body.map_or(0, |b| b as usize))?;
                }
            }
            continue;
        }

        // In the segment stream: split on uuid boundaries (mirrors scan_segments).
        if &kind == b"uuid" {
            let is_muxl = uuid_is_muxl(&mut sb, header_len, body)?;
            // A uuid opens a new segment when it's a second muxl uuid (the
            // previous segment's fragments are complete) or any non-muxl (c2pa)
            // uuid, which always heads a fresh segment's signing prefix.
            if seg_started && (!is_muxl || seg_has_muxl) {
                on_segment(&seg_buf)?;
                seg_buf.clear();
                seg_has_muxl = false;
            }
            seg_started = true;
            seg_has_muxl |= is_muxl;
        } else if !seg_started {
            return Err(Error::InvalidMp4(
                "segment stream did not begin with a uuid box".into(),
            ));
        }

        let consumed = capture_box(&mut sb, header_len, body, &mut seg_buf)? as u64;
        if let Some(rem) = envelope_remaining.as_mut() {
            *rem = rem
                .checked_sub(consumed)
                .ok_or_else(|| Error::InvalidMp4("box overruns mdat envelope".into()))?;
        }
    }

    if seg_started && !seg_buf.is_empty() {
        on_segment(&seg_buf)?;
    }
    Ok(())
}

/// Read an ISOBMFF box header at `pos`, returning `(fourcc, body_start,
/// box_end)`. Handles the 32-bit, 64-bit-`largesize` (size==1), and
/// to-EOF (size==0) forms.
pub(crate) fn read_box_header(bytes: &[u8], pos: usize) -> Result<([u8; 4], usize, usize)> {
    if pos + 8 > bytes.len() {
        return Err(Error::InvalidMp4("truncated box header".into()));
    }
    let size32 = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&bytes[pos + 4..pos + 8]);
    let bad = || Error::InvalidMp4("box size out of range".into());
    let (body_start, box_end) = match size32 {
        1 => {
            if pos + 16 > bytes.len() {
                return Err(Error::InvalidMp4("truncated 64-bit box header".into()));
            }
            let large = u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize;
            let end = large
                .checked_add(pos)
                .filter(|&e| e >= pos + 16 && e <= bytes.len())
                .ok_or_else(bad)?;
            (pos + 16, end)
        }
        0 => (pos + 8, bytes.len()),
        n => {
            let end = (n as usize)
                .checked_add(pos)
                .filter(|&e| e >= pos + 8 && e <= bytes.len())
                .ok_or_else(bad)?;
            (pos + 8, end)
        }
    };
    Ok((kind, body_start, box_end))
}

/// Byte offset at which the canonical-segment stream begins inside a storage
/// wrapper, resolved with random access: only the leading container framing is
/// read, never the body. The mirror of [`locate_segment_stream`] for inputs too
/// large to hold in memory.
///
/// This is the base a caller holding *fragment-relative* offsets — an HLS
/// metafile's `offset`, say — adds to reach an absolute position in the blob.
/// muxl owns the number because muxl synthesized the header that displaces the
/// fragments: a caller computing it by hand has to re-derive the `mdat`
/// envelope's framing, including the 64-bit `largesize` form that any body over
/// 4 GiB forces, and silently reads garbage the day it guesses wrong.
///
/// Returns 0 for a bare segment stream (nothing displaces it).
pub fn segment_stream_start<R: crate::io::ReadAt + ?Sized>(input: &R) -> Result<u64> {
    let size = input.size().map_err(Error::Io)?;
    let mut pos = 0u64;
    while pos + 8 <= size {
        let (kind, body_start, box_end) = read_box_header_at(input, pos, size)?;
        match &kind {
            // Container framing — skip past it.
            b"ftyp" | b"moov" | b"free" | b"skip" | b"styp" | b"sidx" => pos = box_end,
            // Flat MP4 outer envelope: its payload *is* the segment stream.
            b"mdat" => return Ok(body_start),
            // Start of a bare / fMP4 segment stream.
            b"moof" => return Ok(pos),
            b"uuid" => {
                if is_muxl_uuid_at(input, body_start, box_end)? {
                    return Ok(pos);
                }
                // A non-MUXL uuid is either a signed segment's leading c2pa/S2PA
                // box — immediately followed by its MUXL uuid, so the segment
                // stream starts here — or a wrapper-level box that
                // sign_per_track places after ftyp and before moov, which is
                // framing to skip. Disambiguate on the next box, exactly as
                // `next_box_is_muxl` does for the in-memory path.
                if box_end + 8 <= size {
                    let (next_kind, next_body, next_end) = read_box_header_at(input, box_end, size)?;
                    if &next_kind == b"uuid" && is_muxl_uuid_at(input, next_body, next_end)? {
                        return Ok(pos);
                    }
                }
                pos = box_end;
            }
            // Unknown leading box — skip conservatively.
            _ => pos = box_end,
        }
    }
    // No recognizable framing: the whole input is the segment stream.
    Ok(0)
}

/// Whether the `uuid` box body at `body_start` carries [`MUXL_UUID`].
fn is_muxl_uuid_at<R: crate::io::ReadAt + ?Sized>(
    input: &R,
    body_start: u64,
    box_end: u64,
) -> Result<bool> {
    if body_start + 16 > box_end {
        return Ok(false);
    }
    let mut uuid = [0u8; 16];
    input.read_exact_at(body_start, &mut uuid).map_err(Error::Io)?;
    Ok(uuid == MUXL_UUID)
}

/// Random-access box header read: returns the FourCC, the offset its body
/// starts at, and the offset one past its end. Handles all three ISO-BMFF
/// sizings — 32-bit, `size == 1` 64-bit `largesize`, and `size == 0`
/// extends-to-EOF.
fn read_box_header_at<R: crate::io::ReadAt + ?Sized>(
    input: &R,
    pos: u64,
    size: u64,
) -> Result<([u8; 4], u64, u64)> {
    let bad = || Error::InvalidMp4(format!("malformed box header at offset {pos}"));

    let avail = size.saturating_sub(pos);
    if avail < 8 {
        return Err(bad());
    }
    let want = if avail >= 16 { 16 } else { 8 };
    let mut hdr = [0u8; 16];
    input
        .read_exact_at(pos, &mut hdr[..want])
        .map_err(Error::Io)?;

    let mut kind = [0u8; 4];
    kind.copy_from_slice(&hdr[4..8]);

    let (body_start, box_end) = match u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) {
        // 64-bit largesize follows the FourCC. The `mdat` envelope over a
        // >4 GiB body always lands here.
        1 => {
            if want < 16 {
                return Err(bad());
            }
            let large = u64::from_be_bytes([
                hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
            ]);
            if large < 16 {
                return Err(bad());
            }
            (pos + 16, pos.checked_add(large).ok_or_else(bad)?)
        }
        // Extends to EOF.
        0 => (pos + 8, size),
        n if n < 8 => return Err(bad()),
        n => (pos + 8, pos.checked_add(n as u64).ok_or_else(bad)?),
    };
    if box_end > size {
        return Err(bad());
    }
    Ok((kind, body_start, box_end))
}

/// Where a caller's recorded segment offset is measured from. Storage layouts
/// differ on this — a flat-MP4 blob's index is usually relative to the
/// fragments, which the synthesized header displaces, while an
/// `[init][segments]` blob's is usually absolute — and guessing wrong reads a
/// header's worth of the wrong bytes. Callers state which they hold; muxl does
/// the arithmetic either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentOffset {
    /// Relative to the first canonical segment, so muxl resolves the container
    /// framing ahead of it and adds that itself.
    Stream(u64),
    /// An absolute byte position in the input; nothing is added.
    File(u64),
}

/// Read `count` canonical segments out of a storage wrapper, starting at
/// `offset`, touching only those segments' bytes.
///
/// [`SegmentOffset`] says what the offset is measured from — muxl resolves the
/// container framing for a [`SegmentOffset::Stream`] offset. Keeping that
/// addition here is the point: callers index fragments, muxl owns where the
/// bytes live.
///
/// Segment extents come from the same `uuid`-boundary rule [`unwrap`] uses, so
/// the caller supplies only a starting point — it never has to record, or get
/// right, how long a segment is. `count` of `None` reads to the end of the
/// stream. The returned bytes are verbatim, so signatures over them survive.
pub fn read_segments_at<R: crate::io::ReadAt + ?Sized>(
    input: &R,
    offset: SegmentOffset,
    count: Option<u64>,
) -> Result<Vec<u8>> {
    let total = input.size().map_err(Error::Io)?;
    // Only a stream-relative offset needs the container framing resolved; a
    // file offset is already absolute.
    let (base, offset) = match offset {
        SegmentOffset::Stream(o) => (segment_stream_start(input)?, o),
        SegmentOffset::File(o) => (0, o),
    };
    let start = base
        .checked_add(offset)
        .filter(|&s| s <= total)
        .ok_or_else(|| {
            Error::InvalidMp4(format!(
                "segment offset {offset} runs past the end of the input \
                 (segment stream starts at {base}, input is {total} bytes)"
            ))
        })?;

    // Walk box headers forward, applying `scan_segments`' boundary rule, until
    // `count` segments have been closed. Only headers are read here — the
    // payload is fetched once, as a single span, below.
    let mut pos = start;
    let mut closed = 0u64;
    let mut seg_open = false;
    let mut seg_has_muxl = false;
    let end = loop {
        if pos + 8 > total {
            break total;
        }
        let (kind, body_start, box_end) = read_box_header_at(input, pos, total)?;
        if &kind == b"uuid" {
            let is_muxl = is_muxl_uuid_at(input, body_start, box_end)?;
            // A uuid box opens a new segment when either it's a second muxl
            // uuid (the previous segment's fragments are complete) or it's a
            // non-muxl (c2pa) uuid, which always heads a signing prefix.
            if seg_open && (!is_muxl || seg_has_muxl) {
                closed += 1;
                if count.is_some_and(|c| closed >= c) {
                    break pos;
                }
                seg_has_muxl = false;
            }
            seg_open = true;
            seg_has_muxl |= is_muxl;
        } else if !seg_open {
            return Err(Error::InvalidMp4(format!(
                "offset {offset} does not land on a segment boundary \
                 (found a {} box, expected uuid)",
                String::from_utf8_lossy(&kind)
            )));
        }
        pos = box_end;
    };

    if let Some(c) = count
        && closed + u64::from(seg_open) < c
    {
        return Err(Error::InvalidMp4(format!(
            "requested {c} segments from offset {offset} but the stream held only {}",
            closed + u64::from(seg_open)
        )));
    }

    let len = usize::try_from(end - start)
        .map_err(|_| Error::InvalidMp4("segment span too large for this platform".into()))?;
    let mut buf = vec![0u8; len];
    input.read_exact_at(start, &mut buf).map_err(Error::Io)?;
    Ok(buf)
}

/// Test fixture: a real AV stream plus a synthetic canonical WebVTT track, for
/// exercising every track-generic path (unwrap, event re-derivation, flat MP4
/// synthesis) with text.
#[cfg(test)]
pub(crate) mod text_fixture {
    use super::*;
    use crate::catalog::{Container, TextConfig};
    use crate::fragment::{FrameInfo, TrackProgress, write_frame_fragment};
    use std::io::Cursor;

    /// One `wvtt` sample that holds a single cue: `vttc` with a `payl`.
    pub fn vttc(text: &str) -> Vec<u8> {
        let payl_len = 8 + text.len();
        let mut out = Vec::new();
        out.extend_from_slice(&((8 + payl_len) as u32).to_be_bytes());
        out.extend_from_slice(b"vttc");
        out.extend_from_slice(&(payl_len as u32).to_be_bytes());
        out.extend_from_slice(b"payl");
        out.extend_from_slice(text.as_bytes());
        out
    }

    /// One `wvtt` gap sample: a lone `vtte` box.
    pub fn vtte() -> Vec<u8> {
        b"\x00\x00\x00\x08vtte".to_vec()
    }

    pub fn text_config(track_id: u32) -> TextConfig {
        TextConfig {
            codec: "wvtt".into(),
            container: Container::cmaf(1000, track_id),
            language: "en".into(),
            label: Some("captions".into()),
            config: "WEBVTT".into(),
        }
    }

    pub struct TextStream {
        /// The AV catalog plus the text rendition.
        pub catalog: Catalog,
        pub text_track_id: u32,
        /// Canonical segments in interleave order (per GoP, tracks ascending).
        pub ordered: Vec<(u32, Vec<u8>)>,
        /// Every text sample payload, in decode order.
        pub text_payloads: Vec<Vec<u8>>,
        /// Every text sample duration (ms), in decode order.
        pub text_durations: Vec<u32>,
    }

    impl TextStream {
        pub fn concat(&self) -> Vec<u8> {
            self.ordered.iter().flat_map(|(_, s)| s.iter().copied()).collect()
        }
    }

    /// Segment `fixture` (which must have video) and add a text track with the
    /// next free track id. Each GoP gets one text segment covering exactly the
    /// GoP: it starts at `floor(video_gop_start * 1000 / video_timescale)` and
    /// ends where the next GoP starts. Long enough GoPs get a cue for the
    /// first half and a `vtte` gap for the rest. The last GoP's text is
    /// extended by `tail_pad_ms`.
    pub fn av_with_text(fixture: &str, tail_pad_ms: u32) -> TextStream {
        let data = std::fs::read(format!("samples/fixtures/{fixture}"))
            .unwrap_or_else(|_| panic!("samples/fixtures/{fixture} must exist for tests"));
        let mut gops = Vec::new();
        let av = crate::segment::segment_fmp4(&mut Cursor::new(&data), |g| {
            gops.push(g);
            Ok(())
        })
        .unwrap();
        let video = av.video_configs().next().expect("fixture must have video");
        let (vtid, vts) = (video.track_id(), video.timescale() as u64);
        let ttid = catalog_tracks(&av).map(|t| t.track_id).max().unwrap() + 1;

        let mut catalog = av.clone();
        catalog.insert_text(format!("text{ttid}"), text_config(ttid));
        let prefix = crate::segment::mint_canonical_segment_prefix(&catalog, ttid).unwrap();

        let starts: Vec<u64> = gops
            .iter()
            .map(|g| g.first_decode_times[&vtid] * 1000 / vts)
            .collect();
        let last = gops.last().unwrap();
        let end_ms = (last.first_decode_times[&vtid] + last.durations[&vtid]) * 1000 / vts
            + tail_pad_ms as u64;

        let mut ordered = Vec::new();
        let mut text_payloads = Vec::new();
        let mut text_durations = Vec::new();
        for (gi, gop) in gops.iter().enumerate() {
            let mut progress = TrackProgress::starting_at(starts[gi]);
            for (&tid, bytes) in &gop.tracks {
                ordered.push((tid, bytes.clone()));
            }
            let end = starts.get(gi + 1).copied().unwrap_or(end_ms);
            let span = (end - starts[gi]) as u32;
            assert!(span > 0, "GoP {gi} has no text span");
            let samples: Vec<(u32, Vec<u8>)> = if span >= 2 {
                vec![
                    (span / 2, vttc(&format!("cue {gi}"))),
                    (span - span / 2, vtte()),
                ]
            } else {
                vec![(span, vttc(&format!("cue {gi}")))]
            };
            let mut seg = prefix.clone();
            for (duration, payload) in samples {
                let frame = FrameInfo {
                    duration,
                    size: payload.len() as u32,
                    is_sync: true,
                    cts_offset: 0,
                };
                write_frame_fragment(&mut seg, ttid, &mut progress, &frame, &payload).unwrap();
                text_durations.push(duration);
                text_payloads.push(payload);
            }
            ordered.push((ttid, seg));
        }
        TextStream {
            catalog,
            text_track_id: ttid,
            ordered,
            text_payloads,
            text_durations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::build_init_segment;
    use std::io::Cursor;

    fn read_fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("samples/fixtures/{name}"))
            .unwrap_or_else(|_| panic!("samples/fixtures/{name} must exist for tests"))
    }

    /// Segment a fixture, then re-assemble the canonical segments in
    /// interleave order (per GoP, tracks ascending) — the same layout the
    /// fMP4/flat writers and `cmd_segment_fmp4` use.
    fn segments_in_order(data: &[u8]) -> (Catalog, Vec<(u32, Vec<u8>)>) {
        let mut gops = Vec::new();
        let catalog = crate::segment::segment_fmp4(&mut Cursor::new(data), |gop| {
            gops.push(gop);
            Ok(())
        })
        .unwrap();
        let mut ordered = Vec::new();
        for gop in &gops {
            for (&tid, bytes) in &gop.tracks {
                ordered.push((tid, bytes.clone()));
            }
        }
        (catalog, ordered)
    }

    #[test]
    fn unwrap_fmp4_recovers_segments_verbatim() {
        let data = read_fixture("h264-opus-frag.mp4");
        let (catalog, ordered) = segments_in_order(&data);

        // Build a MUXL fMP4: init + canonical segments in interleave order.
        let mut fmp4 = build_init_segment(&catalog).unwrap();
        for (_, seg) in &ordered {
            fmp4.extend_from_slice(seg);
        }

        let recovered = unwrap(&fmp4).unwrap();
        assert_eq!(recovered.len(), ordered.len(), "segment count mismatch");
        for (rec, (tid, seg)) in recovered.iter().zip(ordered.iter()) {
            assert_eq!(rec.track_id, *tid, "track id mismatch");
            assert_eq!(rec.data, seg.as_slice(), "segment bytes must be verbatim");
        }
    }

    #[test]
    fn unwrap_bare_m4s_stream_recovers_segments() {
        let data = read_fixture("h264-opus-frag.mp4");
        let (_catalog, ordered) = segments_in_order(&data);

        // No ftyp/moov framing at all — just concatenated canonical segments.
        let mut stream = Vec::new();
        for (_, seg) in &ordered {
            stream.extend_from_slice(seg);
        }

        let recovered = unwrap(&stream).unwrap();
        assert_eq!(recovered.len(), ordered.len());
        for (rec, (tid, seg)) in recovered.iter().zip(ordered.iter()) {
            assert_eq!(rec.track_id, *tid);
            assert_eq!(rec.data, seg.as_slice());
        }
    }

    #[test]
    fn unwrap_flat_mp4_descends_envelope() {
        // A flat MP4's outer mdat payload is the verbatim segment stream;
        // unwrapping it then concatenating must reproduce the payload exactly.
        let data = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&data).unwrap();
        let mut flat = Vec::new();
        let info = crate::flat::write(&source, &data, &mut flat).unwrap();

        let recovered = unwrap(&flat).unwrap();
        assert!(!recovered.is_empty(), "should recover segments from flat MP4");

        let envelope = &flat[info.mdat_payload_offset as usize..];
        let joined: Vec<u8> = recovered.iter().flat_map(|s| s.data.iter().copied()).collect();
        assert_eq!(joined.as_slice(), envelope, "segments must reconstruct the mdat envelope verbatim");

        // Each recovered segment is itself a valid canonical segment.
        for s in &recovered {
            assert_eq!(catalog::from_segment(s.data).unwrap(), s.catalog);
        }
    }

    #[test]
    fn unwrap_keeps_c2pa_prefix_with_segment() {
        // Simulate a signed stream: prepend a fake c2pa uuid box to each
        // canonical segment. unwrap must keep [c2pa][muxl][frags] together.
        let data = read_fixture("h264-opus-frag.mp4");
        let (_catalog, ordered) = segments_in_order(&data);

        let fake_c2pa = |body: &[u8]| {
            let total = 24 + body.len();
            let mut b = Vec::new();
            b.extend_from_slice(&(total as u32).to_be_bytes());
            b.extend_from_slice(b"uuid");
            b.extend_from_slice(&[0xd8u8; 16]); // non-MUXL usertype
            b.extend_from_slice(body);
            b
        };

        let mut stream = Vec::new();
        let mut signed = Vec::new();
        for (i, (_, seg)) in ordered.iter().enumerate() {
            let prefix = fake_c2pa(format!("manifest {i}").as_bytes());
            let mut s = prefix.clone();
            s.extend_from_slice(seg);
            stream.extend_from_slice(&s);
            signed.push(s);
        }

        let recovered = unwrap(&stream).unwrap();
        assert_eq!(recovered.len(), ordered.len());
        for (rec, expected) in recovered.iter().zip(signed.iter()) {
            assert_eq!(rec.data, expected.as_slice(), "c2pa prefix must stay with its segment");
            assert!(rec.data.starts_with(&[0, 0, 0]), "segment should start with the c2pa uuid box");
        }
    }

    #[test]
    fn unwrap_skips_wrapper_c2pa_before_moov() {
        // sign_per_track output shape: ftyp + c2pa-uuid(wrapper) + moov +
        // mdat{segments}. unwrap must skip the wrapper uuid (it sits before
        // moov, followed by moov not a muxl uuid) and still descend the mdat
        // envelope — unlike a signed segment's c2pa prefix, which precedes a
        // MUXL uuid.
        let data = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&data).unwrap();
        let mut flat = Vec::new();
        crate::flat::write(&source, &data, &mut flat).unwrap();
        let baseline = unwrap(&flat).unwrap();

        // Splice a fake (non-MUXL) wrapper uuid box in right after ftyp.
        let (kind, _bs, ftyp_end) = read_box_header(&flat, 0).unwrap();
        assert_eq!(&kind, b"ftyp");
        let wrapper = {
            let body = b"fake wrapper manifest";
            let total = 24 + body.len();
            let mut b = Vec::new();
            b.extend_from_slice(&(total as u32).to_be_bytes());
            b.extend_from_slice(b"uuid");
            b.extend_from_slice(&[0xd8u8; 16]); // non-MUXL usertype
            b.extend_from_slice(body);
            b
        };
        let mut wrapped = Vec::new();
        wrapped.extend_from_slice(&flat[..ftyp_end]);
        wrapped.extend_from_slice(&wrapper);
        wrapped.extend_from_slice(&flat[ftyp_end..]);

        let recovered = unwrap(&wrapped).unwrap();
        assert_eq!(
            recovered.len(),
            baseline.len(),
            "wrapper uuid must not change the recovered segment count"
        );
        for (r, b) in recovered.iter().zip(baseline.iter()) {
            assert_eq!(r.data, b.data, "segments recovered verbatim past the wrapper box");
        }
    }

    #[test]
    fn segment_events_reconstructs_event_stream() {
        use crate::cbor::CborEvent;
        // Build a MUXL fMP4 (init + canonical segments), then re-derive the
        // event stream from it: one Init, then one Segment per GoP with
        // verbatim bytes + recomputed durations/sample counts.
        let data = read_fixture("h264-opus-frag.mp4");
        let (catalog, ordered) = segments_in_order(&data);
        let mut fmp4 = build_init_segment(&catalog).unwrap();
        for (_, seg) in &ordered {
            fmp4.extend_from_slice(seg);
        }

        let events = segment_events(&fmp4).unwrap();
        assert!(matches!(events[0], CborEvent::Init { .. }), "first event is Init");

        let mut seg_count = 0;
        let mut byte_total = 0u64;
        for e in &events {
            if let CborEvent::Segment { durations, sample_counts, body_size, .. } = e {
                seg_count += 1;
                byte_total += body_size;
                assert!(!durations.is_empty(), "segment has tracks");
                for d in durations.values() {
                    assert!(*d > 0, "duration must be non-zero");
                }
                for c in sample_counts.values() {
                    assert!(*c > 0, "sample count must be non-zero");
                }
            }
        }
        assert!(seg_count >= 2, "fixture should yield multiple GoPs, got {seg_count}");
        let expected: u64 = ordered.iter().map(|(_, b)| b.len() as u64).sum();
        assert_eq!(byte_total, expected, "verbatim segment bytes preserved");
    }

    /// A `Read` that hands out at most `chunk` bytes per call, so a single box
    /// header/body is split across many reads — exercising the streaming
    /// scanner's cross-read reassembly.
    struct ChunkReader<'a> {
        data: &'a [u8],
        pos: usize,
        chunk: usize,
    }
    impl std::io::Read for ChunkReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = (self.data.len() - self.pos).min(self.chunk).min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn drisl_slurp(wrapper: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        segment_events_streaming(wrapper, |ev| {
            dasl::drisl::to_writer(&mut out, &ev)
                .map_err(|e| Error::InvalidMp4(e.to_string()))?;
            Ok(())
        })
        .unwrap();
        out
    }

    fn drisl_stream(wrapper: &[u8], chunk: usize) -> Vec<u8> {
        let mut out = Vec::new();
        segment_events_stream(
            ChunkReader { data: wrapper, pos: 0, chunk },
            |ev| {
                dasl::drisl::to_writer(&mut out, &ev)
                    .map_err(|e| Error::InvalidMp4(e.to_string()))?;
                Ok(())
            },
        )
        .unwrap();
        out
    }

    /// The fully-streaming `segment_events_stream` must emit byte-identical
    /// DRISL to the slurp-based `segment_events` for a MUXL fMP4 — regardless of
    /// how the input is chunked across reads (1 byte at a time stresses the
    /// box-header reassembly the hardest).
    #[test]
    fn stream_events_match_slurp_fmp4() {
        let data = read_fixture("h264-opus-frag.mp4");
        let (catalog, ordered) = segments_in_order(&data);
        let mut fmp4 = build_init_segment(&catalog).unwrap();
        for (_, seg) in &ordered {
            fmp4.extend_from_slice(seg);
        }

        let slurp = drisl_slurp(&fmp4);
        assert!(!slurp.is_empty(), "fixture must produce events");
        for chunk in [1usize, 7, 4096, usize::MAX] {
            assert_eq!(
                drisl_stream(&fmp4, chunk),
                slurp,
                "stream (chunk={chunk}) must equal slurp for fMP4"
            );
        }
    }

    /// Same equivalence for a bare m4s stream (no ftyp/moov framing).
    #[test]
    fn stream_events_match_slurp_bare_m4s() {
        let data = read_fixture("h264-opus-frag.mp4");
        let (_catalog, ordered) = segments_in_order(&data);
        let mut stream = Vec::new();
        for (_, seg) in &ordered {
            stream.extend_from_slice(seg);
        }
        assert_eq!(drisl_stream(&stream, 5), drisl_slurp(&stream));
    }

    /// Same equivalence for a flat MP4 — the scanner must descend the outer
    /// `mdat` envelope rather than skip it.
    #[test]
    fn stream_events_match_slurp_flat() {
        let data = read_fixture("h264-opus-frag.mp4");
        let source = crate::read(&data).unwrap();
        let mut flat = Vec::new();
        crate::flat::write(&source, &data, &mut flat).unwrap();
        assert_eq!(drisl_stream(&flat, 3), drisl_slurp(&flat));
    }

    /// Same equivalence for signed segments: a non-MUXL c2pa `uuid` prefix must
    /// stay attached to the segment it heads (the scanner starts a stream at a
    /// non-MUXL uuid only when the next box is a MUXL uuid).
    #[test]
    fn stream_events_match_slurp_signed_segments() {
        let data = read_fixture("h264-opus-frag.mp4");
        let (catalog, ordered) = segments_in_order(&data);
        let fake_c2pa = |body: &[u8]| {
            let total = 24 + body.len();
            let mut b = Vec::new();
            b.extend_from_slice(&(total as u32).to_be_bytes());
            b.extend_from_slice(b"uuid");
            b.extend_from_slice(&[0xd8u8; 16]); // non-MUXL usertype
            b.extend_from_slice(body);
            b
        };
        let mut fmp4 = build_init_segment(&catalog).unwrap();
        for (i, (_, seg)) in ordered.iter().enumerate() {
            fmp4.extend_from_slice(&fake_c2pa(format!("manifest {i}").as_bytes()));
            fmp4.extend_from_slice(seg);
        }
        assert_eq!(drisl_stream(&fmp4, 9), drisl_slurp(&fmp4));
    }

    #[test]
    fn unwrap_rejects_non_segment_stream() {
        // A moof with no preceding uuid is not a canonical segment stream.
        let bytes = b"\x00\x00\x00\x08moof".to_vec();
        assert!(unwrap(&bytes).is_err());
    }

    /// Build a flat MP4 (ftyp+moov+mdat envelope) from a fixture, the shape a
    /// finalized VOD blob has, plus the fragment-relative offset of each
    /// canonical segment within it.
    fn flat_with_offsets(fixture: &str) -> (Vec<u8>, Vec<(Vec<u8>, u64)>) {
        let data = read_fixture(fixture);
        let (catalog, ordered) = segments_in_order(&data);
        let slices: Vec<&[u8]> = ordered.iter().map(|(_, s)| s.as_slice()).collect();
        let mut flat = Vec::new();
        crate::present::write_flat_from_m4s(&catalog, &slices, &mut flat).unwrap();

        let mut offsets = Vec::new();
        let mut running = 0u64;
        for (_, seg) in &ordered {
            offsets.push((seg.clone(), running));
            running += seg.len() as u64;
        }
        (flat, offsets)
    }

    #[test]
    fn read_segments_at_indexes_a_flat_blob_by_fragment_offset() {
        let (flat, offsets) = flat_with_offsets("h264-opus-frag.mp4");

        // The stream starts past ftyp+moov+the mdat box header — exactly the
        // base a caller must NOT have to compute for itself.
        let base = segment_stream_start(&flat).unwrap();
        assert!(base > 0, "a flat MP4 displaces its fragments");
        assert_eq!(
            base as usize + offsets.iter().map(|(s, _)| s.len()).sum::<usize>(),
            flat.len(),
            "fragments must fill the mdat envelope exactly"
        );

        // Every segment is recoverable, verbatim, from its offset alone — no
        // length passed in; muxl derives the extent from the uuid boundaries.
        for (want, offset) in &offsets {
            let got = read_segments_at(&flat, SegmentOffset::Stream(*offset), Some(1)).unwrap();
            assert_eq!(&got, want, "segment at fragment offset {offset}");
        }

        // Several at once are returned contiguously, in stream order.
        let first_three: Vec<u8> = offsets[..3].iter().flat_map(|(s, _)| s.clone()).collect();
        assert_eq!(read_segments_at(&flat, SegmentOffset::Stream(0), Some(3)).unwrap(), first_three);

        // No count reads to the end of the stream.
        let all: Vec<u8> = offsets.iter().flat_map(|(s, _)| s.clone()).collect();
        assert_eq!(read_segments_at(&flat, SegmentOffset::Stream(0), None).unwrap(), all);
    }

    #[test]
    fn read_segments_at_rejects_offsets_that_are_not_fragment_relative() {
        let (flat, offsets) = flat_with_offsets("h264-opus-frag.mp4");
        let base = segment_stream_start(&flat).unwrap();

        // Passing an *absolute* blob offset — the mistake that reads garbage
        // when a caller adds the flat-header size itself — must fail loudly
        // rather than return whatever bytes happen to live there.
        let (_, mid) = &offsets[offsets.len() / 2];
        assert!(
            read_segments_at(&flat, SegmentOffset::Stream(base + mid), Some(1)).is_err(),
            "an absolute offset must not be mistaken for a fragment offset"
        );

        // That same absolute offset is correct when declared as one.
        assert_eq!(
            read_segments_at(&flat, SegmentOffset::File(base + mid), Some(1)).unwrap(),
            offsets[offsets.len() / 2].0,
            "SegmentOffset::File must not add the stream base"
        );

        // Landing inside a box is likewise an error, not silent garbage.
        assert!(read_segments_at(&flat, SegmentOffset::Stream(4), Some(1)).is_err());

        // Asking for more segments than remain is an error, not a short read.
        assert!(read_segments_at(&flat, SegmentOffset::Stream(0), Some(offsets.len() as u64 + 1)).is_err());
    }

    #[test]
    fn segment_stream_start_handles_64bit_mdat_largesize() {
        // A body over 4 GiB forces the `size == 1` + 64-bit largesize form, so
        // the payload starts 16 bytes in rather than 8. Real blobs that hit
        // this are too big for a test, so build the framing by hand around a
        // real segment stream.
        let (flat, offsets) = flat_with_offsets("h264-opus-frag.mp4");
        let base = segment_stream_start(&flat).unwrap() as usize;
        let body = &flat[base..];

        let mut blob = Vec::new();
        blob.extend_from_slice(b"\x00\x00\x00\x10ftypmuxltest");
        blob.extend_from_slice(&1u32.to_be_bytes()); // size == 1 → largesize
        blob.extend_from_slice(b"mdat");
        blob.extend_from_slice(&((body.len() + 16) as u64).to_be_bytes());
        let header_len = blob.len() as u64;
        blob.extend_from_slice(body);

        assert_eq!(
            segment_stream_start(&blob).unwrap(),
            header_len,
            "largesize mdat payload starts 16 bytes past the box start"
        );
        // And the offsets still resolve against it.
        let (want, offset) = &offsets[offsets.len() / 2];
        assert_eq!(&read_segments_at(&blob, SegmentOffset::Stream(*offset), Some(1)).unwrap(), want);
    }

    fn fmp4_of(catalog: &Catalog, ordered: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut fmp4 = build_init_segment(catalog).unwrap();
        for (_, seg) in ordered {
            fmp4.extend_from_slice(seg);
        }
        fmp4
    }

    #[test]
    fn catalog_tracks_enumerates_every_type() {
        let ts = text_fixture::av_with_text("h264-opus-frag.mp4", 0);
        let tracks: Vec<CatalogTrack> = catalog_tracks(&ts.catalog).collect();
        assert_eq!(tracks.len(), 3);
        let text = catalog_track(&ts.catalog, ts.text_track_id).unwrap();
        assert!(text.is_text);
        assert_eq!(text.timescale, 1000);
        assert_eq!(tracks.iter().filter(|t| t.is_text).count(), 1);
        assert!(catalog_track(&ts.catalog, 999).is_none());
    }

    #[test]
    fn unwrap_recovers_text_segments_verbatim() {
        let ts = text_fixture::av_with_text("h264-opus-frag.mp4", 0);
        let fmp4 = fmp4_of(&ts.catalog, &ts.ordered);

        let recovered = unwrap(&fmp4).unwrap();
        assert_eq!(recovered.len(), ts.ordered.len());
        let mut text_segments = 0;
        for (rec, (tid, seg)) in recovered.iter().zip(ts.ordered.iter()) {
            assert_eq!(rec.track_id, *tid);
            assert_eq!(rec.data, seg.as_slice(), "segment bytes must be verbatim");
            if *tid == ts.text_track_id {
                text_segments += 1;
                assert_eq!(rec.catalog, ts.catalog.filter_to_track(*tid));
            }
        }
        assert!(text_segments >= 2, "expected a text segment per GoP");

        // The aggregate catalog carries the text rendition next to AV.
        let agg = aggregate_catalog(&recovered);
        assert_eq!(agg.text, ts.catalog.text);
        assert_eq!(agg.video_configs().count(), 1);
        assert_eq!(agg.audio_configs().count(), 1);
    }

    #[test]
    fn segment_events_carry_text_without_stretching_gop_duration() {
        use crate::cbor::CborEvent;
        let data = read_fixture("h264-opus-frag.mp4");
        let (av_catalog, av_ordered) = segments_in_order(&data);
        let av_events = segment_events(&fmp4_of(&av_catalog, &av_ordered)).unwrap();

        // The last GoP's text runs 500 ms past the AV content. Text spans
        // follow the AV GoP, so they must not change any GoP's duration_us.
        let ts = text_fixture::av_with_text("h264-opus-frag.mp4", 500);
        let events = segment_events(&fmp4_of(&ts.catalog, &ts.ordered)).unwrap();
        let key = ts.text_track_id.to_string();

        let inits: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                CborEvent::Init { catalog, track_inits, .. } => Some((catalog, track_inits)),
                _ => None,
            })
            .collect();
        assert_eq!(inits.len(), 1, "text in every GoP needs exactly one Init");
        assert_eq!(inits[0].0.as_ref().unwrap().text, ts.catalog.text);
        assert!(inits[0].1.contains_key(&key), "per-track init for the text track");

        let durs = |evs: &[CborEvent]| -> Vec<u64> {
            evs.iter()
                .filter_map(|e| match e {
                    CborEvent::Segment { duration_us, .. } => Some(*duration_us),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(durs(&events), durs(&av_events));

        let mut text_samples = 0u32;
        for e in &events {
            if let CborEvent::Segment { tracks, durations, sample_counts, .. } = e {
                assert!(tracks.contains_key(&key), "every GoP carries the text track");
                assert!(durations[&key] > 0);
                text_samples += sample_counts[&key];
            }
        }
        assert_eq!(text_samples as usize, ts.text_payloads.len());
    }

    #[test]
    fn stream_events_match_slurp_with_text() {
        let ts = text_fixture::av_with_text("h264-opus-frag.mp4", 0);
        let fmp4 = fmp4_of(&ts.catalog, &ts.ordered);
        let slurp = drisl_slurp(&fmp4);
        for chunk in [1usize, 7, usize::MAX] {
            assert_eq!(drisl_stream(&fmp4, chunk), slurp, "chunk={chunk}");
        }
    }

    #[test]
    fn segment_events_for_text_only_gop_use_text_duration() {
        use crate::cbor::CborEvent;
        // A text-only stream: the GoP duration falls back to the text span.
        let ts = text_fixture::av_with_text("h264-opus-frag.mp4", 0);
        let text_only: Vec<(u32, Vec<u8>)> = ts
            .ordered
            .iter()
            .filter(|(tid, _)| *tid == ts.text_track_id)
            .cloned()
            .collect();
        let catalog = ts.catalog.filter_to_track(ts.text_track_id);
        let events = segment_events(&fmp4_of(&catalog, &text_only)).unwrap();
        let mut total_us = 0u64;
        for e in &events {
            if let CborEvent::Segment { duration_us, .. } = e {
                assert!(*duration_us > 0);
                total_us += duration_us;
            }
        }
        let total_ms: u64 = ts.text_durations.iter().map(|&d| d as u64).sum();
        assert_eq!(total_us, total_ms * 1000);
    }
    #[test]
    fn text_before_av_groups_by_reference_and_reemits_changed_config() {
        use crate::cbor::CborEvent;
        let input = read_fixture("h264-opus-frag.mp4");
        let mut gops = Vec::new();
        let catalog = crate::segment_fmp4(&mut std::io::Cursor::new(input),|g| { gops.push(g);Ok(()) }).unwrap();
        let mut bytes = Vec::new();
        for (epoch,index) in [0usize,1,0].into_iter().enumerate() {
            let req = crate::text::StreamingText::request(&catalog,&gops[index]).unwrap();
            let config = crate::catalog::TextConfig {
                codec:"wvtt".into(),container:crate::catalog::Container::cmaf(1000,10),
                language:if epoch==2 {"es"} else {"en"}.into(),
                label:Some(if epoch==2 {"human"} else {"auto"}.into()),config:"WEBVTT".into(),
            };
            let cue = crate::text::Cue { start:req.start_ms,end:req.end_ms,text:format!("epoch {epoch}"),id:Some(format!("e{epoch}")),settings:None };
            bytes.extend(crate::text::build_track(&config,&[cue],req.start_ms,req.end_ms).unwrap());
            for data in gops[index].tracks.values() { bytes.extend_from_slice(data); }
        }
        let events = segment_events(&bytes).unwrap();
        let mut groups = Vec::new();
        let mut configs = Vec::new();
        for event in events { match event {
            CborEvent::Init {catalog:Some(catalog),..} => configs.push(catalog.text_configs().next().unwrap().clone()),
            CborEvent::Segment {tracks,first_decode_times,..} => {
                assert!(tracks.contains_key("1") && tracks.contains_key("2") && tracks.contains_key("10"));
                let cues = crate::text::cues_from_fragments(&tracks["10"].0,1000).unwrap();
                groups.push((first_decode_times["1"],cues[0].text.clone()));
            },
            _ => {},
        } }
        assert_eq!(groups.len(),3);
        assert_eq!(groups[0].1,"epoch 0");
        assert_eq!(groups[1].1,"epoch 1");
        assert_eq!(groups[2].1,"epoch 2");
        assert_eq!(groups[0].0,groups[2].0);
        assert_eq!(configs.len(),2);
        assert_eq!(configs[0].language,"en");
        assert_eq!(configs[1].language,"es");
        assert_eq!(configs[1].label.as_deref(),Some("human"));
    }
    #[test]
    fn sparse_av_flat_blobs_preserve_legacy_groups_and_all_bytes() {
        use crate::cbor::CborEvent;
        let input = read_fixture("h264-opus-frag.mp4");
        let (catalog, ordered) = segments_in_order(&input);
        assert_eq!(ordered.iter().map(|(id,_)| *id).collect::<Vec<_>>(),vec![1,2,1,2]);
        // Track-major storage, a leading audio fragment, and repeated trailing
        // audio were all accepted before timed-text grouping was introduced.
        for layout in [vec![0usize,2,1,3],vec![1,0,3,2],vec![0,1,3,1]] {
            let mut expected = Vec::new();
            let mut current = std::collections::BTreeMap::new();
            let mut last = 0;
            let mut body = Vec::new();
            for &index in &layout {
                let (tid,data) = &ordered[index];
                if *tid <= last && !current.is_empty() {
                    expected.push(std::mem::take(&mut current));
                }
                current.insert(tid.to_string(),data.clone());
                body.extend_from_slice(data);
                last = *tid;
            }
            expected.push(current);
            // The reader descends the flat blob's outer mdat without relying
            // on sample tables. Use the same header/envelope shape as storage.
            let mut blob = build_init_segment(&catalog).unwrap();
            blob.extend_from_slice(&1u32.to_be_bytes());
            blob.extend_from_slice(b"mdat");
            blob.extend_from_slice(&(16+body.len() as u64).to_be_bytes());
            blob.extend_from_slice(&body);
            let actual: Vec<std::collections::BTreeMap<String,Vec<u8>>> = segment_events(&blob).unwrap().into_iter()
                .filter_map(|e| match e { CborEvent::Segment {tracks,..} => Some(tracks.into_iter().map(|(id,data)| (id,data.0)).collect()), _=>None }).collect();
            assert_eq!(actual,expected,"layout {layout:?} must retain legacy AV grouping and verbatim bytes");
            for chunk in [1,7,usize::MAX] {
                assert_eq!(drisl_stream(&blob,chunk),drisl_slurp(&blob));
            }
        }
    }

    #[test]
    fn trailing_text_without_another_av_reference_is_preserved() {
        use crate::cbor::CborEvent;
        let input = read_fixture("h264-opus-frag.mp4");
        let (catalog,ordered) = segments_in_order(&input);
        let mut blob = build_init_segment(&catalog).unwrap();
        for (_,data) in ordered { blob.extend(data); }
        let config = crate::catalog::TextConfig {
            codec:"wvtt".into(),container:crate::catalog::Container::cmaf(1000,100),
            language:"en".into(),label:Some("human".into()),config:"WEBVTT".into(),
        };
        let mut expected = Vec::new();
        for start in [10_000,11_000] {
            let cue = crate::text::Cue {start,end:start+1000,text:format!("tail {start}"),id:None,settings:None};
            let track = crate::text::build_track(&config,&[cue],start,start+1000).unwrap();
            blob.extend_from_slice(&track);
            expected.push(track);
        }
        let actual: Vec<_> = segment_events(&blob).unwrap().into_iter()
            .filter_map(|e| match e { CborEvent::Segment {mut tracks,..} => tracks.remove("100").map(|data|data.0), _=>None }).collect();
        assert_eq!(actual,expected,"trailing text fragments must neither be rejected nor overwrite each other");
        assert_eq!(drisl_stream(&blob,7),drisl_slurp(&blob));
    }
}
