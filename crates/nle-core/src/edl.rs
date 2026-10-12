//! CMX3600 EDL parsing — pure, no processes.
//!
//! An EDL is a text list of events (`NNN  REEL  TRK  TRANS  SRC_IN SRC_OUT
//! REC_IN REC_OUT`) plus `TITLE:` / `FCM:` headers and `*` comment lines
//! (which usually carry `FROM CLIP NAME:` / `SOURCE FILE:` — the best
//! asset-matching hints). Cuts only: dissolves and wipes are rejected.

use thiserror::Error;

use crate::time::{Seconds, Timecode, frames_to_seconds, parse_timecode, timecode_to_frames};
use crate::timeline::{Clip, Gap, Segment, Timeline, Track, TrackKind};

#[derive(Debug, Error, PartialEq)]
pub enum EdlError {
    #[error("no events found in the EDL")]
    NoEvents,
    #[error("event {event}: malformed line: {line:?}")]
    Malformed { event: usize, line: String },
    #[error("event {event}: unsupported transition {transition:?} (cuts only)")]
    Transition { event: usize, transition: String },
    #[error("event {event}: bad timecode {value:?}")]
    Timecode { event: usize, value: String },
}

/// One EDL event.
#[derive(Debug, Clone, PartialEq)]
pub struct EdlEvent {
    pub num: u32,
    /// Reel / tape name (e.g. `AX`, `BL`, or a filename in file-based EDLs).
    pub reel: String,
    /// Channel: `V`, `A`, `A2`, `B`, `AA`, …
    pub channel: String,
    /// Transition code; only `C` (cut) is supported.
    pub transition: char,
    pub src_in: Timecode,
    pub src_out: Timecode,
    pub rec_in: Timecode,
    pub rec_out: Timecode,
    /// `* FROM CLIP NAME:` when present.
    pub clip_name: Option<String>,
    /// `* SOURCE FILE:` when present.
    pub source_file: Option<String>,
}

/// A parsed EDL.
#[derive(Debug, Clone, PartialEq)]
pub struct EdlDoc {
    pub title: Option<String>,
    pub drop_frame: bool,
    pub events: Vec<EdlEvent>,
}

/// Parse CMX3600 text into an `EdlDoc`.
pub fn parse(input: &str) -> Result<EdlDoc, EdlError> {
    let mut title = None;
    let mut drop_frame = false;
    let mut events: Vec<EdlEvent> = Vec::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("TITLE:") {
            title = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("FCM:") {
            let up = rest.trim().to_uppercase();
            drop_frame = up.contains("DROP") && !up.contains("NON");
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('*') {
            // Comment lines attach to the most recent event.
            if let Some(ev) = events.last_mut() {
                let rest = rest.trim();
                if let Some(v) = rest.strip_prefix("FROM CLIP NAME:") {
                    ev.clip_name = Some(v.trim().to_string());
                } else if let Some(v) = rest.strip_prefix("SOURCE FILE:") {
                    ev.source_file = Some(v.trim().to_string());
                }
            }
            continue;
        }
        // An event starts with its event number.
        if !trimmed.as_bytes()[0].is_ascii_digit() {
            continue;
        }
        events.push(parse_event(trimmed, events.len() + 1)?);
    }

    if events.is_empty() {
        return Err(EdlError::NoEvents);
    }
    Ok(EdlDoc {
        title,
        drop_frame,
        events,
    })
}

/// Parse one event line. The last four whitespace-separated tokens are the
/// timecodes; token 4 is the transition, token 5 (when present, between the
/// transition and the timecodes) is the transition duration.
fn parse_event(line: &str, event: usize) -> Result<EdlEvent, EdlError> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let bad = || EdlError::Malformed {
        event,
        line: line.to_string(),
    };
    if tokens.len() < 8 {
        return Err(bad());
    }
    let num: u32 = tokens[0].parse().map_err(|_| bad())?;
    let reel = tokens[1].to_string();
    let channel = tokens[2].to_string();
    let transition = tokens[3].chars().next().ok_or_else(bad)?;
    if transition != 'C' {
        return Err(EdlError::Transition {
            event,
            transition: tokens[3].to_string(),
        });
    }
    let tc = |i: usize| -> Result<Timecode, EdlError> {
        let v = tokens[tokens.len() - 4 + i];
        parse_timecode(v).map_err(|_| EdlError::Timecode {
            event,
            value: v.to_string(),
        })
    };
    Ok(EdlEvent {
        num,
        reel,
        channel,
        transition,
        src_in: tc(0)?,
        src_out: tc(1)?,
        rec_in: tc(2)?,
        rec_out: tc(3)?,
        clip_name: None,
        source_file: None,
    })
}

