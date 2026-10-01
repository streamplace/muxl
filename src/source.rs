//! `Source` and `Plan` — the wrapper-agnostic in-memory view of an MP4
//! input that any of the three codec representations (CBOR catalog, fMP4
//! init, flat MP4 header) can be produced from.
//!
//! A source carries two things:
//!
//! - [`Catalog`] — codec configuration for every track (what's the codec,
//!   dimensions, sample rate, timescale, track id). See `src/catalog.rs`.
//! - [`Plan`] — the per-sample layout: durations, sizes, sync flags, cts
//!   offsets, and the sample's byte offset in the *input*. No sample bytes
//!   live in a `Plan`; they're streamed from the input at write time.
//!
//! Because a `Plan` stores only metadata, even a 24-hour source sits at a
//! bounded memory cost (≈24 B/sample; ~120 MB for a 24 h/60 fps video).
//! Sample payload is always read on-demand from the original input, so
//! write paths are streaming from the input side and from the output side.
//!
//! Every reader (`muxl::read`, `fmp4::read`, `flat::read`) returns a
//! `Source`; every writer (`fmp4::write`, `flat::write`) takes one.
//! Convert flat → fMP4 is `fmp4::write(&flat::read(input)?, input, out)`.

use crate::catalog::Catalog;

/// In-memory view of an MP4 input — catalog plus a sample plan that can
/// be re-emitted into any wrapper.
#[derive(Debug, Clone)]
pub struct Source {
    /// Codec configuration for every track.
    pub catalog: Catalog,
    /// Per-track sample plan.
    pub plan: Plan,
}

impl Source {
    /// Return a new `Source` whose catalog and plan contain only the
    /// requested track. Useful for emitting per-track flat MP4s from a
    /// multi-track input — the resulting source can be passed to
    /// [`crate::flat::write`] verbatim.
    ///
    /// Returns `None` if no track has the given id.
    pub fn filter_to_track(&self, track_id: u32) -> Option<Source> {
        let track = self.plan.track(track_id)?.clone();
        Some(Source {
            catalog: self.catalog.filter_to_track(track_id),
            plan: Plan::new(vec![track]),
        })
    }

    /// Remap track IDs across the catalog and the per-track sample plan in
    /// place. Both the canonical moov (`tkhd`/`trex`) and every minted
    /// `moof` (`tfhd`) take their track id from these, so the rewrite
    /// propagates to all emitted bytes when the source is written via
    /// [`crate::fmp4::write`]. Sample byte offsets are unaffected (they're
    /// file positions, independent of track id), so the streamed sample data
    /// is unchanged. See [`crate::catalog::Catalog::remap_track_ids`].
    pub fn remap_track_ids(&mut self, map: &std::collections::BTreeMap<u32, u32>) {
        self.catalog.remap_track_ids(map);
        for plan in &mut self.plan.tracks {
            if let Some(&new) = map.get(&plan.track_id) {
                plan.track_id = new;
            }
        }
        self.plan.tracks.sort_by_key(|t| t.track_id);
    }
}

/// Per-track sample plans in track-id order.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub tracks: Vec<TrackPlan>,
}

impl Plan {
    /// Sort tracks by `track_id`. The resulting `Plan` preserves each
    /// track's `start_offset_ticks` verbatim — A/V sync rides on the
    /// per-track delta, and the absolute time anchoring is whatever the
    /// source had (live-stream segments retain their cumulative-from-
    /// stream-start tfdts; standalone files at non-zero anchors keep that
    /// too). Callers who specifically want a "shift smallest to zero"
    /// transform should apply it explicitly, not via canonicalization.
    pub fn new(tracks: Vec<TrackPlan>) -> Self {
        let mut tracks = tracks;
        tracks.sort_by_key(|t| t.track_id);
        Self { tracks }
    }

    /// Find a track plan by `track_id`.
    pub fn track(&self, track_id: u32) -> Option<&TrackPlan> {
        self.tracks.iter().find(|t| t.track_id == track_id)
    }
}

/// One track's sample plan — metadata only, no sample bytes.
#[derive(Debug, Clone)]
pub struct TrackPlan {
    pub track_id: u32,
    /// `true` for video tracks, `false` for audio/other.
    pub is_video: bool,
    /// Media timescale (ticks per second) — matches the track's `mdhd`.
    pub timescale: u32,
    /// Presentation start offset in the track's media timescale. Baked
    /// into the first fragment's `tfdt` on write, and into a synthesized
    /// canonical `elst` for the flat MP4 moov. Source file leading
    /// empty-edit → this value. See `spec/canonical-form.md § edts/elst`.
    pub start_offset_ticks: u64,
    /// Samples in decode order.
    pub samples: Vec<Sample>,
}

