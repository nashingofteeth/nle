//! Editing as data: reversible operations + an undo/redo history.
//!
//! One user action (split, trim, ripple-delete…) becomes a *batch* of
//! `EditOp`s. Every op carries enough data to invert itself, so undo is
//! just applying inverses in reverse order — no diffing, no snapshots of
//! the whole project. Batches are validated as a whole: if any op fails,
//! the already-applied ones are rolled back and the project is untouched.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write as _;
use std::path::Path;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::project::{Project, ProjectError};
use crate::time::{format_seconds, Seconds};
use crate::timeline::{Clip, TimelineError, Track, TrackKind};

/// Deepest undo stack we keep (batches).
pub const MAX_HISTORY: usize = 100;
/// Sidecar file format version (bump when EditOp changes shape).
pub const HISTORY_VERSION: u32 = 1;

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
    #[error("cannot extend clip #{id} — no source after {out} (asset ends at {duration})")]
    BeyondAsset { id: u32, out: String, duration: String },
    #[error("cannot extend clip #{id} — needs {needed} of source before its in-point ({ain})")]
    BeforeAsset { id: u32, needed: String, ain: String },
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

/// Serde wrapper: Option<Seconds> as "n/d" strings — enum variant fields
/// can't carry #[serde(with)] directly, so the newtype does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SerLen(#[serde(with = "crate::time::rational_ser::opt")] Option<Seconds>);

/// One reversible change to the project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EditOp {
    AddClip { track: usize, clip: Clip },
    /// The embedded clip is the removed snapshot (its inverse re-adds it).
    RemoveClip { track: usize, clip: Clip },
    /// `before` identifies the clip (by id) and is the undo payload.
    ReplaceClip { track: usize, before: Clip, after: Clip },
    /// Set the sequence tail length (Timeline::length) — how "leave a
    /// gap" is represented when the last clip is deleted or trimmed.
    SetLength {
        before: SerLen,
        after: SerLen,
    },
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
            EditOp::SetLength { after, .. } => {
                project.timeline.length = after.0;
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
            EditOp::SetLength { before, after } => EditOp::SetLength {
                before: SerLen(after.0),
                after: SerLen(before.0),
            },
        }
    }
}

/// A user action: ops applied together, undone together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// Serializable snapshot of both stacks (persisted in a sidecar file).
    pub fn state(&self) -> HistoryState {
        HistoryState {
            version: HISTORY_VERSION,
            done: self.done.clone(),
            undone: self.undone.clone(),
        }
    }

    /// Rebuild a History from a snapshot, honoring the depth cap.
    pub fn restore(state: HistoryState) -> Self {
        if state.version != HISTORY_VERSION {
            return Self::default();
        }
        let mut done = state.done;
        if done.len() > MAX_HISTORY {
            let excess = done.len() - MAX_HISTORY;
            done.drain(0..excess);
        }
        Self {
            done,
            undone: state.undone,
        }
    }

    /// Atomic save to the sidecar file (crash can't corrupt it).
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        let mut json = serde_json::to_string_pretty(&self.state())?;
        json.push('\n');
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let tmp = NamedTempFile::new_in(dir)?;
        tmp.as_file().write_all(json.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Load a sidecar. Missing or unreadable file → error; use
    /// `History::default()` at the call site to start fresh.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let text = fs::read_to_string(path)?;
        let state: HistoryState = serde_json::from_str(&text)
            .map_err(|e| std::io::Error::other(format!("corrupt history file: {e}")))?;
        if state.version != HISTORY_VERSION {
            return Err(std::io::Error::other(format!(
                "history version {} unsupported (this build reads v{HISTORY_VERSION})",
                state.version
            )));
        }
        Ok(Self::restore(state))
    }
}

/// Serializable snapshot of the undo/redo stacks (the sidecar file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryState {
    pub version: u32,
    pub done: Vec<Batch>,
    pub undone: Vec<Batch>,
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

