//! Editing as data: reversible operations + an undo/redo history.
//!
//! One user action (split, trim, ripple-delete…) becomes a *batch* of
//! `EditOp`s. Every op carries enough data to invert itself, so undo is
//! just applying inverses in reverse order — no diffing, no snapshots of
//! the whole project. Batches are validated as a whole: if any op fails,
//! the already-applied ones are rolled back and the project is untouched.

use thiserror::Error;

use crate::project::{Project, ProjectError};
use crate::time::{format_seconds, Seconds};
use crate::timeline::{Clip, TimelineError, Track, TrackKind};

/// Deepest undo stack we keep (batches).
pub const MAX_HISTORY: usize = 100;

#[derive(Debug, Error)]
pub enum EditError {
    #[error("clip #{0} not found")]
    ClipNotFound(u32),
    #[error("no track #{0}")]
    TrackNotFound(usize),
    #[error("playhead {0} is not inside a clip (j/w/b to reach one)")]
    NotInClip(String),
    #[error("result would be shorter than one frame")]
    TooShort,
    #[error("out-point {0} is beyond the asset's duration")]
    BeyondAsset(String),
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
    #[error("no frame rate set — add a clip first")]
    NoFps,
    #[error("playhead is not in a gap")]
    NoGap,
    #[error(transparent)]
    Timeline(#[from] TimelineError),
    #[error(transparent)]
    Project(#[from] ProjectError),
}

/// One reversible change to the project.
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    AddClip { track: usize, clip: Clip },
    /// The embedded clip is the removed snapshot (its inverse re-adds it).
    RemoveClip { track: usize, clip: Clip },
    /// `before` identifies the clip (by id) and is the undo payload.
    ReplaceClip { track: usize, before: Clip, after: Clip },
}

impl EditOp {
    /// Apply to the project. Overlaps and structural problems are
    /// rejected here, leaving the project untouched.
    pub fn apply(&self, project: &mut Project) -> Result<(), EditError> {
        match self {
            EditOp::AddClip { track, clip } => {
                track_mut(project, *track)?.try_insert(clip.clone())?;
            }
            EditOp::RemoveClip { track, clip } => {
                track_mut(project, *track)?.try_remove(clip.id)?;
            }
            EditOp::ReplaceClip { track, after, .. } => {
                track_mut(project, *track)?.try_replace(after.clone())?;
            }
        }
        Ok(())
    }

