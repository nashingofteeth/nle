//! OpenTimelineIO (`.otio`) import — pure, no processes.
//!
//! Kdenlive (25.04+) exports its timeline as OTIO: a `Timeline` whose
//! `tracks` `Stack` holds `Track`s of `Clip`s and `Gap`s. Each `Clip`
//! carries an `ExternalReference` (`target_url`) and a `source_range`.
//!
//! **Rate conforms.** Kdenlive does *not* emit a `LinearTimeWarp` when it
//! plays a clip at a different rate than its media — it writes the clip's
//! `source_range` in *project* frames and the media reference's
//! `available_range` in the *media* rate. So `speed = source_rate /
//! media_rate`, and the source range maps into media seconds by that
//! factor. Clips whose rates agree are 1:1 (`speed = 1`).
//!
//! Parsing walks `serde_json::Value` rather than deriving structs because
//! OTIO's `OTIO_SCHEMA` tags are versioned (`Clip.1`, `Clip.2`, …);
//! dispatching on the schema *name* (the part before the `.`) stays
//! readable across versions instead of breaking on a version bump.

use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

use crate::time::{Seconds, format_seconds};
use crate::timeline::{Clip, Gap, Segment, Timeline, Track, TrackKind};

#[derive(Debug, Error, PartialEq)]
pub enum OtioError {
    #[error("not valid JSON: {0}")]
    Json(String),
    #[error("not an OpenTimelineIO Timeline (found {0:?})")]
    NotTimeline(String),
    #[error("timeline has no tracks")]
    NoTracks,
    #[error("timeline has no video track")]
    NoVideo,
    #[error("track {track}: malformed {what}")]
    Malformed { track: usize, what: String },
    #[error("clip {name:?}: missing a media reference path")]
    MissingUrl { name: String },
    #[error("clip {name:?}: missing a source range")]
    MissingRange { name: String },
}

/// One track of the imported document.
#[derive(Debug, Clone, PartialEq)]
pub struct OtioTrack {
    pub kind: TrackKind,
    /// `enabled: false` in OTIO becomes a muted nle track.
    pub muted: bool,
    pub items: Vec<OtioItem>,
}

/// A track item: a clip or a gap.
#[derive(Debug, Clone, PartialEq)]
pub enum OtioItem {
    Clip(OtioClip),
    Gap(Seconds),
}

/// A clip's source range in media seconds, plus the rate factor Kdenlive
/// baked into the conform (`1` when the rates agree).
#[derive(Debug, Clone, PartialEq)]
pub struct OtioClip {
    pub name: String,
    pub path: PathBuf,
    pub asset_in: Seconds,
    pub asset_out: Seconds,
    pub speed: Seconds,
}

/// A parsed `.otio`, before media resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct OtioDoc {
    pub title: Option<String>,
    pub fps: Seconds,
    pub tracks: Vec<OtioTrack>,
    /// Non-fatal notes: skipped transitions/nesting/unknown items.
    pub warnings: Vec<String>,
}

/// The schema *name* of an OTIO object (`"Clip.2"` → `"Clip"`).
fn schema(v: &Value) -> String {
    v.get("OTIO_SCHEMA")
        .and_then(Value::as_str)
        .map(|s| s.split('.').next().unwrap_or(s).to_string())
        .unwrap_or_default()
}

/// A JSON float → exact `Seconds`, via continued fractions.
fn secs_of(v: f64) -> Option<Seconds> {
    v.is_finite()
        .then(|| Seconds::approximate_float(v))
        .flatten()
}

/// A `RationalTime` object → seconds (`value / rate`).
fn time_seconds(v: &Value) -> Option<Seconds> {
    let value = v.get("value")?.as_f64()?;
    let rate = v.get("rate")?.as_f64()?;
    if rate == 0.0 {
        return None;
    }
    Some(secs_of(value)? / secs_of(rate)?)
}

/// A `RationalTime`'s `rate` field → `Seconds`.
fn time_rate(v: &Value) -> Option<Seconds> {
    secs_of(v.get("rate")?.as_f64()?)
}

/// A `TimeRange`'s rate (from its `duration`, else `start_time`).
fn range_rate(r: &Value) -> Option<Seconds> {
    r.get("duration")
        .and_then(time_rate)
        .or_else(|| r.get("start_time").and_then(time_rate))
}