/// Resolve a reel/clip name to an asset id (the caller owns the pool).
pub type ReelMap<'a> = dyn Fn(&EdlEvent) -> Option<u32> + 'a;

#[derive(Debug, Error, PartialEq)]
pub enum BuildError {
    #[error("no video events in the EDL")]
    NoVideo,
    #[error("event {event}: overlapping record time (cuts only)")]
    Overlap { event: u32 },
    #[error("event {event}: source out is not after source in")]
    BadSpan { event: u32 },
    #[error("event {event}: record out is not after record in")]
    RecSpan { event: u32 },
    #[error("event {event}: no pool asset matches {name:?}")]
    Unresolved { event: u32, name: String },
}

fn hint(ev: &EdlEvent) -> Option<String> {
    ev.source_file.clone().or_else(|| ev.clip_name.clone())
}

fn frames_of(tc: Timecode, doc: &EdlDoc, fps: Seconds) -> i64 {
    timecode_to_frames(tc, fps, doc.drop_frame)
}

/// Build a timeline from `doc`: video events become video track 0, then one
/// audio track per distinct audio channel. Record-position gaps become `Gap`
/// segments; source timecodes map to file time by subtracting
/// `src_offset_frames`. A clip plays at `source_span / record_span` — for a
/// plain 1× edit that is exactly `1`, and when an EDL came out of an edit
/// with a speed change the ratio recovers it (so the built timeline still
/// matches the record times instead of drifting after the event). Audio
/// events whose reel + source range match a video event are *linked* (the
/// video clip already carries that audio) and are skipped, so linked a/v
/// doesn't double.
pub fn build(
    doc: &EdlDoc,
    fps: Seconds,
    src_offset_frames: i64,
    resolve: &ReelMap,
) -> Result<Timeline, BuildError> {
    let mut video: Vec<&EdlEvent> = doc
        .events
        .iter()
        .filter(|e| e.channel.starts_with('V'))
        .collect();
    video.sort_by_key(|e| frames_of(e.rec_in, doc, fps));
    if video.is_empty() {
        return Err(BuildError::NoVideo);
    }

    let mut next_id = 1u32;
    // Standard EDLs start record time at 01:00:00:00 — the earliest event
    // is the timeline origin, not zero.
    let rec_origin = doc
        .events
        .iter()
        .map(|e| frames_of(e.rec_in, doc, fps))
        .min()
        .unwrap_or(0);
    let mut track_of =
        |events: &[&EdlEvent], overlap_ok: bool| -> Result<Vec<Segment>, BuildError> {
            let mut segs = Vec::new();
            let mut cursor = rec_origin;
            for ev in events {
                let rec_in = frames_of(ev.rec_in, doc, fps);
                let rec_out = frames_of(ev.rec_out, doc, fps);
                if rec_in < cursor {
                    if overlap_ok {
                        continue;
                    }
                    return Err(BuildError::Overlap { event: ev.num });
                }
                if rec_in > cursor {
                    segs.push(Segment::Gap(Gap {
                        duration: frames_to_seconds(rec_in - cursor, fps),
                    }));
                }
                let src_in = frames_of(ev.src_in, doc, fps) - src_offset_frames;
                let src_out = frames_of(ev.src_out, doc, fps) - src_offset_frames;
                if src_out <= src_in {
                    return Err(BuildError::BadSpan { event: ev.num });
                }
                // nle's timeline span for a clip is source / speed, and the
                // record span is the timeline span the original edit used —
                // so their ratio IS the speed. Equal spans reduce to 1.
                if rec_out <= rec_in {
                    return Err(BuildError::RecSpan { event: ev.num });
                }
                let speed = Seconds::new(src_out - src_in, rec_out - rec_in);
                let asset = resolve(ev).ok_or_else(|| BuildError::Unresolved {
                    event: ev.num,
                    name: hint(ev).unwrap_or_else(|| ev.reel.clone()),
                })?;
                segs.push(Segment::Clip(Clip {
                    id: next_id,
                    asset,
                    asset_in: frames_to_seconds(src_in, fps),
                    asset_out: frames_to_seconds(src_out, fps),
                    speed,
                    effects: Vec::new(),
                }));
                next_id += 1;
                cursor = rec_out;
            }
            Ok(segs)
        };

    let video_segs = track_of(&video, false)?;
    let mut tracks = vec![Track {
        kind: TrackKind::Video,
        muted: false,
        segments: video_segs,
    }];

    // Linked-audio keys: (reel, src_in_frames, src_out_frames).
    let linked: Vec<(String, i64, i64)> = video
        .iter()
        .map(|e| {
            (
                e.reel.clone(),
                frames_of(e.src_in, doc, fps),
                frames_of(e.src_out, doc, fps),
            )
        })
        .collect();

    let mut channels: Vec<String> = doc
        .events
        .iter()
        .filter(|e| e.channel.starts_with('A'))
        .map(|e| e.channel.clone())
        .collect();
    channels.sort();
    channels.dedup();

    for ch in channels {
        let evs: Vec<&EdlEvent> = doc
            .events
            .iter()
            .filter(|e| {
                e.channel == ch
                    && !linked.contains(&(
                        e.reel.clone(),
                        frames_of(e.src_in, doc, fps),
                        frames_of(e.src_out, doc, fps),
                    ))
            })
            .collect();
        let mut evs = evs;
        evs.sort_by_key(|e| frames_of(e.rec_in, doc, fps));
        if evs.is_empty() {
            continue;
        }
        let segs = track_of(&evs, true)?;
        if !segs.is_empty() {
            tracks.push(Track {
                kind: TrackKind::Audio,
                muted: false,
                segments: segs,
            });
        }
    }

    Ok(Timeline { tracks })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUT_EDL: &str = "\
TITLE: Demo
FCM: NON-DROP FRAME

001  AX       V     C        00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00
* FROM CLIP NAME: red.mp4
002  AX       V     C        00:00:01:00 00:00:03:00 01:00:02:00 01:00:04:00
* SOURCE FILE: /media/blue.mp4
";

    #[test]
    fn parses_cuts_and_comments() {
        let doc = parse(CUT_EDL).unwrap();
        assert_eq!(doc.title.as_deref(), Some("Demo"));
        assert!(!doc.drop_frame);
        assert_eq!(doc.events.len(), 2);
        let e = &doc.events[0];
        assert_eq!(e.num, 1);
        assert_eq!(e.reel, "AX");
        assert_eq!(e.channel, "V");
        assert_eq!(e.transition, 'C');
        assert_eq!(e.src_in, parse_timecode("00:00:00:00").unwrap());
        assert_eq!(e.rec_out, parse_timecode("01:00:02:00").unwrap());
        assert_eq!(e.clip_name.as_deref(), Some("red.mp4"));
        assert_eq!(
            doc.events[1].source_file.as_deref(),
            Some("/media/blue.mp4")
        );
    }

    #[test]
    fn detects_drop_frame() {
        let doc = parse(
            "FCM: DROP FRAME\n\n001 AX V C 00:00:00;00 00:00:01;00 01:00:00;00 01:00:01;00\n",
        )
        .unwrap();
        assert!(doc.drop_frame);
        assert_eq!(doc.events.len(), 1);
    }

    #[test]
    fn rejects_dissolves() {
        let edl = "001  AX  V  D  024  00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00\n";
        assert!(matches!(
            parse(edl),
            Err(EdlError::Transition { event: 1, .. })
        ));
    }

    #[test]
    fn rejects_empty_and_malformed() {
        assert_eq!(parse("TITLE: x\n"), Err(EdlError::NoEvents));
        assert!(matches!(
            parse("001 AX V C\n"),
            Err(EdlError::Malformed { event: 1, .. })
        ));
        assert!(matches!(
            parse("001 AX V C 00:00:00:00 00:00:02:00 01:00:00:00 nope\n"),
            Err(EdlError::Timecode { event: 1, .. })
        ));
    }

    fn some_asset(_: &EdlEvent) -> Option<u32> {
        Some(7)
    }

    #[test]
    fn builds_video_with_record_gaps() {
        // Event 2 starts one second late → a record gap between them.
        let edl = "\
001 AX V C 00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00
002 AX V C 00:00:01:00 00:00:03:00 01:00:03:00 01:00:05:00
";
        let doc = parse(edl).unwrap();
        let fps = Seconds::from_integer(30);
        let tl = build(&doc, fps, 0, &some_asset).unwrap();
        assert_eq!(tl.tracks.len(), 1);
        let segs = &tl.tracks[0].segments;
        assert!(matches!(segs[0], Segment::Clip(_)), "{segs:?}");
        assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1)));
        assert!(matches!(segs[2], Segment::Clip(_)), "{segs:?}");
    }

    #[test]
    fn speed_conform_matches_record_layout() {
        // 4s of source over an 8s record span = 0.5x: the built clip occupies
        // the full record span, so the timeline layout matches the record
        // times (the old fixed-1x build drifted every event after it by 4s).
        let edl = "\
001 AX V C 00:00:00:00 00:00:04:00 01:00:00:00 01:00:08:00
002 AX V C 00:00:04:00 00:00:06:00 01:00:08:00 01:00:10:00
";
        let doc = parse(edl).unwrap();
        let fps = Seconds::from_integer(30);
        let tl = build(&doc, fps, 0, &some_asset).unwrap();
        let segs = &tl.tracks[0].segments;
        match &segs[0] {
            Segment::Clip(c) => {
                assert_eq!(c.speed, Seconds::new(1, 2));
                assert_eq!(c.duration(), Seconds::from_integer(8));
            }
            _ => panic!("expected a clip"),
        }
        // No gap: event 2 starts exactly where the slowed event 1 ends.
        match (&segs[0], &segs[1]) {
            (Segment::Clip(a), Segment::Clip(b)) => assert_eq!(b.asset_in, a.asset_out),
            _ => panic!("expected two clips"),
        }
        assert_eq!(tl.end(), Seconds::from_integer(10));
    }

    #[test]
    fn a_one_frame_conform_reduces_to_one_x() {
        // 60 frames of source over 60 frames of record → 1/1, not 60/60.
        let edl = "001 AX V C 00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00\n";
        let doc = parse(edl).unwrap();
        let tl = build(&doc, Seconds::from_integer(30), 0, &some_asset).unwrap();
        match &tl.tracks[0].segments[0] {
            Segment::Clip(c) => assert_eq!(c.speed, Seconds::from_integer(1)),
            _ => panic!("expected a clip"),
        }
    }

    #[test]
    fn rejects_a_non_positive_record_span() {
        let edl = "001 AX V C 00:00:00:00 00:00:02:00 01:00:02:00 01:00:00:00\n";
        let doc = parse(edl).unwrap();
        let err = build(&doc, Seconds::from_integer(30), 0, &some_asset).unwrap_err();
        assert!(matches!(err, BuildError::RecSpan { event: 1 }), "{err:?}");
    }

    #[test]
    fn source_offset_maps_to_file_time() {
        let edl = "001 AX V C 01:00:00:00 01:00:02:00 01:00:00:00 01:00:02:00\n";
        let doc = parse(edl).unwrap();
        let fps = Seconds::from_integer(30);
        let off = timecode_to_frames(parse_timecode("01:00:00:00").unwrap(), fps, false);
        let tl = build(&doc, fps, off, &some_asset).unwrap();
        match &tl.tracks[0].segments[0] {
            Segment::Clip(c) => {
                assert_eq!(c.asset_in, Seconds::from_integer(0));
                assert_eq!(c.asset_out, Seconds::from_integer(2));
            }
            _ => panic!("expected a clip"),
        }
    }

    #[test]
    fn unresolved_event_errors() {
        let edl = "001 AX V C 00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00\n";
        let doc = parse(edl).unwrap();
        let none = |_: &EdlEvent| None;
        let err = build(&doc, Seconds::from_integer(30), 0, &none).unwrap_err();
        assert!(
            matches!(err, BuildError::Unresolved { event: 1, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn linked_audio_is_skipped_but_distinct_audio_gets_a_track() {
        let edl = "\
001 AX V C 00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00
002 AX A C 00:00:00:00 00:00:02:00 01:00:00:00 01:00:02:00
003 MU A C 00:00:00:00 00:00:05:00 01:00:00:00 01:00:05:00
";
        let doc = parse(edl).unwrap();
        let tl = build(&doc, Seconds::from_integer(30), 0, &some_asset).unwrap();
        assert_eq!(tl.tracks.len(), 2, "video + one distinct audio track");
        assert_eq!(tl.tracks[0].kind, TrackKind::Video);
        assert_eq!(tl.tracks[1].kind, TrackKind::Audio);
        // The linked A event (same reel + src as the video) is skipped;
        // only the MU event remains.
        assert_eq!(tl.tracks[1].segments.len(), 1);
    }
}
