//! CMX3600 EDL parsing — pure, no processes.
//!
//! An EDL is a text list of events (`NNN  REEL  TRK  TRANS  SRC_IN SRC_OUT
//! REC_IN REC_OUT`) plus `TITLE:` / `FCM:` headers and `*` comment lines
//! (which usually carry `FROM CLIP NAME:` / `SOURCE FILE:` — the best
//! asset-matching hints). Cuts only: dissolves and wipes are rejected.

use thiserror::Error;

use crate::time::{Timecode, parse_timecode};

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
}