    /// The op that undoes this one.
    pub fn invert(&self) -> EditOp {
        match self {
            EditOp::AddClip { track, clip } => EditOp::RemoveClip {
                track: *track,
                clip: clip.clone(),
            },
            EditOp::RemoveClip { track, clip } => EditOp::AddClip {
                track: *track,
                clip: clip.clone(),
            },
            EditOp::ReplaceClip { track, before, after } => EditOp::ReplaceClip {
                track: *track,
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

/// A user action: ops applied together, undone together.
#[derive(Debug, Clone)]
pub struct Batch {
    pub ops: Vec<EditOp>,
    /// Human description, shown in the TUI message line.
    pub desc: String,
}

/// Undo/redo stack of batches. `apply` is the only way in — it validates,
/// rolls back on failure, clears the redo stack, and caps the depth.
#[derive(Debug, Default, Clone)]
pub struct History {
    done: Vec<Batch>,
    undone: Vec<Batch>,
}

impl History {
    pub fn apply(&mut self, project: &mut Project, batch: Batch) -> Result<(), EditError> {
        apply_batch(project, &batch.ops)?;
        project.validate()?;
        self.done.push(batch);
        self.undone.clear();
        if self.done.len() > MAX_HISTORY {
            let excess = self.done.len() - MAX_HISTORY;
            self.done.drain(0..excess);
        }
        Ok(())
    }

    pub fn undo(&mut self, project: &mut Project) -> Result<String, EditError> {
        let batch = self.done.pop().ok_or(EditError::NothingToUndo)?;
        let mut undone = batch.clone();
        undone.ops = batch.ops.iter().rev().map(|op| op.invert()).collect();
        apply_batch(project, &undone.ops)?;
        project.validate()?;
        self.undone.push(batch.clone());
        Ok(batch.desc)
    }

    pub fn redo(&mut self, project: &mut Project) -> Result<String, EditError> {
        let batch = self.undone.pop().ok_or(EditError::NothingToRedo)?;
        apply_batch(project, &batch.ops)?;
        project.validate()?;
        self.done.push(batch.clone());
        Ok(batch.desc)
    }

    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    pub fn depth(&self) -> usize {
        self.done.len()
    }
}

/// Apply ops in order; on any failure, roll back the applied prefix in
/// reverse so the project is exactly as it was before.
fn apply_batch(project: &mut Project, ops: &[EditOp]) -> Result<(), EditError> {
    for (applied, op) in ops.iter().enumerate() {
        if let Err(e) = op.apply(project) {
            for prev in ops[..applied].iter().rev() {
                // Inverses of validated ops must succeed; if they don't,
                // that's a bug we want to see loudly, not silently corrupt.
                prev.invert().apply(project).expect("history rollback");
            }
            return Err(e);
        }
    }
    Ok(())
}

fn track_mut(project: &mut Project, ti: usize) -> Result<&mut Track, EditError> {
    project
        .timeline
        .tracks
        .get_mut(ti)
        .ok_or(EditError::TrackNotFound(ti))
}

fn find_clip(project: &Project, clip_id: u32) -> Result<(usize, Clip), EditError> {
    for (ti, track) in project.timeline.tracks.iter().enumerate() {
        if let Some(c) = track.clips.iter().find(|c| c.id == clip_id) {
            return Ok((ti, c.clone()));
        }
    }
    Err(EditError::ClipNotFound(clip_id))
}

fn video_track_index(project: &Project) -> Result<usize, EditError> {
    project
        .timeline
        .tracks
        .iter()
        .position(|t| t.kind == TrackKind::Video)
        .ok_or(EditError::Timeline(TimelineError::NoVideoTrack))
}

/// Snap a (float) playhead to the frame grid of the project's fps.
fn snap(project: &Project, t: f64) -> Result<Seconds, EditError> {
    let fps = project.fps.ok_or(EditError::NoFps)?;
    let n = (t * secs_f64(&fps)).round() as i64;
    Ok(Seconds::new(n, 1) / fps)
}

fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

/// Result of a trim/split must be at least one frame long.
fn at_least_a_frame(project: &Project, dur: &Seconds) -> Result<(), EditError> {
    let fps = project.fps.ok_or(EditError::NoFps)?;
    if (*dur * fps).round().to_integer() >= 1 {
        Ok(())
    } else {
        Err(EditError::TooShort)
    }
}

// ------------------------------------------------------------ operations

/// Trim a clip's in-point to the (snapped) playhead. The clip's start and
/// asset_in move together, so the same source frames keep playing.
pub fn trim_in(project: &Project, clip_id: u32, playhead: f64) -> Result<Batch, EditError> {
    let (ti, clip) = find_clip(project, clip_id)?;
    let at = snap(project, playhead)?;
    if at <= clip.start || at >= clip.end() {
        return Err(EditError::NotInClip(format_seconds(&at)));
    }
    let delta = at - clip.start;
    let after = Clip {
        id: clip.id,
        asset: clip.asset,
        start: at,
        asset_in: clip.asset_in + delta,
        asset_out: clip.asset_out,
    };
    at_least_a_frame(project, &after.duration())?;
    Ok(Batch {
        ops: vec![EditOp::ReplaceClip { track: ti, before: clip, after }],
        desc: format!("trim in of clip #{clip_id} to {}", format_seconds(&at)),
    })
}

/// Trim a clip's out-point to the (snapped) playhead. The source range
/// shortens; the clip's position doesn't move.
pub fn trim_out(project: &Project, clip_id: u32, playhead: f64) -> Result<Batch, EditError> {
    let (ti, clip) = find_clip(project, clip_id)?;
    let at = snap(project, playhead)?;
    if at <= clip.start || at >= clip.end() {
        return Err(EditError::NotInClip(format_seconds(&at)));
    }
    let new_out = clip.asset_in + (at - clip.start);
    let asset = project
        .asset(clip.asset)
        .ok_or(EditError::ClipNotFound(clip.asset))?;
    if let Some(d) = asset.duration()
        && new_out > d {
            return Err(EditError::BeyondAsset(format_seconds(&new_out)));
        }
    let after = Clip {
        id: clip.id,
        asset: clip.asset,
        start: clip.start,
        asset_in: clip.asset_in,
        asset_out: new_out,
    };
    at_least_a_frame(project, &after.duration())?;
    Ok(Batch {
        ops: vec![EditOp::ReplaceClip { track: ti, before: clip, after }],
        desc: format!("trim out of clip #{clip_id} to {}", format_seconds(&at)),
    })
}

/// Split a clip at the (snapped) playhead into two clips.
pub fn split(project: &Project, clip_id: u32, playhead: f64) -> Result<Batch, EditError> {
    let (ti, clip) = find_clip(project, clip_id)?;
    let at = snap(project, playhead)?;
    if at <= clip.start || at >= clip.end() {
        return Err(EditError::NotInClip(format_seconds(&at)));
    }
    let left_len = at - clip.start;
    let right_len = clip.end() - at;
    at_least_a_frame(project, &left_len)?;
    at_least_a_frame(project, &right_len)?;

    let mut left = clip.clone();
    left.asset_out = clip.asset_in + left_len;
    let right = Clip {
        id: project.next_clip_id(),
        asset: clip.asset,
        start: at,
        asset_in: left.asset_out,
        asset_out: clip.asset_out,
    };
    Ok(Batch {
        ops: vec![
            EditOp::ReplaceClip { track: ti, before: clip.clone(), after: left },
            EditOp::AddClip { track: ti, clip: right },
        ],
        desc: format!("split clip #{} at {}", clip.id, format_seconds(&at)),
    })
}

/// Delete a clip, leaving a gap.
pub fn delete_clip(project: &Project, clip_id: u32) -> Result<Batch, EditError> {
    let (ti, clip) = find_clip(project, clip_id)?;
    Ok(Batch {
        ops: vec![EditOp::RemoveClip { track: ti, clip }],
        desc: format!("delete clip #{clip_id}"),
    })
}

/// Delete a clip and shift its *contiguous* successors left to close the
/// gap (a pre-existing gap stops the ripple).
pub fn delete_ripple(project: &Project, clip_id: u32) -> Result<Batch, EditError> {
    let (ti, clip) = find_clip(project, clip_id)?;
    let mut ops = vec![EditOp::RemoveClip { track: ti, clip: clip.clone() }];
    let track = &project.timeline.tracks[ti];
    push_chain_shift(&mut ops, track, ti, &clip.end(), &clip.duration());
    Ok(Batch {
        ops,
        desc: format!("ripple-delete clip #{clip_id}"),
    })
}

/// Append `ReplaceClip` ops shifting every clip contiguous from `from`
/// left by `by` (stops at a pre-existing gap). Shared by ripple delete
/// and ripple trims.
fn push_chain_shift(ops: &mut Vec<EditOp>, track: &Track, ti: usize, from: &Seconds, by: &Seconds) {
    let mut cursor = *from;
    for c in &track.clips {
        if c.start < cursor {
            continue; // at or before the edited clip
        }
        if c.start != cursor {
            break; // gap — the chain ends here
        }
        let mut after = c.clone();
        after.start -= *by;
        ops.push(EditOp::ReplaceClip {
            track: ti,
            before: c.clone(),
            after,
        });
        cursor += c.duration();
    }
}

/// Ripple trim-in (`I`): remove the clip's head up to the (snapped)
/// playhead, keep the clip's position, and shift contiguous successors
/// left — the timeline shrinks, no gap opens.
pub fn trim_in_ripple(project: &Project, clip_id: u32, playhead: f64) -> Result<Batch, EditError> {
    let (ti, clip) = find_clip(project, clip_id)?;
    let at = snap(project, playhead)?;
    if at <= clip.start || at >= clip.end() {
        return Err(EditError::NotInClip(format_seconds(&at)));
    }
    let delta = at - clip.start;
    let after = Clip {
        id: clip.id,
        asset: clip.asset,
        start: clip.start, // stays put
        asset_in: clip.asset_in + delta,
        asset_out: clip.asset_out,
    };
    at_least_a_frame(project, &after.duration())?;
    let mut ops = vec![EditOp::ReplaceClip {
        track: ti,
        before: clip.clone(),
        after,
    }];
    let track = &project.timeline.tracks[ti];
    push_chain_shift(&mut ops, track, ti, &clip.end(), &delta);
    Ok(Batch {
        ops,
        desc: format!("ripple trim in of clip #{clip_id} to {}", format_seconds(&at)),
    })
}

/// Ripple trim-out (`O`): remove the clip's tail from the (snapped)
/// playhead on, and shift contiguous successors left. No gap opens.
pub fn trim_out_ripple(project: &Project, clip_id: u32, playhead: f64) -> Result<Batch, EditError> {
    let (ti, clip) = find_clip(project, clip_id)?;
    let at = snap(project, playhead)?;
    if at <= clip.start || at >= clip.end() {
        return Err(EditError::NotInClip(format_seconds(&at)));
    }
    // new_out < old asset_out (which was within the asset), so no asset
    // bound check is needed.
    let after = Clip {
        id: clip.id,
        asset: clip.asset,
        start: clip.start,
        asset_in: clip.asset_in,
        asset_out: clip.asset_in + (at - clip.start),
    };
    at_least_a_frame(project, &after.duration())?;
    let removed = clip.end() - at;
    let mut ops = vec![EditOp::ReplaceClip {
        track: ti,
        before: clip.clone(),
        after,
    }];
    let track = &project.timeline.tracks[ti];
    push_chain_shift(&mut ops, track, ti, &clip.end(), &removed);
    Ok(Batch {
        ops,
        desc: format!("ripple trim out of clip #{clip_id} to {}", format_seconds(&at)),
    })
}

/// Delete the gap containing the (snapped) playhead: every clip after it
/// shifts left to close the gap. A trailing gap has nothing to shift.
pub fn delete_gap(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let ti = video_track_index(project)?;
    let track = project
        .timeline
        .video_track()
        .ok_or(EditError::Timeline(TimelineError::NoVideoTrack))?;
    let at = snap(project, playhead)?;

    // The gap containing `at`: [end of the last clip starting at-or-before
    // `at`, start of the first clip starting after `at`).
    let mut gap_start = Seconds::from_integer(0);
    let mut gap_end: Option<Seconds> = None;
    for c in &track.clips {
        if at < c.start {
            gap_end = Some(c.start);
            break;
        }
        gap_start = c.end();
    }
    let gap_end = gap_end.ok_or(EditError::NoGap)?; // trailing gap
    if gap_end <= gap_start {
        return Err(EditError::NoGap); // playhead sits inside a clip
    }
    let shift = gap_end - gap_start;

    let ops: Vec<EditOp> = track
        .clips
        .iter()
        .filter(|c| c.start >= gap_end)
        .map(|c| {
            let mut after = c.clone();
            after.start -= shift;
            EditOp::ReplaceClip {
                track: ti,
                before: c.clone(),
                after,
            }
        })
        .collect();
    if ops.is_empty() {
        return Err(EditError::NoGap);
    }
    Ok(Batch {
        ops,
        desc: format!(
            "delete gap {}–{}",
            format_seconds(&gap_start),
            format_seconds(&gap_end)
        ),
    })
}

/// Paste a yanked clip. `at = Some(playhead)` inserts at that position
/// (overlap-checked); `None` appends at the end of the video track.
pub fn paste(project: &Project, snapshot: &Clip, at: Option<f64>) -> Result<Batch, EditError> {
    let ti = video_track_index(project)?;
    let (start, at_desc) = match at {
        Some(t) => {
            let s = snap(project, t)?;
            (s, format!("at {}", format_seconds(&s)))
        }
        None => (
            project.timeline.video_track().unwrap().end(),
            "at end".to_string(),
        ),
    };
    let clip = Clip {
        id: project.next_clip_id(),
        asset: snapshot.asset,
        start,
        asset_in: snapshot.asset_in,
        asset_out: snapshot.asset_out,
    };
    let desc = format!("paste clip #{} {}", clip.id, at_desc);
    Ok(Batch {
        ops: vec![EditOp::AddClip { track: ti, clip }],
        desc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RawFormat, RawProbe, RawStream};
    use crate::project::VideoSettings;

    fn probe_report(duration: &str) -> crate::RawProbe {
        RawProbe {
            streams: vec![
                RawStream {
                    index: 0,
                    codec_type: Some("video".into()),
                    codec_name: Some("h264".into()),
                    width: Some(320),
                    height: Some(240),
                    r_frame_rate: Some("30/1".into()),
                    ..Default::default()
                },
                RawStream {
                    index: 1,
                    codec_type: Some("audio".into()),
                    codec_name: Some("aac".into()),
                    ..Default::default()
                },
            ],
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    /// Clips: #1 = [0..2) src 1..3, #2 = [3..5) src 0..2 (gap 2..3).
    fn fixture() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings { width: 320, height: 240 });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.add_clip(a, None, Seconds::from_integer(1), Seconds::from_integer(3))
            .unwrap();
        p.add_clip(a, Some(Seconds::from_integer(3)), Seconds::from_integer(0), Seconds::from_integer(2))
            .unwrap();
        p
    }

    fn clip_by_id(p: &Project, id: u32) -> Clip {
        p.timeline.video_track().unwrap().clips.iter().find(|c| c.id == id).unwrap().clone()
    }

    fn ids(p: &Project) -> Vec<u32> {
        p.timeline.video_track().unwrap().clips.iter().map(|c| c.id).collect()
    }

    #[test]
    fn split_math_and_undo_redo() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        // left half [0..1) src 1..2, right half [1..2) src 2..3, fresh id 3
        let left = clip_by_id(&p, 1);
        let right = clip_by_id(&p, 3);
        assert_eq!(left.start, Seconds::from_integer(0));
        assert_eq!(left.asset_out, Seconds::from_integer(2));
        assert_eq!(right.start, Seconds::from_integer(1));
        assert_eq!(right.asset_in, Seconds::from_integer(2));
        assert_eq!(right.asset_out, Seconds::from_integer(3));
        assert_eq!(ids(&p), vec![1, 3, 2]);

        h.undo(&mut p).unwrap();
        assert_eq!(ids(&p), vec![1, 2]);
        let restored = clip_by_id(&p, 1);
        assert_eq!(restored.asset_in, Seconds::from_integer(1));
        assert_eq!(restored.asset_out, Seconds::from_integer(3));

        h.redo(&mut p).unwrap();
        assert_eq!(ids(&p), vec![1, 3, 2]);
    }

    #[test]
    fn split_rejects_gaps_edges_and_tiny_results() {
        let mut p = fixture();
        // playhead in the gap
        assert!(matches!(split(&p, 1, 2.5), Err(EditError::NotInClip(_))));
        // on the clip's start edge
        assert!(split(&p, 1, 0.0).is_err());
        // snapping pulls 0.01s back to frame 0 → "not inside" wins
        assert!(matches!(split(&p, 1, 0.01), Err(EditError::NotInClip(_))));
        // non-frame-aligned end: the right side would be 0.015s (0.45 frames)
        let mut c = clip_by_id(&p, 1);
        c.asset_out = Seconds::new(603, 200); // 3.015 → clip [0..2.015)
        p.timeline.tracks[0].clips[0] = c;
        assert!(matches!(split(&p, 1, 2.0), Err(EditError::TooShort)));
    }

    #[test]
    fn trim_in_moves_start_and_source_together() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_in(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let c = clip_by_id(&p, 1);
        assert_eq!(c.start, Seconds::from_integer(1));
        assert_eq!(c.asset_in, Seconds::from_integer(2)); // 1 + 1
        assert_eq!(c.end(), Seconds::from_integer(2));
        // in the gap → model-level rejection too
        assert!(matches!(trim_in(&p, 1, 2.5), Err(EditError::NotInClip(_))));
    }

    #[test]
    fn trim_out_shortens_source_stays_put() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_out(&p, 2, 4.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        let c = clip_by_id(&p, 2);
        assert_eq!(c.start, Seconds::from_integer(3)); // didn't move
        assert_eq!(c.asset_out, Seconds::new(3, 2)); // 0 + 1.5
        // beyond the clip's own end is "not inside"
        assert!(matches!(trim_out(&p, 2, 5.5), Err(EditError::NotInClip(_))));
    }

    #[test]
    fn delete_leaves_gap_ripple_shifts_contiguous_only() {
        // [0..1) #1 · gap [1..2) · [2..3) #2 · [3..5) #3
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings { width: 320, height: 240 });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        let mk = |id: u32, start: i64, dur: i64| Clip {
            id,
            asset: a,
            start: Seconds::from_integer(start),
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(dur),
        };
        p.timeline.video_track_mut().unwrap().clips = vec![mk(1, 0, 1), mk(2, 2, 1), mk(3, 3, 2)];
        p.validate().unwrap();

        let mut h = History::default();
        let batch = delete_clip(&p, 1).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(ids(&p), vec![2, 3]);

        // ripple-delete #2: its only contiguous successor is #3
        let batch = delete_ripple(&p, 2).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(ids(&p), vec![3]);
        assert_eq!(clip_by_id(&p, 3).start, Seconds::from_integer(2));
        h.undo(&mut p).unwrap();
        assert_eq!(ids(&p), vec![2, 3]);
        assert_eq!(clip_by_id(&p, 3).start, Seconds::from_integer(3));

        // with a gap between #2 and the last clip, the ripple must stop
        let batch = paste(&p, &clip_by_id(&p, 3), None).unwrap();
        h.apply(&mut p, batch).unwrap(); // #4 at [5..7)
        let batch = delete_clip(&p, 3).unwrap();
        h.apply(&mut p, batch).unwrap(); // #3 gone → gap [3..5)
        let before = clip_by_id(&p, 4).start;
        let batch = delete_ripple(&p, 2).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 4).start, before, "ripple must stop at gaps");
    }

    #[test]
    fn paste_overlap_rejected_append_works() {
        let mut p = fixture();
        let mut h = History::default();
        let src = clip_by_id(&p, 1); // src 1..3, 2s long
        // paste at 1.0 overlaps clip #1 → error, project untouched
        let batch = paste(&p, &src, Some(1.0)).unwrap();
        assert!(h.apply(&mut p, batch).is_err());
        assert_eq!(ids(&p), vec![1, 2]);
        // paste in the free zone [2..3)? a 2s paste at 2.0 would hit #2 — rejected too
        let batch = paste(&p, &src, Some(2.0)).unwrap();
        assert!(h.apply(&mut p, batch).is_err());
        // append at end works
        let batch = paste(&p, &src, None).unwrap();
        h.apply(&mut p, batch).unwrap();
        let pasted = clip_by_id(&p, 3);
        assert_eq!(pasted.start, Seconds::from_integer(5));
        assert_eq!(pasted.asset_in, Seconds::from_integer(1)); // trimmed range kept
    }

    #[test]
    fn failed_batch_rolls_back_completely() {
        let mut p = fixture();
        let mut h = History::default();
        // remove #1 (ok), then add an overlapping clip (fails) → rollback
        let removed = clip_by_id(&p, 1);
        let bad = Clip {
            id: 99,
            asset: removed.asset,
            start: Seconds::from_integer(1), // inside where #1 will be... after removal it's a gap, but inside #2? no — 1..3 vs #2 3..5 → valid! pick 3.5 to collide with #2
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
        };
        let batch = Batch {
            ops: vec![
                EditOp::RemoveClip { track: 0, clip: removed.clone() },
                EditOp::AddClip { track: 0, clip: bad },
            ],
            desc: "test rollback".into(),
        };
        // after removing #1 the gap is [0..3); a clip at [3.5..5.5) collides with #2 [3..5)
        let mut batch = batch;
        if let EditOp::AddClip { clip, .. } = &mut batch.ops[1] {
            clip.start = Seconds::new(7, 2);
        }
        assert!(h.apply(&mut p, batch).is_err());
        assert_eq!(ids(&p), vec![1, 2], "rollback must restore clip #1");
        assert_eq!(clip_by_id(&p, 1).asset_in, Seconds::from_integer(1));
    }

    #[test]
    fn history_depth_is_capped() {
        let mut p = fixture();
        let mut h = History::default();
        let src = clip_by_id(&p, 1);
        for _ in 0..(MAX_HISTORY + 5) {
            let batch = paste(&p, &src, None).unwrap();
            h.apply(&mut p, batch).unwrap();
        }
        assert_eq!(h.depth(), MAX_HISTORY);
    }

    #[test]
    fn delete_gap_closes_it_up() {
        // [0..1) #1 · gap [1..3) · [3..5) #2 · [5..7) #3
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings { width: 320, height: 240 });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        let mk = |id: u32, start: i64, dur: i64| Clip {
            id,
            asset: a,
            start: Seconds::from_integer(start),
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(dur),
        };
        p.timeline.video_track_mut().unwrap().clips = vec![mk(1, 0, 1), mk(2, 3, 2), mk(3, 5, 2)];
        p.validate().unwrap();
        let mut h = History::default();

        // playhead in the middle of the gap
        let batch = delete_gap(&p, 2.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(ids(&p), vec![1, 2, 3]);
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(1));
        assert_eq!(clip_by_id(&p, 3).start, Seconds::from_integer(3));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(3));

