//! The timeline: tracks of sequential segments — clips and gaps.
//!
//! A track is a *sequence*: segments play back-to-back, and a clip's
//! start is DERIVED (the sum of the durations before it). Overlaps are
//! unrepresentable; gaps — leading, interior, trailing — are ordinary
//! segments, not special state. Removing a segment IS a ripple; replacing
//! a clip with a gap IS "leave a gap".

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::{Seconds, format_seconds, rational_ser};

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

/// The longest timeline we accept, in seconds — a day. Not a workflow
/// limit (nobody edits a day): it is the guard that keeps duration sums
/// (i64 rationals) from overflowing, which wraps a track's length
/// negative in release builds and panics in debug ones.
pub const MAX_TIMELINE_SECONDS: i64 = 24 * 60 * 60;

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
    /// Playback rate relative to the source: timeline duration is
    /// `(asset_out - asset_in) / speed`. `1` is normal speed; a Kdenlive
    /// rate conform (e.g. 24 fps project over 29.97 fps media) is
    /// `project_rate / media_rate`. Never zero or negative.
    #[serde(with = "rational_ser")]
    pub speed: Seconds,
}

impl Clip {
    /// A normal-speed clip (`speed = 1`).
    pub fn new(id: u32, asset: u32, asset_in: Seconds, asset_out: Seconds) -> Self {
        Self {
            id,
            asset,
            asset_in,
            asset_out,
            speed: Seconds::from_integer(1),
        }
    }

    /// The source span the clip reads, before the speed factor.
    pub fn source_duration(&self) -> Seconds {
        self.asset_out - self.asset_in
    }

    /// The timeline span the clip occupies (source scaled by `speed`).
    pub fn duration(&self) -> Seconds {
        self.source_duration() / self.speed
    }

    /// Source time at a timeline offset inside the clip (timeline →
    /// source). Edits move in timeline time but `asset_in/out` live in
    /// source time, so crossing that boundary scales by `speed`.
    pub fn source_at(&self, timeline_offset: Seconds) -> Seconds {
        self.asset_in + timeline_offset * self.speed
    }

