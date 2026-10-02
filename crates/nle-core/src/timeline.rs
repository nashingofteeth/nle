//! The timeline: tracks of positioned clips. Pure data + pure operations.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::{format_seconds, rational_ser, Seconds};

#[derive(Debug, Error)]
pub enum TimelineError {
    #[error("no video track in timeline")]
    NoVideoTrack,
    #[error("clip #{0} not found")]
    ClipNotFound(u32),
    #[error("asset #{0} is not in the media pool")]
    UnknownAsset(u32),
    #[error("clip would overlap clip #{existing} at {at}")]
    Overlap { existing: u32, at: String },
    #[error("timeline corrupt: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

/// One piece of media placed on a track.
///
/// A clip plays the asset from `asset_in` to `asset_out` (source time),
/// starting at `start` on the timeline. Its length is asset_out − asset_in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: u32,
    /// Asset id in the project's media pool.
    pub asset: u32,
    #[serde(with = "rational_ser")]
    pub start: Seconds,
    #[serde(with = "rational_ser")]
    pub asset_in: Seconds,
    #[serde(with = "rational_ser")]
    pub asset_out: Seconds,
}

impl Clip {
    /// Clip length in exact seconds.
    pub fn duration(&self) -> Seconds {
        self.asset_out - self.asset_in
    }

    /// Where the clip ends on the timeline.
    pub fn end(&self) -> Seconds {
        self.start + self.duration()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub kind: TrackKind,
    /// Always kept sorted by `start`.
    pub clips: Vec<Clip>,
}

impl Track {
    /// Insert keeping sort order; reject overlaps.
    pub fn try_insert(&mut self, clip: Clip) -> Result<(), TimelineError> {
        for existing in &self.clips {
            if existing.start < clip.end() && clip.start < existing.end() {
                return Err(TimelineError::Overlap {
                    existing: existing.id,
                    at: format_seconds(&clip.start),
                });
            }
        }
        let pos = self.clips.partition_point(|c| c.start <= clip.start);
        self.clips.insert(pos, clip);
        Ok(())
    }

    /// Last end across all clips (0 for an empty track).
    pub fn end(&self) -> Seconds {
        self.clips
            .iter()
            .map(|c| c.end())
            .max()
            .unwrap_or_else(|| Seconds::from_integer(0))
    }

    pub fn find(&self, id: u32) -> Option<usize> {
        self.clips.iter().position(|c| c.id == id)
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
                clips: Vec::new(),
            }],
        }
    }

    pub fn video_track(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    pub fn video_track_mut(&mut self) -> Option<&mut Track> {
        self.tracks
            .iter_mut()
            .find(|t| t.kind == TrackKind::Video)
    }

    /// End of the whole timeline (across all tracks).
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
            if let Some(i) = track.find(id) {
                track.clips.remove(i);
                return Ok(());
            }
        }
        Err(TimelineError::ClipNotFound(id))
    }

    /// Structural sanity: positive spans, sorted non-overlapping clips.
    pub fn validate(&self) -> Result<(), TimelineError> {
        for (ti, track) in self.tracks.iter().enumerate() {
            let mut prev_end = Seconds::from_integer(0);
            for (ci, clip) in track.clips.iter().enumerate() {
                if clip.asset_out <= clip.asset_in {
                    return Err(TimelineError::Corrupt(format!(
                        "track {ti} clip {ci} (#{}) has a non-positive span",
                        clip.id
                    )));
                }
                if clip.start < prev_end {
                    return Err(TimelineError::Corrupt(format!(
                        "track {ti}: clip #{} starts at {} before the previous clip ends at {}",
                        clip.id,
                        format_seconds(&clip.start),
                        format_seconds(&prev_end)
                    )));
                }
                prev_end = clip.end();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: u32, start: i64, dur: i64) -> Clip {
        Clip {
            id,
            asset: 1,
            start: Seconds::from_integer(start),
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(dur),
        }
    }

    #[test]
    fn insert_keeps_order_and_rejects_overlap() {
        let mut track = Track { kind: TrackKind::Video, clips: vec![] };
        track.try_insert(clip(1, 0, 5)).unwrap();
        track.try_insert(clip(2, 5, 5)).unwrap();
        assert_eq!(track.clips.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1, 2]);
        // overlapping clip #1
        assert!(matches!(
            track.try_insert(clip(3, 4, 5)),
            Err(TimelineError::Overlap { existing: 1, .. })
        ));
        // gap insert sorts into place
        track.try_insert(clip(4, 12, 5)).unwrap();
        track.try_insert(clip(5, 10, 2)).unwrap();
        assert_eq!(track.clips.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1, 2, 5, 4]);
        assert_eq!(track.end(), Seconds::from_integer(17));
    }

    #[test]
    fn remove_and_validate() {
        let mut track = Track { kind: TrackKind::Video, clips: vec![] };
        track.try_insert(clip(1, 0, 5)).unwrap();
        track.try_insert(clip(2, 5, 5)).unwrap();
        let mut timeline = Timeline { tracks: vec![track] };
        timeline.remove_clip(2).unwrap();
        assert!(timeline.remove_clip(99).is_err());
        timeline.validate().unwrap();

        // corrupt: overlapping clips
        let mut bad = Timeline::new();
        bad.tracks[0].clips = vec![clip(1, 0, 5), clip(2, 3, 5)];
        assert!(bad.validate().is_err());
    }
}