/// Parse OTIO text into an [`OtioDoc`].
pub fn parse(input: &str) -> Result<OtioDoc, OtioError> {
    let root: Value = serde_json::from_str(input).map_err(|e| OtioError::Json(e.to_string()))?;
    let sch = schema(&root);
    if sch != "Timeline" {
        return Err(OtioError::NotTimeline(sch));
    }
    let title = root
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let children = root
        .get("tracks")
        .and_then(|t| t.get("children"))
        .and_then(Value::as_array)
        .ok_or(OtioError::NoTracks)?;

    let fps = infer_fps(children).unwrap_or_else(|| Seconds::from_integer(24));

    let mut warnings = Vec::new();
    let mut tracks = Vec::new();
    for (ti, t) in children.iter().enumerate() {
        let kind = match t.get("kind").and_then(Value::as_str) {
            Some("Video") => TrackKind::Video,
            Some("Audio") => TrackKind::Audio,
            Some(other) => {
                warnings.push(format!("track {ti}: unsupported kind {other:?} — skipped"));
                continue;
            }
            None => {
                warnings.push(format!("track {ti}: no kind — skipped"));
                continue;
            }
        };
        let muted = !t.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let items = track_items(t, ti, &mut warnings)?;
        if items.is_empty() {
            // Kdenlive exports its unused tracks too. An empty track carries no
            // edit, so skip it rather than litter the timeline with empty rows
            // (and a later re-import re-adding them). Not a silent *content*
            // drop: there is nothing in it.
            continue;
        }
        tracks.push(OtioTrack { kind, muted, items });
    }
    if tracks.iter().all(|t| t.kind != TrackKind::Video) {
        return Err(OtioError::NoVideo);
    }
    Ok(OtioDoc {
        title,
        fps,
        tracks,
        warnings,
    })
}

/// The project frame rate: the first track/clip `source_range` rate.
fn infer_fps(tracks: &[Value]) -> Option<Seconds> {
    for t in tracks {
        if let Some(r) = t
            .get("source_range")
            .filter(|v| !v.is_null())
            .and_then(range_rate)
        {
            return Some(r);
        }
        let empty = Vec::new();
        let children = t
            .get("children")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        for ch in children {
            if schema(ch) == "Clip"
                && let Some(r) = ch.get("source_range").and_then(range_rate)
            {
                return Some(r);
            }
        }
    }
    None
}

fn track_items(
    t: &Value,
    ti: usize,
    warnings: &mut Vec<String>,
) -> Result<Vec<OtioItem>, OtioError> {
    let mut items = Vec::new();
    let empty = Vec::new();
    let children = t
        .get("children")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for ch in children {
        match schema(ch).as_str() {
            "Gap" => {
                let d = ch
                    .get("source_range")
                    .and_then(|sr| sr.get("duration"))
                    .and_then(time_seconds)
                    .ok_or_else(|| OtioError::Malformed {
                        track: ti,
                        what: "gap without a source range".into(),
                    })?;
                items.push(OtioItem::Gap(d));
            }
            "Clip" => {
                let clip = parse_clip(ch)?;
                // A clip can carry time effects (a `LinearTimeWarp` speed
                // change, a freeze). They are not imported — everything
                // else unsupported warns here, so effects must too rather
                // than silently playing at the wrong rate.
                if ch
                    .get("effects")
                    .and_then(Value::as_array)
                    .is_some_and(|e| !e.is_empty())
                {
                    warnings.push(format!(
                        "track {ti}: clip {:?} effects ignored (speed warps/time effects are not imported)",
                        clip.name
                    ));
                }
                items.push(OtioItem::Clip(clip));
            }
            "Transition" => warnings.push(format!("track {ti}: transition skipped (cuts only)")),
            "Stack" | "Track" => warnings.push(format!(
                "track {ti}: nested {schema} skipped",
                schema = schema(ch)
            )),
            other => warnings.push(format!("track {ti}: {other} skipped")),
        }
    }
    Ok(items)
}