        // leading gap: everything shifts
        let batch = delete_gap(&p, 0.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 1).start, Seconds::from_integer(0));
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(1));

        // inside a clip → not a gap
        assert!(matches!(delete_gap(&p, 1.5), Err(EditError::NoGap)));
        // trailing gap → nothing to shift
        let mut p2 = fixture();
        let batch = delete_clip(&p2, 2).unwrap();
        h.apply(&mut p2, batch).unwrap(); // now [0..2), gap 2..end
        assert!(matches!(delete_gap(&p2, 5.0), Err(EditError::NoGap)));
    }

    #[test]
    fn ripple_trims_close_the_gap() {
        // #1 [0..2) src 1..3, then a flush neighbor #3 [2..4) — remove the
        // fixture's #2 first so the neighbors really touch
        let mut p = fixture();
        p.remove_clip(2).unwrap();
        let mut h = History::default();
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.add_clip(a, Some(Seconds::from_integer(2)), Seconds::from_integer(0), Seconds::from_integer(2))
            .unwrap(); // #3 [2..4)

        // ripple trim-in #1 at 1.0: #1 becomes [0..1) src 2..3, the
        // neighbor shifts to 1
        let batch = trim_in_ripple(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let c1 = clip_by_id(&p, 1);
        assert_eq!(c1.start, Seconds::from_integer(0), "clip stays put");
        assert_eq!(c1.asset_in, Seconds::from_integer(2));
        assert_eq!(c1.end(), Seconds::from_integer(1));
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(1), "successor shifted");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_in, Seconds::from_integer(1));
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(2));

        // ripple trim-out #1 at 1.0: #1 [0..1) src 1..2, neighbor shifts to 1
        let batch = trim_out_ripple(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let c1 = clip_by_id(&p, 1);
        assert_eq!(c1.asset_out, Seconds::from_integer(2));
        assert_eq!(c1.end(), Seconds::from_integer(1));
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(1));
    }

    #[test]
    fn ripple_trims_stop_at_gaps() {
        // #1 [0..2) src 1..3, #2 [3..5) — a gap between them
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_in_ripple(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_in, Seconds::from_integer(2));
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(3), "gap stops the ripple");

        // fresh fixture for trim-out (the first apply mutated the model)
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_out_ripple(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(3), "gap stops the ripple");
    }

    #[test]
    fn redo_is_cleared_by_new_edits() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        h.undo(&mut p).unwrap();
        assert!(h.can_redo());
        let batch = delete_clip(&p, 2).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert!(!h.can_redo());
        assert!(matches!(h.redo(&mut p), Err(EditError::NothingToRedo)));
        h.undo(&mut p).unwrap();
        assert!(matches!(h.undo(&mut p), Err(EditError::NothingToUndo)));
    }
}
