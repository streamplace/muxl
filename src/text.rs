//! ISO/IEC 14496-30 WebVTT samples and canonical GoP text runs.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use mp4_atom::{Header, Moof, ReadAtom, ReadFrom};
use serde::{Deserialize, Serialize};
use crate::{Error, Result};
use crate::catalog::{Catalog, TextConfig};
use crate::fragment::TrackProgress;

/// Cue timestamps are absolute milliseconds; end is exclusive.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cue {
    pub start: u64,
    pub end: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<String>,
}

pub struct TextSample {
    pub start: u64,
    pub duration: u32,
    pub data: Vec<u8>,
}

fn invalid(message: &str) -> Error { Error::InvalidMp4(message.into()) }
fn box_bytes(kind: &[u8; 4], body: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let size = u32::try_from(body.len().checked_add(8).ok_or_else(|| invalid("text box too large"))?)
        .map_err(|_| invalid("text box too large"))?;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    Ok(())
}

/// Canonical active-cue intervals, including empty intervals. Input order does
/// not matter. Adjacent equivalent active sets are coalesced.
pub fn canonical_samples(cues: &[Cue], start: u64, end: u64) -> Result<Vec<TextSample>> {
    if end <= start { return Err(invalid("text range must have positive duration")); }
    let mut points = BTreeSet::from([start, end]);
    for cue in cues {
        if cue.end < cue.start { return Err(invalid("cue end precedes start")); }
        if cue.start < end && cue.end > start && cue.end > cue.start {
            points.insert(cue.start.max(start));
            points.insert(cue.end.min(end));
        }
    }
    let points: Vec<_> = points.into_iter().collect();
    let mut out: Vec<TextSample> = Vec::new();
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let mut active: Vec<_> = cues.iter().filter(|c| c.start <= a && c.end >= b && c.end > c.start).collect();
        active.sort_by(|a,b| (&a.text, &a.id, &a.settings).cmp(&(&b.text, &b.id, &b.settings)));
        let mut data = Vec::new();
        if active.is_empty() { box_bytes(b"vtte", &[], &mut data)?; }
        for cue in active {
            let mut body = Vec::new();
            if let Some(id) = cue.id.as_ref().filter(|s| !s.is_empty()) { box_bytes(b"iden", id.as_bytes(), &mut body)?; }
            if let Some(settings) = cue.settings.as_ref().filter(|s| !s.is_empty()) { box_bytes(b"sttg", settings.as_bytes(), &mut body)?; }
            box_bytes(b"payl", cue.text.as_bytes(), &mut body)?;
            box_bytes(b"vttc", &body, &mut data)?;
        }
        let duration = u32::try_from(b-a).map_err(|_| invalid("text interval exceeds u32 milliseconds"))?;
        if let Some(previous) = out.last_mut().filter(|p| p.data == data && p.start + p.duration as u64 == a && p.duration.checked_add(duration).is_some()) {
            previous.duration += duration;
        } else { out.push(TextSample {start:a,duration,data}); }
    }
    Ok(out)
}

/// Decode the cue boxes in one sample. Unknown optional boxes are ignored.
pub fn cues_from_sample(data: &[u8], start: u64, end: u64) -> Result<Vec<Cue>> {
    let mut cues = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let (kind, body, next) = crate::reader::read_box_header(data, pos)?;
        match &kind {
            b"vtte" => { if body != next { return Err(invalid("vtte must be empty")); } }
            b"vttc" => {
                let mut cue = Cue {start,end,text:String::new(),id:None,settings:None};
                let mut found = false;
                let mut child = body;
                while child < next {
                    let (k, b, n) = crate::reader::read_box_header(&data[..next], child)?;
                    if matches!(&k, b"payl" | b"iden" | b"sttg") {
                        let value = std::str::from_utf8(&data[b..n]).map_err(|_| invalid("WebVTT box is not UTF-8"))?.to_owned();
                        match &k { b"payl" => { if found { return Err(invalid("duplicate payl")); } cue.text=value;found=true; }, b"iden" => cue.id=Some(value), b"sttg" => cue.settings=Some(value), _ => unreachable!() }
                    }
                    child=n;
                }
                if !found { return Err(invalid("vttc missing payl")); }
                if end > start { cues.push(cue); }
            }
            _ => return Err(invalid("unsupported WebVTT sample box")),
        }
        pos=next;
    }
    Ok(cues)
}