/// Per-sample metadata. 24 B/sample (plus align) — a 24 h/60 fps video
/// is ~120 MB of `Sample` records.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Sample duration in the track's media timescale.
    pub duration: u32,
    /// Encoded sample size in bytes.
    pub size: u32,
    /// Sync (key) frame flag.
    pub is_sync: bool,
    /// Composition-time offset (decode time → presentation time) in the
    /// track's media timescale. Zero for audio and for video without
    /// B-frames.
    pub cts_offset: i32,
    /// Byte offset of this sample's encoded data in the *original* input.
    /// The writer streams these bytes through via `ReadAt::read_at`.
    pub input_offset: u64,
}

/// Canonicalized text payloads are appended virtually to the input. AV data
/// remains random-access and is never copied or buffered by this adapter.
pub(crate) struct TextInput<'a, R: crate::io::ReadAt + ?Sized> {
    input: &'a R,
    base: u64,
    text: Vec<u8>,
}

impl<R: crate::io::ReadAt + ?Sized> crate::io::ReadAt for TextInput<'_, R> {
    fn size(&self) -> std::io::Result<u64> { Ok(self.base + self.text.len() as u64) }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if offset < self.base { return self.input.read_at(offset, buf); }
        let pos = (offset-self.base) as usize;
        if pos >= self.text.len() { return Ok(0); }
        let count = buf.len().min(self.text.len()-pos);
        buf[..count].copy_from_slice(&self.text[pos..pos+count]);
        Ok(count)
    }
}

/// Normalize text samples on all source-based minting paths. GoP bounds are
/// derived from the AV reference and floored to milliseconds.
pub(crate) fn normalize_text<'a, R: crate::io::ReadAt + ?Sized>(
    source: &Source, input: &'a R,
) -> crate::Result<(Source, TextInput<'a,R>)> {
    use crate::segment::ticks_to_ms;
    let mut result = source.clone();
    let mut payloads = TextInput { input, base: input.size()?, text: Vec::new() };
    let reference = source.plan.tracks.iter().filter(|p| p.is_video).min_by_key(|p|p.track_id)
        .or_else(|| source.plan.tracks.iter().filter(|p| !source.catalog.text_configs().any(|t| t.track_id()==p.track_id)).min_by_key(|p|p.track_id))
        .or_else(|| source.plan.tracks.iter().min_by_key(|p|p.track_id));
    let Some(reference) = reference else { return Ok((result,payloads)); };
    let mut spans = Vec::new();
    let mut dt = reference.start_offset_ticks;
    let mut start = ticks_to_ms(dt,reference.timescale);
    let mut duration = 0u64;
    let text_only = source.catalog.text_configs().any(|t|t.track_id()==reference.track_id);
    for (i,sample) in reference.samples.iter().enumerate() {
        if i>0 && ((reference.is_video && sample.is_sync) || (!reference.is_video && !text_only && duration >= reference.timescale as u64)) {
            let end=ticks_to_ms(dt,reference.timescale);
            if end>start { spans.push((start,end)); }
            start=end;duration=0;
        }
        dt+=sample.duration as u64;duration+=sample.duration as u64;
    }
    let end=ticks_to_ms(dt,reference.timescale);
    if text_only {
        while start < end { let next=(start+1000).min(end); spans.push((start,next));start=next; }
    } else if end>start { spans.push((start,end)); }
    for config in source.catalog.text_configs() {
        let plan=source.plan.track(config.track_id()).ok_or_else(|| crate::Error::InvalidMp4("text track missing plan".into()))?;
        let mut cues=Vec::new();
        let mut dt=plan.start_offset_ticks;
        let mut bytes=Vec::new();
        for sample in &plan.samples {
            bytes.resize(sample.size as usize,0);
            input.read_exact_at(sample.input_offset,&mut bytes)?;
            cues.extend(crate::text::cues_from_sample(&bytes,ticks_to_ms(dt,plan.timescale),ticks_to_ms(dt+sample.duration as u64,plan.timescale))?);
            dt+=sample.duration as u64;
        }
        let cues=crate::text::merge_cues(cues);
        let mut samples=Vec::new();
        for &(start,end) in &spans {
            for sample in crate::text::canonical_samples(&cues,start,end)? {
                samples.push(Sample {duration:sample.duration,size:sample.data.len() as u32,is_sync:true,cts_offset:0,input_offset:payloads.base+payloads.text.len() as u64});
                payloads.text.extend_from_slice(&sample.data);
            }
        }
        let normalized=result.plan.tracks.iter_mut().find(|p|p.track_id==config.track_id()).unwrap();
        normalized.timescale=1000;
        normalized.start_offset_ticks=spans.first().map_or(0,|s|s.0);
        normalized.samples=samples;
    }
    crate::segment::normalize_text_catalog(&mut result.catalog);
    Ok((result,payloads))
}
