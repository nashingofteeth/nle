//! The timeline: tracks of sequential segments — clips and gaps.
//!
//! A track is a *sequence*: segments play back-to-back, and a clip's
//! start is DERIVED (the sum of the durations before it). Overlaps are
//! unrepresentable; gaps — leading, interior, trailing — are ordinary
//! segments, not special state. Removing a segment IS a ripple; replacing
//! a clip with a gap IS "leave a gap".

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::{format_seconds, rational_ser, Seconds};

#[derive(Debug, Error)]
pub enum TimelineError {
    #[error("no video track in timeline")]
    NoVideoTrack,
    #[error("clip #{0} not found")]
    ClipNotFound(u32),
    #[error("no segment at index {0}")]
    SegmentNotFound(usize),
    #[error("timeline corrupt: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

/// One piece of media on a track. Its timeline position is derived from
/// the segments before it — a clip only owns its source range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: u32,
    pub asset: u32,
    #[serde(with = "rational_ser")]
    pub asset_in: Seconds,
    #[serde(with = "rational_ser")]
    pub asset_out: Seconds,
}

impl Clip {
    pub fn duration(&self) -> Seconds {
        self.asset_out - self.asset_in
    }
}

/// A stretch of nothing: black + silence. Leading, interior, and
/// trailing space are all just Gap segments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    #[serde(with = "rational_ser")]
    pub duration: Seconds,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Segment {
    Clip(Clip),
    Gap(Gap),
}

impl Segment {
    pub fn duration(&self) -> Seconds {
        match self {
            Segment::Clip(c) => c.duration(),
            Segment::Gap(g) => g.duration,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub kind: TrackKind,
    pub segments: Vec<Segment>,
}

impl Track {
    /// Total duration: the sum of segment durations.
    pub fn end(&self) -> Seconds {
        self.segments
            .iter()
            .fold(Seconds::from_integer(0), |acc, s| acc + s.duration())
    }

    /// Start time of the segment at `index` (prefix sum of durations).
    pub fn start_of(&self, index: usize) -> Seconds {
        self.segments
            .iter()
            .take(index)
            .fold(Seconds::from_integer(0), |acc, s| acc + s.duration())
    }

    /// Index of the segment containing `t` (start ≤ t < end).
    pub fn index_at(&self, t: Seconds) -> Option<usize> {
        let mut acc = Seconds::from_integer(0);
        for (i, s) in self.segments.iter().enumerate() {
            let end = acc + s.duration();
            if acc <= t && t < end {
                return Some(i);
            }
            acc = end;
        }
        None
    }

    /// The clip segment containing `t`, if any.
    pub fn clip_at(&self, t: Seconds) -> Option<&Clip> {
        let i = self.index_at(t)?;
        match &self.segments[i] {
            Segment::Clip(c) => Some(c),
            Segment::Gap(_) => None,
        }
    }

    /// Index of the clip segment with the given id.
    pub fn clip_index(&self, id: u32) -> Option<usize> {
        self.segments
            .iter()
            .position(|s| matches!(s, Segment::Clip(c) if c.id == id))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    pub tracks: Vec<Track>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    /// A timeline with one empty video track.
    pub fn new() -> Self {
        Self {
            tracks: vec![Track {
                kind: TrackKind::Video,
                segments: Vec::new(),
            }],
        }
    }

    pub fn video_track(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    pub fn video_track_mut(&mut self) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.kind == TrackKind::Video)
    }

    /// End of the timeline (across all tracks).
    pub fn end(&self) -> Seconds {
        self.tracks
            .iter()
            .map(|t| t.end())
            .max()
            .unwrap_or_else(|| Seconds::from_integer(0))
    }

    /// Remove a clip by id, searching all tracks.
    pub fn remove_clip(&mut self, id: u32) -> Result<(), TimelineError> {
        for track in &mut self.tracks {
            if let Some(i) = track.clip_index(id) {
                track.segments.remove(i);
                return Ok(());
            }
        }
        Err(TimelineError::ClipNotFound(id))
    }

    /// Structural sanity: positive spans and gaps, valid asset refs are
    /// the PROJECT's job (it owns the pool). Overlaps are unrepresentable.
    pub fn validate(&self) -> Result<(), TimelineError> {
        for (ti, track) in self.tracks.iter().enumerate() {
            for (si, seg) in track.segments.iter().enumerate() {
                let bad = |what: &str| {
                    TimelineError::Corrupt(format!(
                        "track {ti} segment {si}: {what}"
                    ))
                };
                match seg {
                    Segment::Clip(c) => {
                        if c.asset_out <= c.asset_in {
                            return Err(bad("clip has a non-positive source span"));
                        }
                    }
                    Segment::Gap(g) => {
                        if g.duration <= Seconds::from_integer(0) {
                            return Err(bad("gap has a non-positive duration"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Format helper re-exported for op descriptions.
#[allow(unused_imports)]
use format_seconds as _fmt_unused;

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: u32, dur: i64) -> Segment {
        Segment::Clip(Clip {
            id,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(dur),
        })
    }

    fn gap(dur: i64) -> Segment {
        Segment::Gap(Gap { duration: Seconds::from_integer(dur) })
    }

    #[test]
    fn end_is_the_sum_of_segments() {
        let t = Track { kind: TrackKind::Video, segments: vec![clip(1, 2), gap(1), clip(2, 3)] };
        assert_eq!(t.end(), Seconds::from_integer(6));
        assert_eq!(t.start_of(0), Seconds::from_integer(0));
        assert_eq!(t.start_of(1), Seconds::from_integer(2));
        assert_eq!(t.start_of(2), Seconds::from_integer(3));
        assert_eq!(t.start_of(3), Seconds::from_integer(6));
    }

    #[test]
    fn index_at_and_clip_at() {
        let t = Track { kind: TrackKind::Video, segments: vec![clip(1, 2), gap(1), clip(2, 3)] };
        assert_eq!(t.index_at(Seconds::from_integer(0)), Some(0));
        assert_eq!(t.index_at(Seconds::from_integer(2)), Some(1), "gap start");
        assert_eq!(t.index_at(Seconds::from_integer(3)), Some(2), "the clip after the gap");
        assert_eq!(t.index_at(Seconds::from_integer(6)), None, "the end belongs to no segment");
        assert!(t.clip_at(Seconds::from_integer(4)).is_some());
        assert!(t.clip_at(Seconds::from_integer(2)).is_none(), "inside a gap");
        assert_eq!(t.clip_index(2), Some(2));
        assert_eq!(t.clip_index(9), None);
    }

    #[test]
    fn validate_rejects_bad_spans() {
        let mut p_timeline = Timeline::new();
        p_timeline.tracks[0].segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(2),
            asset_out: Seconds::from_integer(2),
        })];
        assert!(p_timeline.validate().is_err());
        p_timeline.tracks[0].segments = vec![gap(0)];
        assert!(p_timeline.validate().is_err());
    }
}