/// Merge contiguous pieces of a cue (including pieces split by overlaps or
/// GoP boundaries). Identical simultaneously active cues retain multiplicity.
pub fn merge_cues(cues: Vec<Cue>) -> Vec<Cue> {
    let mut groups: BTreeMap<(String,Option<String>,Option<String>),Vec<Cue>> = BTreeMap::new();
    for c in cues { groups.entry((c.text.clone(),c.id.clone(),c.settings.clone())).or_default().push(c); }
    let mut out = Vec::new();
    for (_, mut pieces) in groups {
        pieces.sort_by_key(|c| (c.start,c.end));
        let mut merged: Vec<Cue> = Vec::new();
        for c in pieces {
            if let Some(p) = merged.iter_mut().rev().find(|p| p.end == c.start) { p.end=c.end; }
            else { merged.push(c); }
        }
        out.extend(merged);
    }
    out.sort(); out
}

/// Read canonical single-sample fragments, using each fragment's actual tfdt.
pub fn cues_from_fragments(data: &[u8], timescale: u32) -> Result<Vec<Cue>> {
    if timescale == 0 { return Err(invalid("zero text timescale")); }
    let mut cues=Vec::new();
    let mut pos=0;
    while pos < data.len() {
        let (kind, _, next)=crate::reader::read_box_header(data,pos)?;
        if &kind == b"moof" {
            let mut cur=Cursor::new(&data[pos..next]);
            let h=Header::read_from(&mut cur).map_err(|e| Error::InvalidMp4(e.to_string()))?;
            let moof=Moof::read_atom(&h,&mut cur).map_err(|e| Error::InvalidMp4(e.to_string()))?;
            let traf=moof.traf.first().ok_or_else(|| invalid("missing text traf"))?;
            let mut dt=traf.tfdt.as_ref().map_or(0, |t| t.base_media_decode_time);
            for trun in &traf.trun {
                let offset=trun.data_offset.ok_or_else(|| invalid("text trun missing data offset"))?;
                let mut payload=usize::try_from(pos as i64 + offset as i64).map_err(|_| invalid("invalid text offset"))?;
                for entry in &trun.entries {
                    let duration=entry.duration.or(traf.tfhd.default_sample_duration).ok_or_else(|| invalid("missing text duration"))?;
                    let size=entry.size.or(traf.tfhd.default_sample_size).ok_or_else(|| invalid("missing text size"))? as usize;
                    let end=dt.checked_add(duration as u64).ok_or_else(|| invalid("text timestamp overflow"))?;
                    let bytes=data.get(payload..payload.checked_add(size).ok_or_else(|| invalid("text size overflow"))?).ok_or_else(|| invalid("text sample out of bounds"))?;
                    cues.extend(cues_from_sample(bytes, dt*1000/timescale as u64,end*1000/timescale as u64)?);
                    dt=end;payload+=size;
                }
            }
        }
        pos=next;
    }
    Ok(merge_cues(cues))
}

pub fn build_track(config: &TextConfig, cues: &[Cue], start: u64, end: u64) -> Result<Vec<u8>> {
    let mut config=config.clone();
    config.container=crate::catalog::Container::cmaf(1000,config.track_id());
    let mut catalog=Catalog::default();
    catalog.insert_text(format!("text{}",config.track_id()),config.clone());
    let samples=crate::segment::text_span_samples(cues,start,end)?;
    let mut progress=TrackProgress::starting_at(start);
    let (out, _)=crate::segment::mint_text_segment(&catalog,config.track_id(),&mut progress,&samples)?;
    Ok(out)
}