fn parse_clip(ch: &Value) -> Result<OtioClip, OtioError> {
    let name = ch
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let sr = ch
        .get("source_range")
        .filter(|v| !v.is_null())
        .ok_or_else(|| OtioError::MissingRange { name: name.clone() })?;
    let start = sr.get("start_time").and_then(time_seconds);
    let dur = sr.get("duration").and_then(time_seconds);
    let (start, dur) = match (start, dur) {
        (Some(s), Some(d)) => (s, d),
        _ => return Err(OtioError::MissingRange { name }),
    };

    let src_rate = range_rate(sr).unwrap_or_else(|| Seconds::from_integer(1));
    let (url, media_rate) = media_reference(ch);
    let url = url.ok_or_else(|| OtioError::MissingUrl { name: name.clone() })?;
    // No media reference rate: treat the source as already at the project rate.
    let media_rate = media_rate.unwrap_or(src_rate);
    let speed = src_rate / media_rate;

    Ok(OtioClip {
        path: url_to_path(&url),
        name,
        // The range is in project frames; scale into media seconds.
        asset_in: start * speed,
        asset_out: (start + dur) * speed,
        speed,
    })
}

/// Pull `(target_url, media_rate)` out of a clip's media reference. Handles
/// both the `Clip.2` `media_references` map and the `Clip.1` singular form.
fn media_reference(ch: &Value) -> (Option<String>, Option<Seconds>) {
    let mr = ch
        .get("media_references")
        .and_then(Value::as_object)
        .and_then(|m| m.get("DEFAULT_MEDIA").or_else(|| m.values().next()))
        .or_else(|| ch.get("media_reference").filter(|v| !v.is_null()));
    let Some(mr) = mr else {
        return (None, None);
    };
    if schema(mr) != "ExternalReference" {
        return (None, None);
    }
    let url = mr
        .get("target_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let rate = mr
        .get("available_range")
        .filter(|v| !v.is_null())
        .and_then(range_rate);
    (url, rate)
}

/// `file://` URL or plain path → filesystem path.
fn url_to_path(url: &str) -> PathBuf {
    match url.strip_prefix("file://") {
        Some(rest) => {
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            PathBuf::from(percent_decode(rest))
        }
        None => PathBuf::from(url),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum BuildError {
    #[error("clip {name:?}: no pooled asset matches its path")]
    Unresolved { name: String },
}

/// Turn a parsed document into a [`Timeline`]. `resolve` maps a clip's
/// source path to a pooled asset id (the caller owns the pool).
pub fn build(
    doc: &OtioDoc,
    resolve: &dyn Fn(&Path) -> Option<u32>,
) -> Result<Timeline, BuildError> {
    let mut next_id = 1u32;
    let mut tracks = Vec::new();
    for t in &doc.tracks {
        let mut segments = Vec::new();
        for item in &t.items {
            match item {
                OtioItem::Gap(d) => segments.push(Segment::Gap(Gap { duration: *d })),
                OtioItem::Clip(c) => {
                    let asset = resolve(&c.path).ok_or_else(|| BuildError::Unresolved {
                        name: c.name.clone(),
                    })?;
                    segments.push(Segment::Clip(Clip {
                        id: next_id,
                        asset,
                        asset_in: c.asset_in,
                        asset_out: c.asset_out,
                        speed: c.speed,
                    }));
                    next_id += 1;
                }
            }
        }
        tracks.push(Track {
            kind: t.kind,
            muted: t.muted,
            segments,
        });
    }
    Ok(Timeline { tracks })
}

/// Clamp each clip's out-point to its media duration. A Kdenlive rate conform
/// can round an out-point a hair past the file's end (e.g. a 0.4× clip on a
/// 59.94 render); the export would then read past EOF and fall short by that
/// sliver, drifting everything after it. `media` maps an asset id to its
/// duration. Returns one note per adjusted clip (and drops a clip whose whole
/// range is past the end).
pub fn clamp_to_media(
    timeline: &mut Timeline,
    media: &dyn Fn(u32) -> Option<Seconds>,
) -> Vec<String> {
    let mut notes = Vec::new();
    for track in &mut timeline.tracks {
        track.segments.retain_mut(|seg| {
            let Segment::Clip(c) = seg else {
                return true;
            };
            let Some(d) = media(c.asset) else {
                return true;
            };
            if c.asset_out <= d {
                return true;
            }
            if d > c.asset_in {
                notes.push(format!(
                    "clip #{}: out {} → media end {}",
                    c.id,
                    format_seconds(&c.asset_out),
                    format_seconds(&d)
                ));
                c.asset_out = d;
                true
            } else {
                notes.push(format!("clip #{}: past the media end — dropped", c.id));
                false
            }
        });
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-video-track timeline: clip, gap, clip. Rates agree (24 == 24),
    /// so `speed = 1` and the source range is literal seconds.
    const CUTS: &str = r#"{
      "OTIO_SCHEMA": "Timeline.1",
      "name": "cuts",
      "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
        { "OTIO_SCHEMA": "Track.1", "kind": "Video", "children": [
          { "OTIO_SCHEMA": "Clip.1", "name": "a.mp4",
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 48.0 },
              "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 48.0 } },
            "media_reference": { "OTIO_SCHEMA": "ExternalReference.1",
              "target_url": "/media/a.mp4",
              "available_range": {
                "OTIO_SCHEMA": "TimeRange.1",
                "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 240.0 } } } },
          { "OTIO_SCHEMA": "Gap.1",
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
              "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0 } } },
          { "OTIO_SCHEMA": "Clip.1", "name": "b.mp4",
            "source_range": {
              "OTIO_SCHEMA": "TimeRange.1",
              "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0 },
              "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 72.0 } },
            "media_reference": { "OTIO_SCHEMA": "ExternalReference.1",
              "target_url": "/media/b.mp4",
              "available_range": {
                "OTIO_SCHEMA": "TimeRange.1",
                "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 240.0 } } } }
        ] }
      ] }
    }"#;

    #[test]
    fn parses_cuts_and_gaps() {
        let doc = parse(CUTS).unwrap();
        assert_eq!(doc.title.as_deref(), Some("cuts"));
        assert_eq!(doc.fps, Seconds::from_integer(24));
        assert_eq!(doc.tracks.len(), 1);
        let items = &doc.tracks[0].items;
        assert_eq!(items.len(), 3);
        match &items[0] {
            OtioItem::Clip(c) => {
                assert_eq!(c.name, "a.mp4");
                assert_eq!(c.path, Path::new("/media/a.mp4"));
                assert_eq!(c.asset_in, Seconds::from_integer(2)); // 48/24
                assert_eq!(c.asset_out, Seconds::from_integer(4)); // 96/24
                assert_eq!(c.speed, Seconds::from_integer(1));
            }
            _ => panic!("expected clip"),
        }
        assert_eq!(items[1], OtioItem::Gap(Seconds::from_integer(1)));
        assert!(doc.warnings.is_empty());
    }

    #[test]
    fn builds_timeline_in_order() {
        let doc = parse(CUTS).unwrap();
        let timeline = build(&doc, &|p| match p.to_str() {
            Some("/media/a.mp4") => Some(7),
            Some("/media/b.mp4") => Some(9),
            _ => None,
        })
        .unwrap();
        assert_eq!(timeline.tracks.len(), 1);
        let segs = &timeline.tracks[0].segments;
        assert_eq!(segs.len(), 3);
        match (&segs[0], &segs[2]) {
            (Segment::Clip(a), Segment::Clip(b)) => {
                assert_eq!((a.asset, b.asset), (7, 9));
                // ids are assigned in document order
                assert_eq!((a.id, b.id), (1, 2));
            }
            _ => panic!("expected clips at 0 and 2"),
        }
        assert_eq!(timeline.end(), Seconds::from_integer(6)); // 2 + 1 + 3
    }

    #[test]
    fn rate_conform_is_derived() {
        // Project 24, media 30000/1001: Kdenlive writes the range in project
        // frames. speed = 24 / (30000/1001) = 1001/1250; source seconds =
        // frames / media_rate.
        let doc = parse(
            r#"{ "OTIO_SCHEMA": "Timeline.1", "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
              { "OTIO_SCHEMA": "Track.1", "kind": "Video",
                "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                  "duration": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 37538.0 } },
                "children": [
                { "OTIO_SCHEMA": "Clip.2", "name": "C0004.MP4",
                  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 126.0 },
                    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 1024.0 } },
                  "media_references": { "DEFAULT_MEDIA": { "OTIO_SCHEMA": "ExternalReference.1",
                    "target_url": "/media/C0004.MP4",
                    "available_range": { "OTIO_SCHEMA": "TimeRange.1",
                      "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 29.97002997002997, "value": 0.0 },
                      "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 29.97002997002997, "value": 1021.0 } } } } }
              ] }
            ] } }"#,
        )
        .unwrap();
        let OtioItem::Clip(c) = &doc.tracks[0].items[0] else {
            panic!("expected clip");
        };
        assert_eq!(c.speed, Seconds::new(1001, 1250)); // 24 / (30000/1001)
        // asset_in = 126 frames / 29.97 ≈ 4.2042 s
        assert_eq!(c.asset_in, Seconds::new(126 * 1001, 30000));
        assert_eq!(c.asset_out, Seconds::new(1150 * 1001, 30000));
        // timeline duration = 1024 / 24, i.e. the project frame span
        assert_eq!(c.asset_out - c.asset_in, Seconds::new(1024 * 1001, 30000));
        // ... and (asset_out - asset_in) / speed == 1024/24
        assert_eq!((c.asset_out - c.asset_in) / c.speed, Seconds::new(1024, 24));
    }

    #[test]
    fn versioned_schema_names_are_accepted() {
        // Clip.2 with the media_references map must parse like Clip.1.
        let doc = parse(&CUTS.replace("\"Clip.1\"", "\"Clip.2\"")).unwrap();
        assert_eq!(doc.tracks[0].items.len(), 3);
    }

    #[test]
    fn tracks_are_preserved_with_kind_and_mute() {
        let doc = parse(
            r#"{ "OTIO_SCHEMA": "Timeline.1", "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
              { "OTIO_SCHEMA": "Track.1", "kind": "Video", "enabled": false, "children": [
                { "OTIO_SCHEMA": "Clip.1", "name": "a.mp4",
                  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0 } },
                  "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "/m/a.mp4" } }
              ] },
              { "OTIO_SCHEMA": "Track.1", "kind": "Audio", "children": [
                { "OTIO_SCHEMA": "Clip.1", "name": "a.mp4",
                  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0 } },
                  "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "/m/a.mp4" } }
              ] }
            ] } }"#,
        )
        .unwrap();
        assert_eq!(doc.tracks.len(), 2);
        assert_eq!(doc.tracks[0].kind, TrackKind::Video);
        assert!(doc.tracks[0].muted);
        assert_eq!(doc.tracks[1].kind, TrackKind::Audio);
        assert!(!doc.tracks[1].muted);
    }

    #[test]
    fn empty_tracks_are_skipped() {
        // Kdenlive exports its unused tracks; they must not come back as rows.
        let doc = parse(
            r#"{ "OTIO_SCHEMA": "Timeline.1", "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
              { "OTIO_SCHEMA": "Track.1", "kind": "Video", "children": [
                { "OTIO_SCHEMA": "Clip.1", "name": "a.mp4",
                  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0 } },
                  "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "/m/a.mp4" } }
              ] },
              { "OTIO_SCHEMA": "Track.1", "kind": "Video", "children": [] },
              { "OTIO_SCHEMA": "Track.1", "kind": "Audio", "children": [] }
            ] } }"#,
        )
        .unwrap();
        assert_eq!(doc.tracks.len(), 1, "empty tracks dropped");
        assert_eq!(doc.tracks[0].kind, TrackKind::Video);
    }

    #[test]
    fn clamp_to_media_trims_or_drops_past_the_end() {
        let doc = parse(CUTS).unwrap();
        // Both clips resolve to asset 1; with a 3s media, each [..4) out-point
        // overruns and is clamped.
        let mut t = build(&doc, &|_| Some(1)).unwrap();
        let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(3)));
        assert_eq!(notes.len(), 2, "{notes:?}");
        let Segment::Clip(c) = &t.tracks[0].segments[0] else {
            panic!("expected clip")
        };
        assert_eq!(c.asset_out, Seconds::from_integer(3));

        // A clip wholly past the media end (in-point at/after it) is dropped.
        let mut t = build(&doc, &|_| Some(1)).unwrap();
        let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(1)));
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            t.tracks[0]
                .segments
                .iter()
                .all(|s| matches!(s, Segment::Gap(_))),
            "both clips dropped; the gap survives"
        );
    }

    #[test]
    fn transitions_and_nesting_are_warned_not_silent() {
        let doc = parse(
            r#"{ "OTIO_SCHEMA": "Timeline.1", "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
              { "OTIO_SCHEMA": "Track.1", "kind": "Video", "children": [
                { "OTIO_SCHEMA": "Transition.1", "name": "t" },
                { "OTIO_SCHEMA": "Stack.1", "children": [] },
                { "OTIO_SCHEMA": "Clip.1", "name": "a.mp4",
                  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0 } },
                  "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": "/m/a.mp4" } }
              ] }
            ] } }"#,
        )
        .unwrap();
        assert_eq!(doc.warnings.len(), 2, "{:?}", doc.warnings);
        assert!(doc.warnings[0].contains("transition"));
        assert!(doc.warnings[1].contains("nested"));
    }

    #[test]
    fn clip_effects_warn_not_silent() {
        // A Kdenlive speed change exports as a LinearTimeWarp effect; it is
        // not imported, so the import must say so rather than mis-rate the
        // clip quietly.
        let doc = parse(
            r#"{ "OTIO_SCHEMA": "Timeline.1", "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
              { "OTIO_SCHEMA": "Track.1", "kind": "Video", "children": [
                { "OTIO_SCHEMA": "Clip.1", "name": "sped.mp4",
                  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0 } },
                  "effects": [ { "OTIO_SCHEMA": "LinearTimeWarp.1", "time_scalar": 2.0 } ],
                  "media_reference": { "OTIO_SCHEMA": "ExternalReference.1",
                    "target_url": "/m/sped.mp4" } }
              ] }
            ] } }"#,
        )
        .unwrap();
        assert_eq!(doc.warnings.len(), 1, "{:?}", doc.warnings);
        assert!(doc.warnings[0].contains("effects"), "{:?}", doc.warnings);
        // The clip still imports (at the derived rate), like other warnings.
        assert_eq!(doc.tracks[0].items.len(), 1);

        // An empty effects list is not a warning.
        let doc = parse(
            r#"{ "OTIO_SCHEMA": "Timeline.1", "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
              { "OTIO_SCHEMA": "Track.1", "kind": "Video", "children": [
                { "OTIO_SCHEMA": "Clip.1", "name": "plain.mp4",
                  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0 } },
                  "effects": [],
                  "media_reference": { "OTIO_SCHEMA": "ExternalReference.1",
                    "target_url": "/m/plain.mp4" } }
              ] }
            ] } }"#,
        )
        .unwrap();
        assert!(doc.warnings.is_empty(), "{:?}", doc.warnings);
    }

    #[test]
    fn file_url_is_decoded() {
        assert_eq!(
            url_to_path("file:///home/a/my%20clip.mov"),
            Path::new("/home/a/my clip.mov")
        );
        assert_eq!(url_to_path("file://localhost/x/y"), Path::new("/x/y"));
        assert_eq!(url_to_path("/plain/path.mp4"), Path::new("/plain/path.mp4"));
    }

    #[test]
    fn build_carries_speed() {
        let doc = parse(
            r#"{ "OTIO_SCHEMA": "Timeline.1", "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
              { "OTIO_SCHEMA": "Track.1", "kind": "Video", "children": [
                { "OTIO_SCHEMA": "Clip.2", "name": "slow.mp4",
                  "source_range": { "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0 },
                    "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 240.0 } },
                  "media_references": { "DEFAULT_MEDIA": { "OTIO_SCHEMA": "ExternalReference.1",
                    "target_url": "/m/slow.mp4",
                    "available_range": { "OTIO_SCHEMA": "TimeRange.1",
                      "start_time": { "OTIO_SCHEMA": "RationalTime.1", "rate": 29.97, "value": 0.0 },
                      "duration":   { "OTIO_SCHEMA": "RationalTime.1", "rate": 29.97, "value": 300.0 } } } } }
              ] }
            ] } }"#,
        )
        .unwrap();
        let timeline = build(&doc, &|_| Some(1)).unwrap();
        let Segment::Clip(c) = &timeline.tracks[0].segments[0] else {
            panic!("expected clip");
        };
        assert_eq!(c.speed, Seconds::from_integer(24) / Seconds::new(2997, 100));
        // Timeline duration is the project frame span, 240/24 = 10 s.
        assert_eq!(c.duration(), Seconds::from_integer(10));
    }

    #[test]
    fn rejects_non_timeline() {
        assert!(matches!(
            parse(r#"{ "OTIO_SCHEMA": "Clip.1" }"#),
            Err(OtioError::NotTimeline(_))
        ));
        assert!(matches!(parse("not json"), Err(OtioError::Json(_))));
    }
}