/// The timeline's end after a batch that removes/replaces clips — used by
/// ripple ops to collapse the sequence tail to the new reality. Reads
/// each clip's end, preferring the post-batch value from `ReplaceClip`
/// ops when one exists for it.
fn end_after(project: &Project, skip_id: Option<u32>, ops: &[EditOp]) -> Seconds {
    let mut end = Seconds::from_integer(0);
    if let Some(track) = project.timeline.video_track() {
        for c in &track.clips {
            if Some(c.id) == skip_id {
                continue;
            }
            let e = ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    EditOp::ReplaceClip { before, after, .. } if before.id == c.id => {
                        Some(after.end())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| c.end());
            if e > end {
                end = e;
            }
        }
    }
    // A replaced clip that remains also counts (it may be the last one).
    for op in ops {
        if let EditOp::ReplaceClip { after, .. } = op
            && Some(after.id) != skip_id && after.end() > end {
                end = after.end();
            }
    }
    end
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
        && new_out > d
    {
        return Err(EditError::BeyondAsset {
            id: clip.id,
            out: format_seconds(&new_out),
            duration: format_seconds(&d),
        });
    }
    let after = Clip {
        id: clip.id,
        asset: clip.asset,
        start: clip.start,
        asset_in: clip.asset_in,
        asset_out: new_out,
    };
    at_least_a_frame(project, &after.duration())?;
    let mut ops = vec![EditOp::ReplaceClip {
        track: ti,
        before: clip.clone(),
        after,
    }];
    // Trimming the clip that defines the timeline end leaves a trailing
    // gap (o preserves; O collapses).
    if clip.end() >= project.timeline.end() {
        ops.push(EditOp::SetLength {
            before: SerLen(project.timeline.length),
            after: SerLen(Some(clip.end())),
        });
    }
    Ok(Batch {
        ops,
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
    let mut ops = vec![EditOp::RemoveClip { track: ti, clip: clip.clone() }];
    // Deleting the clip that defines the timeline end leaves a trailing
    // gap: raise the sequence tail to the old end (dd preserves; D collapses).
    let len_before = project.timeline.length;
    if clip.end() >= project.timeline.end()
        && len_before.as_ref().map(|l| clip.end() > *l).unwrap_or(true)
    {
        ops.push(EditOp::SetLength {
            before: SerLen(len_before),
            after: SerLen(Some(clip.end())),
        });
    }
    Ok(Batch {
        ops,
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
    // Ripple collapses the tail to the new end — the user chose to shrink.
    let new_end = end_after(project, Some(clip.id), &ops);
    ops.push(EditOp::SetLength {
        before: SerLen(project.timeline.length),
        after: SerLen(Some(new_end)),
    });
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
    // Ripple collapses the tail to the new end.
    let new_end = end_after(project, None, &ops);
    ops.push(EditOp::SetLength {
        before: SerLen(project.timeline.length),
        after: SerLen(Some(new_end)),
    });
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
    // Ripple collapses the tail to the new end.
    let new_end = end_after(project, None, &ops);
    ops.push(EditOp::SetLength {
        before: SerLen(project.timeline.length),
        after: SerLen(Some(new_end)),
    });
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
        // Trailing gap: dd shortens the sequence tail to the playhead.
        return Ok(Batch {
            ops: vec![EditOp::SetLength {
                before: SerLen(project.timeline.length),
                after: SerLen(Some(at)),
            }],
            desc: format!("delete trailing gap to {}", format_seconds(&at)),
        });
    }
    // Interior: the uniform shift pulls the last clip's end left —
    // collapsing the sequence tail along with it.
    let mut ops = ops;
    let new_end = end_after(project, None, &ops);
    ops.push(EditOp::SetLength {
        before: SerLen(project.timeline.length),
        after: SerLen(Some(new_end)),
    });
    Ok(Batch {
        ops,
        desc: format!(
            "delete gap {}–{}",
            format_seconds(&gap_start),
            format_seconds(&gap_end)
        ),
    })
}

/// Trim a gap's in-edge to the (snapped) playhead: the previous clip
/// extends into the gap (its source tail grows — needs material); a
/// leading gap instead shifts every clip left. A trailing gap's in-edge
/// also extends the last clip.
pub fn trim_gap_in(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let ti = video_track_index(project)?;
    let track = project
        .timeline
        .video_track()
        .ok_or(EditError::Timeline(TimelineError::NoVideoTrack))?;
    let at = snap(project, playhead)?;

    let mut gs = Seconds::from_integer(0);
    let mut prev: Option<&Clip> = None;
    let mut ge: Option<Seconds> = None;
    for c in &track.clips {
        if at < c.start {
            ge = Some(c.start);
            break;
        }
        gs = c.end();
        prev = Some(c);
    }
    // at must be strictly inside the gap [gs, ge) — ge may be None (trailing).
    if at <= gs || ge.as_ref().is_some_and(|ge| at >= *ge) {
        return Err(EditError::NoGap);
    }
    let delta = at - gs;

    match prev {
        Some(p) => {
            let mut after = p.clone();
            after.asset_out += delta;
            let asset = project
                .asset(p.asset)
                .ok_or(EditError::ClipNotFound(p.asset))?;
            if let Some(d) = asset.duration()
                && after.asset_out > d
            {
                return Err(EditError::BeyondAsset {
                    id: p.id,
                    out: format_seconds(&after.asset_out),
                    duration: format_seconds(&d),
                });
            }
            Ok(Batch {
                ops: vec![EditOp::ReplaceClip { track: ti, before: p.clone(), after }],
                desc: format!("trim gap in to {}", format_seconds(&at)),
            })
        }
        // Leading gap: nothing to extend — pull every clip left instead.
        // Say so explicitly: this is the one gap trim that always
        // shortens the timeline, and that shouldn't be a surprise.
        None => {
            let mut ops = track
                .clips
                .iter()
                .map(|c| {
                    let mut after = c.clone();
                    after.start -= delta;
                    EditOp::ReplaceClip { track: ti, before: c.clone(), after }
                })
                .collect::<Vec<_>>();
            // The uniform shift pulls the last clip's end left — collapse
            // the sequence tail along with it.
            let new_end = end_after(project, None, &ops);
            ops.push(EditOp::SetLength {
                before: SerLen(project.timeline.length),
                after: SerLen(Some(new_end)),
            });
            Ok(Batch {
                ops,
                desc: format!(
                    "trim gap in to {} — timeline shifted left {}",
                    format_seconds(&at),
                    format_seconds(&delta)
                ),
            })
        }
    }
}

/// Trim a gap's out-edge to the (snapped) playhead (`o`): the next
/// clip's head extends into the gap — its start moves left and its
/// source in-point reaches earlier (material permitting). Followers are
/// untouched and the timeline end doesn't move. A trailing gap has no
/// out-edge to trim — `o` there shortens the sequence tail instead.
pub fn trim_gap_out(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let ti = video_track_index(project)?;
    let track = project
        .timeline
        .video_track()
        .ok_or(EditError::Timeline(TimelineError::NoVideoTrack))?;
    let at = snap(project, playhead)?;

    let mut gs = Seconds::from_integer(0);
    let mut next: Option<&Clip> = None;
    for c in &track.clips {
        if at < c.start {
            next = Some(c);
            break;
        }
        gs = c.end();
    }
    let Some(next) = next else {
        // Trailing gap: `o` shortens the sequence tail to the playhead —
        // the only edge there is to trim.
        return Ok(Batch {
            ops: vec![EditOp::SetLength {
                before: SerLen(project.timeline.length),
                after: SerLen(Some(at)),
            }],
            desc: format!("trim tail to {}", format_seconds(&at)),
        });
    };
    if at <= gs || at >= next.start {
        return Err(EditError::NoGap);
    }
    let delta = next.start - at;
    let needed = format_seconds(&delta);

    let mut after = next.clone();
    after.start = at;
    after.asset_in = next.asset_in - delta;
    if after.asset_in < Seconds::from_integer(0) {
        return Err(EditError::BeforeAsset {
            id: next.id,
            needed,
            ain: format_seconds(&next.asset_in),
        });
    }
    Ok(Batch {
        ops: vec![EditOp::ReplaceClip {
            track: ti,
            before: next.clone(),
            after,
        }],
        desc: format!(
            "trim gap out to {} — clip #{} head extended",
            format_seconds(&at),
            next.id
        ),
    })
}

/// Ripple trim of a gap's out-edge (`O`): the next clip and everything
/// after it slide left — the timeline shrinks, no new gaps. A trailing
/// gap has nothing to slide.
pub fn trim_gap_out_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let ti = video_track_index(project)?;
    let track = project
        .timeline
        .video_track()
        .ok_or(EditError::Timeline(TimelineError::NoVideoTrack))?;
    let at = snap(project, playhead)?;

    let mut gs = Seconds::from_integer(0);
    let mut ge: Option<Seconds> = None;
    for c in &track.clips {
        if at < c.start {
            ge = Some(c.start);
            break;
        }
        gs = c.end();
    }
    let ge = ge.ok_or(EditError::NoGap)?; // trailing gap: no out-edge
    if at <= gs || at >= ge {
        return Err(EditError::NoGap);
    }
    let delta = ge - at;

    let ops: Vec<EditOp> = track
        .clips
        .iter()
        .filter(|c| c.start >= ge)
        .map(|c| {
            let mut after = c.clone();
            after.start -= delta;
            EditOp::ReplaceClip { track: ti, before: c.clone(), after }
        })
        .collect();
    if ops.is_empty() {
        return Err(EditError::NoGap);
    }
    let mut ops = ops;
    let new_end = end_after(project, None, &ops);
    ops.push(EditOp::SetLength {
        before: SerLen(project.timeline.length),
        after: SerLen(Some(new_end)),
    });
    Ok(Batch {
        ops,
        desc: format!(
            "trim gap out to {} — timeline shifted left {}",
            format_seconds(&at),
            format_seconds(&delta)
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
    fn history_persists_to_sidecar() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        h.undo(&mut p).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        h.save(&path).unwrap();

        let mut loaded = History::load(&path).unwrap();
        assert_eq!(loaded.state(), h.state());
        // the persisted REDO survives a "restart": a fresh process can
        // redo the split against a freshly loaded project
        assert!(!loaded.can_undo() && loaded.can_redo());
        let mut p2 = fixture();
        loaded.redo(&mut p2).unwrap();
        assert_eq!(ids(&p2), vec![1, 3, 2]);
    }

    #[test]
    fn history_restore_honors_cap_and_version() {
        let mut state = HistoryState { version: HISTORY_VERSION, done: vec![], undone: vec![] };
        for i in 0..(MAX_HISTORY + 5) {
            state.done.push(Batch {
                ops: vec![],
                desc: format!("batch {i}"),
            });
        }
        let h = History::restore(state.clone());
        assert_eq!(h.depth(), MAX_HISTORY);
        assert_eq!(h.state().done[0].desc, format!("batch {}", 5)); // oldest dropped

        state.version = 99;
        assert!(History::restore(state).done.is_empty());
    }

    #[test]
    fn corrupt_sidecar_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(History::load(&path).is_err());
        assert!(History::load(&dir.path().join("missing.json")).is_err());
    }

    #[test]
    fn gap_trims_shrink_from_either_side() {
        // [0..2) #1 src 1..3 · gap [2..3) · [3..5) #2 src 0..2 · [5..7) #3
        let mut p = fixture();
        let mut h = History::default();
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.add_clip(a, Some(Seconds::from_integer(5)), Seconds::from_integer(0), Seconds::from_integer(2))
            .unwrap(); // #3 [5..7)

        // trim gap in at 2.5: #1's tail extends into the gap
        let batch = trim_gap_in(&p, 2.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 1).end(), Seconds::new(5, 2));
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::new(7, 2)); // src 1..3.5
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(3), "next clip untouched");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));

        // trim gap out at 2.5 (o): #2's head extends into the gap
        p.timeline.tracks[0].clips[1].asset_in = Seconds::from_integer(1); // src 1..3
        p.timeline.tracks[0].clips[1].asset_out = Seconds::from_integer(3);
        let batch = trim_gap_out(&p, 2.5).unwrap();
        assert!(batch.desc.contains("head extended"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        let c2 = clip_by_id(&p, 2);
        assert_eq!(c2.start, Seconds::new(5, 2));
        assert_eq!(c2.asset_in, Seconds::new(1, 2), "ain 1 − 0.5");
        assert_eq!(c2.asset_out, Seconds::from_integer(3), "tail untouched");
        assert_eq!(c2.end(), Seconds::from_integer(5), "timeline end unchanged");
        assert_eq!(clip_by_id(&p, 3).start, Seconds::from_integer(5), "followers untouched");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(3));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(1));

        // material gate: ain 0.25 can't absorb a 0.5 head extension
        p.timeline.tracks[0].clips[1].asset_in = Seconds::new(1, 4);
        p.timeline.tracks[0].clips[1].asset_out = Seconds::new(9, 4); // src 0.25..2.25
        assert!(matches!(trim_gap_out(&p, 2.5), Err(EditError::BeforeAsset { .. })));

        // ripple variant (O): #2 and #3 slide left by 0.5, source kept
        let batch = trim_gap_out_ripple(&p, 2.5).unwrap();
        assert!(batch.desc.contains("timeline shifted left 0.5"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 2).start, Seconds::new(5, 2));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::new(1, 4), "shift keeps source");
        assert_eq!(clip_by_id(&p, 3).start, Seconds::new(9, 2));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(3));

        // ripple variant (O): #2 and #3 slide left by 0.5, source kept
        let batch = trim_gap_out_ripple(&p, 2.5).unwrap();
        assert!(batch.desc.contains("timeline shifted left 0.5"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 2).start, Seconds::new(5, 2));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::new(1, 4), "shift keeps source");
        assert_eq!(clip_by_id(&p, 3).start, Seconds::new(9, 2));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).start, Seconds::from_integer(3));

        // playhead not in the gap → rejected (both variants)
        assert!(matches!(trim_gap_in(&p, 1.0), Err(EditError::NoGap)));
        assert!(matches!(trim_gap_out(&p, 4.0), Err(EditError::NoGap)));
        assert!(matches!(trim_gap_out_ripple(&p, 4.0), Err(EditError::NoGap)));
    }

    #[test]
    fn gap_trim_edges_and_leading_gap() {
        // leading gap [0..1): #1 [1..3) src 0..2, #2 [3..5) src 0..2
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings { width: 320, height: 240 });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.add_clip(a, Some(Seconds::from_integer(1)), Seconds::from_integer(0), Seconds::from_integer(2))
            .unwrap();
        p.add_clip(a, Some(Seconds::from_integer(3)), Seconds::from_integer(0), Seconds::from_integer(2))
            .unwrap();
        let mut h = History::default();

        // trim the leading gap's in-edge at 0.5: everything slides left 0.5
        let batch = trim_gap_in(&p, 0.5).unwrap();
        assert!(batch.desc.contains("timeline shifted left 0.5"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 1).start, Seconds::new(1, 2));
        assert_eq!(
            clip_by_id(&p, 1).asset_in,
            Seconds::from_integer(0),
            "content unchanged, repositioned"
        );
        assert_eq!(clip_by_id(&p, 2).start, Seconds::new(5, 2));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 1).start, Seconds::from_integer(1));

        // trailing gap (dd preserved the end as tail length): `o` shortens
        // the tail; `O` has nothing to slide
        let batch = delete_clip(&p, 2).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(p.timeline.length, Some(Seconds::from_integer(5)), "dd on the last clip leaves the gap");
        assert!(matches!(trim_gap_out_ripple(&p, 3.5), Err(EditError::NoGap)));
        let batch = trim_gap_out(&p, 3.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(p.timeline.length, Some(Seconds::new(7, 2)));
        // ... but the gap's in-edge extends the last clip
        let batch = trim_gap_in(&p, 3.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 1).end(), Seconds::new(7, 2));
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::new(5, 2)); // src 0..2.5
    }

    #[test]
    fn gap_trim_respects_source_material() {
        // asset lasts 2.1s; clip #1 [1..3) src 0..2
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings { width: 320, height: 240 });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("2.1"));
        p.add_clip(a, Some(Seconds::from_integer(1)), Seconds::from_integer(0), Seconds::from_integer(2))
            .unwrap();
        // extending the tail past the asset's material → rejected
        assert!(matches!(trim_gap_in(&p, 3.2), Err(EditError::BeyondAsset { .. })));
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