/// Span requested by the streaming signer, on the absolute media timeline.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextRequest {
    pub start_ms: u64,
    pub end_ms: u64,
    pub max_track_id: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TextAttachment {
    #[serde(default)]
    pub tracks: Vec<TextTrackAttachment>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextTrackAttachment {
    pub track_id: u32,
    pub language: String,
    pub label: Option<String>,
    #[serde(default)]
    pub cues: Vec<Cue>,
}

/// Session state: declared tracks remain continuous even if a host call fails.
#[derive(Default)]
pub struct StreamingText {
    tracks: BTreeMap<u32, TextConfig>,
}

impl StreamingText {
    pub fn request(catalog: &Catalog, gop: &crate::GopSegment) -> Result<TextRequest> {
        let reference = catalog.video_configs().min_by_key(|c| c.track_id())
            .map(|c| (c.track_id(), c.timescale()))
            .or_else(|| catalog.audio_configs().min_by_key(|c| c.track_id())
                .map(|c| (c.track_id(), c.timescale())))
            .ok_or_else(|| invalid("streaming text requires an AV reference track"))?;
        let start = gop.first_decode_times[&reference.0];
        Ok(TextRequest {
            start_ms: crate::segment::ticks_to_ms(start, reference.1),
            end_ms: crate::segment::ticks_to_ms(start + gop.durations[&reference.0], reference.1),
            max_track_id: catalog.video_configs().map(|c| c.track_id())
                .chain(catalog.audio_configs().map(|c| c.track_id()))
                .chain(catalog.text_configs().map(|c| c.track_id())).max().unwrap_or(0),
        })
    }

    pub fn attach(&mut self, gop: &mut crate::GopSegment, req: TextRequest, attachment: TextAttachment) -> Result<()> {
        let mut cues = BTreeMap::new();
        for track in attachment.tracks {
            if track.track_id == 0 || gop.tracks.contains_key(&track.track_id) {
                return Err(invalid("streaming text track id must be nonzero and unused"));
            }
            let config = TextConfig {
                codec: "wvtt".into(), container: crate::catalog::Container::cmaf(1000, track.track_id),
                language: if track.language.is_empty() { "und".into() } else { track.language },
                label: track.label.filter(|s| !s.is_empty()), config: "WEBVTT".into(),
            };
            if let Some(previous) = self.tracks.get(&track.track_id) {
                if previous != &config { return Err(invalid("streaming text track configuration changed")); }
            } else {
                self.tracks.insert(track.track_id, config);
            }
            if cues.insert(track.track_id, track.cues).is_some() {
                return Err(invalid("duplicate streaming text track"));
            }
        }
        for (&id, config) in &self.tracks {
            let mut catalog = Catalog::default();
            catalog.insert_text(format!("text{id}"), config.clone());
            let samples = crate::segment::text_span_samples(cues.get(&id).map(Vec::as_slice).unwrap_or(&[]), req.start_ms, req.end_ms)?;
            let mut progress = TrackProgress::starting_at(req.start_ms);
            let (data, meta) = crate::segment::mint_text_segment(&catalog, id, &mut progress, &samples)?;
            gop.body_size += data.len() as u64;
            gop.durations.insert(id, req.end_ms - req.start_ms);
            gop.sample_counts.insert(id, samples.len() as u32);
            gop.first_decode_times.insert(id, req.start_ms);
            gop.samples.insert(id, meta);
            gop.tracks.insert(id, data);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cue(start:u64,end:u64,text:&str)->Cue { Cue{start,end,text:text.into(),id:None,settings:None} }
    #[test]
    fn deterministic_overlaps_gaps_and_clipping() {
        let cues=vec![cue(100,700,"first"),cue(400,1200,"second")];
        let config=TextConfig{codec:"wvtt".into(),container:crate::catalog::Container::cmaf(1000,3),language:"en".into(),label:Some("English".into()),config:"WEBVTT".into()};
        let a=build_track(&config,&cues,0,1000).unwrap();
        let b=build_track(&config,&[cues[1].clone(),cues[0].clone()],0,1000).unwrap();
        assert_eq!(a,b);assert_eq!(crate::cid::from_bytes(&a),crate::cid::from_bytes(&b));
        let samples=canonical_samples(&cues,0,1000).unwrap();
        assert_eq!(samples.iter().map(|s|(s.start,s.duration)).collect::<Vec<_>>(),vec![(0,100),(100,300),(400,300),(700,300)]);
        assert_eq!(&samples[0].data,b"\0\0\0\x08vtte");
        assert_eq!(cues_from_sample(&samples[2].data,400,700).unwrap(),vec![cue(400,700,"first"),cue(400,700,"second")]);
        assert_eq!(cues_from_fragments(&a,1000).unwrap(),vec![cue(100,700,"first"),cue(400,1000,"second")]);
        let next=build_track(&config,&cues,1000,1500).unwrap();
        assert_eq!(cues_from_fragments(&next,1000).unwrap(),vec![cue(1000,1200,"second")]);
        assert_eq!(canonical_samples(&[],0,1000).unwrap()[0].data,b"\0\0\0\x08vtte");
    }

    #[test]
    fn streaming_clips_crossing_cues_and_preserves_av() {
        let input=std::fs::read("samples/fixtures/h264-opus-frag.mp4").unwrap();
        let mut original=Vec::new();
        let mut catalog=crate::segment_fmp4(&mut Cursor::new(&input),|g|{original.push(g);Ok(())}).unwrap();
        let video=catalog.video_configs().next().unwrap();
        let (tid,ts)=(video.track_id(),video.timescale() as u64);
        assert!(original.len()>1);
        let start=original[0].first_decode_times[&tid]*1000/ts;
        let boundary=original[1].first_decode_times[&tid]*1000/ts;
        let last=original.last().unwrap();
        let end=(last.first_decode_times[&tid]+last.durations[&tid])*1000/ts;
        let wanted=vec![cue(boundary-100,boundary+100,"crossing"),cue(boundary+200,boundary+400,"later")];
        let config=TextConfig{codec:"wvtt".into(),container:crate::catalog::Container::cmaf(1000,3),language:"en".into(),label:None,config:"WEBVTT".into()};
        catalog.insert_text("text3",config.clone());
        let mut stream=crate::init::build_init_segment(&catalog).unwrap();
        stream.extend(build_track(&config,&wanted,start,end).unwrap());
        for gop in &original {for bytes in gop.tracks.values(){stream.extend_from_slice(bytes);}}
        let mut output=Vec::new();
        crate::segment_fmp4(&mut Cursor::new(&stream),|g|{output.push(g);Ok(())}).unwrap();
        assert_eq!(output.len(),original.len());
        for (got,old) in output.iter().zip(&original) {
            for (tid,bytes) in &old.tracks {assert_eq!(&got.tracks[tid],bytes);}
            let a=got.first_decode_times[&tid]*1000/ts;
            let b=(got.first_decode_times[&tid]+got.durations[&tid])*1000/ts;
            assert_eq!(got.first_decode_times[&3],a);assert_eq!(got.durations[&3],b-a);
        }
        let recovered=output.iter().flat_map(|g|cues_from_fragments(&g.tracks[&3],1000).unwrap()).collect();
        assert_eq!(merge_cues(recovered),wanted);
        let mut push=crate::Segmenter::new();
        let mut events=Vec::new();
        for chunk in stream.chunks(97) {events.extend(push.feed(chunk).unwrap());}
        events.extend(push.flush().unwrap());
        let pushed:Vec<_>=events.into_iter().filter_map(|e|match e{crate::SegmenterEvent::Segment(g)=>Some(g),_=>None}).collect();
        assert_eq!(pushed,output);
        let source=crate::read(&stream).unwrap();
        let mut normalized=Vec::new();
        crate::fmp4::write(&source,&stream,&mut normalized).unwrap();
        let mut roundtrip=Vec::new();
        crate::segment_fmp4(&mut Cursor::new(&normalized),|g|{roundtrip.push(g);Ok(())}).unwrap();
        assert_eq!(roundtrip,output);
    }
    #[test]
    fn attach_in_stream_is_continuous_and_preserves_av() {
        let input = std::fs::read("samples/fixtures/h264-opus-frag.mp4").unwrap();
        let mut gops = Vec::new();
        let catalog = crate::segment_fmp4(&mut Cursor::new(input), |g| { gops.push(g); Ok(()) }).unwrap();
        let mut state = StreamingText::default();
        let first = StreamingText::request(&catalog, &gops[0]).unwrap();
        let wanted = cue(first.end_ms - 100, first.end_ms + 100, "boundary");
        let id = first.max_track_id + 1;
        let mut recovered = Vec::new();
        for (i, gop) in gops.iter_mut().enumerate() {
            let req = StreamingText::request(&catalog, gop).unwrap();
            let av = gop.tracks.clone();
            let duration = gop.duration_us;
            let attachment = if i < 2 { TextAttachment { tracks: vec![TextTrackAttachment {
                track_id: id, language: "en-US".into(), label: Some("ingest".into()), cues: vec![wanted.clone()],
            }] } } else { TextAttachment::default() };
            state.attach(gop, req, attachment).unwrap();
            for (tid, data) in av { assert_eq!(gop.tracks[&tid], data); }
            assert_eq!(gop.duration_us, duration);
            assert_eq!(gop.first_decode_times[&id], req.start_ms);
            assert_eq!(gop.durations[&id], req.end_ms - req.start_ms);
            assert_eq!(gop.samples[&id].durations.iter().map(|&d| d as u64).sum::<u64>(), req.end_ms - req.start_ms);
            recovered.extend(cues_from_fragments(&gop.tracks[&id], 1000).unwrap());
            if i > 1 { assert!(cues_from_fragments(&gop.tracks[&id], 1000).unwrap().is_empty()); }
        }
        assert_eq!(merge_cues(recovered), vec![wanted]);
    }

    #[test]
    fn empty_attachment_is_byte_identical_before_declaration() {
        let input = std::fs::read("samples/fixtures/h264-opus-frag.mp4").unwrap();
        let mut gops = Vec::new();
        let catalog = crate::segment_fmp4(&mut Cursor::new(input), |g| { gops.push(g); Ok(()) }).unwrap();
        let mut state = StreamingText::default();
        for gop in &mut gops {
            let req = StreamingText::request(&catalog, gop).unwrap();
            let before = gop.tracks.clone();
            let body = gop.body_size;
            state.attach(gop, req, TextAttachment::default()).unwrap();
            assert_eq!(gop.tracks, before);
            assert_eq!(gop.body_size, body);
        }
    }
}