    /// Timeline offset of a source time inside the clip (source →
    /// timeline) — the inverse of [`Clip::source_at`].
    pub fn timeline_offset_of(&self, source: Seconds) -> Seconds {
        (source - self.asset_in) / self.speed
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
    /// A muted track contributes nothing to the mix. Defaults to `false` so
    /// a track written before this field existed still parses.
    #[serde(default)]
    pub muted: bool,
    pub segments: Vec<Segment>,
}

impl Track {
    /// A new, empty track of `kind`.
    pub fn new(kind: TrackKind) -> Self {
        Self {
            kind,
            muted: false,
            segments: Vec::new(),
        }
    }

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
            tracks: vec![Track::new(TrackKind::Video)],
        }
    }

    /// Track at `ti` (by position — the stacking order).
    pub fn track(&self, ti: usize) -> Option<&Track> {
        self.tracks.get(ti)
    }

    pub fn track_mut(&mut self, ti: usize) -> Option<&mut Track> {
        self.tracks.get_mut(ti)
    }

    /// All video tracks, in stacking order.
    pub fn video_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Video)
    }

    /// All audio tracks, in stacking order.
    pub fn audio_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Audio)
    }

    /// Append a track of `kind`; returns its index.
    pub fn add_track(&mut self, kind: TrackKind) -> usize {
        self.tracks.push(Track::new(kind));
        self.tracks.len() - 1
    }

    /// The first video track. Editing still assumes a single video track
    /// until the ops take an explicit index (M4 step 2).
    pub fn video_track(&self) -> Option<&Track> {
        self.video_tracks().next()
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

    /// Structural sanity: at least one video track, positive spans and
    /// gaps. Valid asset refs are the PROJECT's job (it owns the pool).
    /// Overlaps are unrepresentable.
    pub fn validate(&self) -> Result<(), TimelineError> {
        if self.video_tracks().next().is_none() {
            return Err(TimelineError::NoVideoTrack);
        }
        let limit = Seconds::from_integer(MAX_TIMELINE_SECONDS);
        for (ti, track) in self.tracks.iter().enumerate() {
            // A length that summed *past* the limit wrapped negative; one
            // that merely grew too long is caught here too.
            let end = track.end();
            if end < Seconds::from_integer(0) || end > limit {
                return Err(TimelineError::Corrupt(format!(
                    "track {ti} length {} is out of range (limit {MAX_TIMELINE_SECONDS} s)",
                    format_seconds(&end)
                )));
            }
            for (si, seg) in track.segments.iter().enumerate() {
                let bad =
                    |what: &str| TimelineError::Corrupt(format!("track {ti} segment {si}: {what}"));
                match seg {
                    Segment::Clip(c) => {
                        if c.asset_out <= c.asset_in {
                            return Err(bad("clip has a non-positive source span"));
                        }
                        if c.speed <= Seconds::from_integer(0) {
                            return Err(bad("clip has a non-positive speed"));
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
            speed: Seconds::from_integer(1),
        })
    }

    fn gap(dur: i64) -> Segment {
        Segment::Gap(Gap {
            duration: Seconds::from_integer(dur),
        })
    }

    #[test]
    fn source_mapping_scales_by_speed() {
        // 2× clip reading source [1, 5): 1s of timeline = 2s of source.
        let c = Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::from_integer(2),
        };
        assert_eq!(
            c.source_at(Seconds::from_integer(1)),
            Seconds::from_integer(3)
        );
        assert_eq!(
            c.timeline_offset_of(Seconds::from_integer(3)),
            Seconds::from_integer(1)
        );
    }

    #[test]
    fn end_is_the_sum_of_segments() {
        let t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![clip(1, 2), gap(1), clip(2, 3)],
        };
        assert_eq!(t.end(), Seconds::from_integer(6));
        assert_eq!(t.start_of(0), Seconds::from_integer(0));
        assert_eq!(t.start_of(1), Seconds::from_integer(2));
        assert_eq!(t.start_of(2), Seconds::from_integer(3));
        assert_eq!(t.start_of(3), Seconds::from_integer(6));
    }

    #[test]
    fn index_at_and_clip_at() {
        let t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![clip(1, 2), gap(1), clip(2, 3)],
        };
        assert_eq!(t.index_at(Seconds::from_integer(0)), Some(0));
        assert_eq!(t.index_at(Seconds::from_integer(2)), Some(1), "gap start");
        assert_eq!(
            t.index_at(Seconds::from_integer(3)),
            Some(2),
            "the clip after the gap"
        );
        assert_eq!(
            t.index_at(Seconds::from_integer(6)),
            None,
            "the end belongs to no segment"
        );
        assert!(t.clip_at(Seconds::from_integer(4)).is_some());
        assert!(
            t.clip_at(Seconds::from_integer(2)).is_none(),
            "inside a gap"
        );
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
            speed: Seconds::from_integer(1),
        })];
        assert!(p_timeline.validate().is_err());
        p_timeline.tracks[0].segments = vec![gap(0)];
        assert!(p_timeline.validate().is_err());
    }

    #[test]
    fn tracks_are_addressed_by_index_and_kind() {
        let mut t = Timeline::new();
        assert_eq!(t.tracks.len(), 1, "one video track to start");
        assert_eq!(t.video_tracks().count(), 1);
        assert_eq!(t.audio_tracks().count(), 0);
        assert_eq!(t.video_track().unwrap().kind, TrackKind::Video);
        assert!(t.track(1).is_none());

        let ai = t.add_track(TrackKind::Audio);
        assert_eq!(ai, 1, "add_track returns the new index");
        assert_eq!(t.track(1).unwrap().kind, TrackKind::Audio);
        assert_eq!(t.video_tracks().count(), 1);
        assert_eq!(t.audio_tracks().count(), 1);
        // video_track() is the *first* video, not the last track.
        assert_eq!(t.video_track().unwrap().kind, TrackKind::Video);
        t.track_mut(1).unwrap().muted = true;
        assert!(t.track(1).unwrap().muted);
    }

    #[test]
    fn muted_defaults_false_and_round_trips() {
        let t = Track::new(TrackKind::Audio);
        assert!(!t.muted, "new tracks are not muted");
        // A track written before the field existed parses as unmuted.
        let legacy: Track = serde_json::from_str(r#"{"kind":"Video","segments":[]}"#).unwrap();
        assert!(!legacy.muted);
        // And it round-trips once set.
        let mut t = Track::new(TrackKind::Video);
        t.muted = true;
        let json = serde_json::to_string(&t).unwrap();
        let back: Track = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn validate_requires_a_video_track() {
        let no_video = Timeline {
            tracks: vec![Track::new(TrackKind::Audio)],
        };
        assert!(matches!(
            no_video.validate(),
            Err(TimelineError::NoVideoTrack)
        ));

        let mut ok = Timeline::new();
        ok.add_track(TrackKind::Audio);
        assert!(ok.validate().is_ok());
    }
}
