//! Editing as data: reversible segment operations + undo/redo history.
//!
//! Every edit is a small batch of segment ops (Insert / Remove / Replace
//! at a track index). Because a track is a SEQUENCE, the old special
//! cases are gone:
//!
//! - removing a segment is inherently a ripple (followers slide left);
//! - replacing a clip with a gap preserves the timeline end (dd);
//! - inserting a segment splits whatever is there and grows the end (P);
//! - the trailing gap is an ordinary segment — never special state.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::Path;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::project::{Project, ProjectError};
use crate::time::{format_seconds, Seconds};
use crate::timeline::{Clip, Gap, Segment, TimelineError, TrackKind};

/// Deepest undo stack we keep (batches).
pub const MAX_HISTORY: usize = 100;
/// Sidecar file format version (bump when EditOp changes shape).
pub const HISTORY_VERSION: u32 = 2;

#[derive(Debug, Error)]
pub enum EditError {
    #[error("clip #{0} not found")]
    ClipNotFound(u32),
    #[error("no track #{0}")]
    TrackNotFound(usize),
    #[error("no segment at index {0}")]
    SegmentNotFound(usize),
    #[error("playhead is not in a gap")]
    NoGap,
    #[error("no segment under the playhead (empty timeline?)")]
    NoSegment,
    #[error("nothing to trim at {0} — the playhead is on a segment edge")]
    NothingToTrim(String),
    #[error("result would be shorter than one frame")]
    TooShort,
    #[error("cannot extend clip #{id} — no source after {out} (asset ends at {duration})")]
    BeyondAsset { id: u32, out: String, duration: String },
    #[error("cannot extend clip #{id} — needs {needed} of source before its in-point ({ain})")]
    BeforeAsset { id: u32, needed: String, ain: String },
    #[error("paste needs a gap with room — the playhead is inside a clip, or the gap is too small")]
    NoRoom,
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
    #[error("no frame rate set — add a clip first")]
    NoFps,
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

/// One reversible change: a segment inserted / removed / replaced at an
/// index of a track's segment list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EditOp {
    Insert { track: usize, index: usize, segment: Segment },
    /// The embedded segment is the removed snapshot (its inverse re-adds it).
    Remove { track: usize, index: usize, segment: Segment },
    /// `before` is the safety snapshot: applying verifies it's still there.
    Replace { track: usize, index: usize, before: Segment, after: Segment },
}

impl EditOp {
    /// Apply to the project.
    pub fn apply(&self, project: &mut Project) -> Result<(), EditError> {
        let track = *match self {
            EditOp::Insert { track, .. }
            | EditOp::Remove { track, .. }
            | EditOp::Replace { track, .. } => track,
        };
        let segments = &mut project
            .timeline
            .tracks
            .get_mut(track)
            .ok_or(EditError::TrackNotFound(track))?
            .segments;
        match self {
            EditOp::Insert { index, segment, .. } => {
                if *index > segments.len() {
                    return Err(EditError::SegmentNotFound(*index));
                }
                segments.insert(*index, segment.clone());
            }
            EditOp::Remove { index, segment, .. } => {
                let current = segments
                    .get_mut(*index)
                    .ok_or(EditError::SegmentNotFound(*index))?;
                if current != segment {
                    return Err(EditError::Timeline(TimelineError::Corrupt(format!(
                        "segment {index} changed under the undo stack"
                    ))));
                }
                segments.remove(*index);
            }
            EditOp::Replace { index, before, after, .. } => {
                let current = segments
                    .get_mut(*index)
                    .ok_or(EditError::SegmentNotFound(*index))?;
                if current != before {
                    return Err(EditError::Timeline(TimelineError::Corrupt(format!(
                        "segment {index} changed under the undo stack"
                    ))));
                }
                *current = after.clone();
            }
        }
        Ok(())
    }

    /// The op that undoes this one.
    pub fn invert(&self) -> EditOp {
        match self {
            EditOp::Insert { track, index, segment } => EditOp::Remove {
                track: *track,
                index: *index,
                segment: segment.clone(),
            },
            EditOp::Remove { track, index, segment } => EditOp::Insert {
                track: *track,
                index: *index,
                segment: segment.clone(),
            },
            EditOp::Replace { track, index, before, after } => EditOp::Replace {
                track: *track,
                index: *index,
                before: after.clone(),
                after: before.clone(),
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
        let inverses: Vec<EditOp> = batch.ops.iter().rev().map(|op| op.invert()).collect();
        apply_batch(project, &inverses)?;
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
        let text = std::fs::read_to_string(path)?;
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
                // Inverses of applied ops must succeed; if they don't,
                // that's a bug we want to see loudly, not silently corrupt.
                prev.invert().apply(project).expect("history rollback");
            }
            return Err(e);
        }
    }
    Ok(())
}

// ------------------------------------------------------------ helpers

fn video_track_index(project: &Project) -> Result<usize, EditError> {
    project
        .timeline
        .tracks
        .iter()
        .position(|t| t.kind == TrackKind::Video)
        .ok_or(EditError::Timeline(TimelineError::NoVideoTrack))
}

/// Snap a float playhead to the nearest frame of the project's fps.
/// Without a known fps (empty project), whole seconds are the grid.
fn snap(project: &Project, t: f64) -> Result<Seconds, EditError> {
    match project.fps {
        Some(fps) => {
            let n = (t * secs_f64(&fps)).round() as i64;
            Ok(Seconds::new(n, 1) / fps)
        }
        None => Ok(Seconds::from_integer(t.round() as i64)),
    }
}

fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

/// Result of a trim must leave at least one frame.
fn at_least_a_frame(project: &Project, dur: &Seconds) -> Result<(), EditError> {
    let fps = project.fps.ok_or(EditError::NoFps)?;
    if (*dur * fps).round().to_integer() >= 1 {
        Ok(())
    } else {
        Err(EditError::TooShort)
    }
}

/// The clip segment containing the (snapped) playhead.
fn clip_at(project: &Project, t: &Seconds) -> Result<(usize, usize, Clip), EditError> {
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let si = track.index_at(*t).ok_or(EditError::NoSegment)?;
    match &track.segments[si] {
        Segment::Clip(c) => Ok((ti, si, c.clone())),
        Segment::Gap(_) => Err(EditError::NoGap),
    }
}

/// The contiguous gap REGION containing the (snapped) playhead —
/// adjacent gaps read as one region. Returns
/// (track, first index, last index, region start, region end).
fn gap_region_at(project: &Project, t: &Seconds) -> Result<(usize, usize, usize, Seconds, Seconds), EditError> {
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let si = track.index_at(*t).ok_or(EditError::NoSegment)?;
    if !matches!(track.segments[si], Segment::Gap(_)) {
        return Err(EditError::NoGap); // inside a clip
    }
    let mut first = si;
    while first > 0 && matches!(track.segments[first - 1], Segment::Gap(_)) {
        first -= 1;
    }
    let mut last = si;
    while last + 1 < track.segments.len() && matches!(track.segments[last + 1], Segment::Gap(_)) {
        last += 1;
    }
    let start = track.start_of(first);
    let end = track.start_of(last) + track.segments[last].duration();
    Ok((ti, first, last, start, end))
}

/// Build a Replace+Inserts batch that swaps the segment at `index` for a
/// list of pieces (splits and inserts). The first piece rides the
/// Replace, the rest are Inserted after it.
fn replace_with_many(track_idx: usize, index: usize, before: &Segment, pieces: Vec<Segment>) -> Vec<EditOp> {
    let mut ops = Vec::new();
    ops.push(EditOp::Replace {
        track: track_idx,
        index,
        before: before.clone(),
        after: pieces[0].clone(),
    });
    for (k, piece) in pieces[1..].iter().enumerate() {
        ops.push(EditOp::Insert {
            track: track_idx,
            index: index + 1 + k,
            segment: piece.clone(),
        });
    }
    ops
}

/// A gap piece, omitted when its duration rounds to nothing.
fn gap_piece(duration: Seconds) -> Option<Segment> {
    if duration > Seconds::from_integer(0) {
        Some(Segment::Gap(Gap { duration }))
    } else {
        None
    }
}

// ------------------------------------------------------------ operations

/// `i` — trim the in-edge of whatever is under the playhead:
/// - clip: its head is cut at the playhead and a gap of the trimmed
///   material takes its place (everything else stays put);
/// - gap: the previous clip's tail extends into it (material permitting);
///   a leading gap shortens from the front.
pub fn trim_in(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let si = track.index_at(t).ok_or(EditError::NoSegment)?;

    if let Segment::Clip(c) = &track.segments[si] {
        let seg_start = track.start_of(si);
        let delta = t - seg_start;
        if delta <= Seconds::from_integer(0) {
            return Err(EditError::NothingToTrim(format_seconds(&t)));
        }
        let after = Clip {
            id: c.id,
            asset: c.asset,
            asset_in: c.asset_in + delta,
            asset_out: c.asset_out,
        };
        at_least_a_frame(project, &after.duration())?;
        let mut ops = vec![EditOp::Replace {
            track: ti,
            index: si,
            before: Segment::Clip(c.clone()),
            after: Segment::Clip(after),
        }];
        if let Some(g) = gap_piece(delta) {
            ops.push(EditOp::Insert { track: ti, index: si, segment: g });
        }
        return Ok(Batch {
            ops,
            desc: format!("trim in to {}", format_seconds(&t)),
        });
    }

    // Gap region: [first..=last] are Gap segments; the playhead trims the
    // region's in-edge.
    let (ti, first, last, rs, _re) = gap_region_at(project, &t)?;
    let delta = t - rs;
    if delta <= Seconds::from_integer(0) {
        return Err(EditError::NothingToTrim(format_seconds(&t)));
    }
    let mut ops = Vec::new();

    // The previous clip's tail extends into the region (if one exists).
    if first > 0 {
        let (pi, prev) = match &track.segments[first - 1] {
            Segment::Clip(c) => (first - 1, c.clone()),
            Segment::Gap(_) => unreachable!("region merges adjacent gaps"),
        };
        let mut after = prev.clone();
        after.asset_out = prev.asset_out + delta;
        let asset = project
            .asset(after.asset)
            .ok_or(EditError::ClipNotFound(after.asset))?;
        if let Some(d) = asset.duration()
            && after.asset_out > d
        {
            return Err(EditError::BeyondAsset {
                id: prev.id,
                out: format_seconds(&after.asset_out),
                duration: format_seconds(&d),
            });
        }
        ops.push(EditOp::Replace {
            track: ti,
            index: pi,
            before: Segment::Clip(prev),
            after: Segment::Clip(after),
        });
    }

    // The region loses `delta` from its front: remove fully-eaten gap
    // segments (all at index `first` — removals pull the next one in),
    // then shrink the first survivor.
    let mut remaining = delta;
    for k in first..=last {
        let g_dur = match &track.segments[k] {
            Segment::Gap(g) => g.duration,
            Segment::Clip(_) => unreachable!("region is gaps"),
        };
        if g_dur > remaining {
            // this gap partially donates: shrink it, and we're done
            ops.push(EditOp::Replace {
                track: ti,
                index: first,
                before: Segment::Gap(Gap { duration: g_dur }),
                after: Segment::Gap(Gap { duration: g_dur - remaining }),
            });
            break;
        }
        // fully donated: removed — every removal lands at index `first`
        // because the list shifts down after each one
        ops.push(EditOp::Remove {
            track: ti,
            index: first,
            segment: Segment::Gap(Gap { duration: g_dur }),
        });
        remaining -= g_dur;
    }
    Ok(Batch {
        ops,
        desc: format!("trim gap in to {}", format_seconds(&t)),
    })
}

/// `o` — trim the out-edge of whatever is under the playhead:
/// - clip: its tail is cut at the playhead and a gap of the trimmed
///   material takes its place (everything else stays put);
/// - gap: the next clip's head extends back into it (material
///   permitting); a trailing gap shortens to the playhead.
pub fn trim_out(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let si = track.index_at(t).ok_or(EditError::NoSegment)?;

    if let Segment::Clip(c) = &track.segments[si] {
        let seg_start = track.start_of(si);
        let new_len = t - seg_start;
        if new_len <= Seconds::from_integer(0) {
            return Err(EditError::NothingToTrim(format_seconds(&t)));
        }
        let after = Clip {
            id: c.id,
            asset: c.asset,
            asset_in: c.asset_in,
            asset_out: c.asset_in + new_len,
        };
        at_least_a_frame(project, &after.duration())?;
        let mut ops = vec![EditOp::Replace {
            track: ti,
            index: si,
            before: Segment::Clip(c.clone()),
            after: Segment::Clip(after),
        }];
        if let Some(g) = gap_piece(seg_start + c.duration() - t) {
            ops.push(EditOp::Insert { track: ti, index: si + 1, segment: g });
        }
        return Ok(Batch {
            ops,
            desc: format!("trim out to {}", format_seconds(&t)),
        });
    }

    // Gap region: the playhead trims the region's out-edge.
    let (ti, first, last, _rs, re) = gap_region_at(project, &t)?;
    let delta = re - t;
    if delta <= Seconds::from_integer(0) {
        return Err(EditError::NothingToTrim(format_seconds(&t)));
    }
    let mut ops = Vec::new();

    // The next clip's head extends back into the region (if one exists).
    if last + 1 < track.segments.len() {
        let (ni, next) = match &track.segments[last + 1] {
            Segment::Clip(c) => (last + 1, c.clone()),
            Segment::Gap(_) => unreachable!("region merges adjacent gaps"),
        };
        let mut after = next.clone();
        after.asset_in = next.asset_in - delta;
        if after.asset_in < Seconds::from_integer(0) {
            return Err(EditError::BeforeAsset {
                id: next.id,
                needed: format_seconds(&delta),
                ain: format_seconds(&next.asset_in),
            });
        }
        ops.push(EditOp::Replace {
            track: ti,
            index: ni,
            before: Segment::Clip(next),
            after: Segment::Clip(after),
        });
    }

    // The region loses `delta` from its back: remove fully-eaten gap
    // segments walking backwards, then shrink the first survivor.
    let remaining = delta;
    for k in (first..=last).rev() {
        let g = match &track.segments[k] {
            Segment::Gap(g) => g.clone(),
            Segment::Clip(_) => unreachable!("region is gaps"),
        };
        if g.duration > remaining {
            ops.push(EditOp::Replace {
                track: ti,
                index: k,
                before: Segment::Gap(g.clone()),
                after: Segment::Gap(Gap { duration: g.duration - remaining }),
            });
            break;
        }
        ops.push(EditOp::Remove {
            track: ti,
            index: k,
            segment: Segment::Gap(g),
        });
    }
    Ok(Batch {
        ops,
        desc: format!("trim gap out to {}", format_seconds(&t)),
    })
}

/// `I` — ripple trim-in: the clip's head is cut at the playhead and the
/// followers slide left (no gap is left behind). On a gap, identical to
/// `i` (gap trims are inherently ripple).
pub fn trim_in_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    match trim_in(project, playhead) {
        // strip the gap-insert op from the clip case to make it ripple
        Ok(mut batch) => {
            batch.ops.retain(|op| !matches!(op, EditOp::Insert { segment: Segment::Gap(_), .. }));
            Ok(batch)
        }
        Err(e) => Err(e),
    }
}

/// `O` — ripple trim-out: the clip's tail is cut at the playhead and the
/// followers slide left (no gap left behind). On a gap, identical to `o`.
pub fn trim_out_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    match trim_out(project, playhead) {
        Ok(mut batch) => {
            batch
                .ops
                .retain(|op| !matches!(op, EditOp::Insert { segment: Segment::Gap(_), .. }));
            Ok(batch)
        }
        Err(e) => Err(e),
    }
}

/// `s` — split the clip under the playhead into two clips.
pub fn split(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let (ti, si, c) = clip_at(project, &t)?;
    let seg_start = project.timeline.tracks[ti].start_of(si);
    let left_len = t - seg_start;
    if left_len <= Seconds::from_integer(0) || left_len >= c.duration() {
        return Err(EditError::NothingToTrim(format_seconds(&t)));
    }
    at_least_a_frame(project, &left_len)?;
    at_least_a_frame(project, &(c.duration() - left_len))?;

    let left = Segment::Clip(Clip {
        id: c.id,
        asset: c.asset,
        asset_in: c.asset_in,
        asset_out: c.asset_in + left_len,
    });
    let right = Segment::Clip(Clip {
        id: project.next_clip_id(),
        asset: c.asset,
        asset_in: c.asset_in + (t - seg_start),
        asset_out: c.asset_out,
    });
    Ok(Batch {
        ops: replace_with_many(ti, si, &Segment::Clip(c.clone()), vec![left, right]),
        desc: format!("split clip #{} at {}", c.id, format_seconds(&t)),
    })
}

/// `dd` — delete what's under the playhead, leaving a gap where a clip
/// was (the timeline end is preserved); a gap under the playhead is
/// removed (shrinking the timeline).
pub fn delete_under(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let (ti, si) = {
        let ti = video_track_index(project)?;
        let si = project.timeline.tracks[ti]
            .index_at(t)
            .ok_or(EditError::NoSegment)?;
        (ti, si)
    };
    let seg = project.timeline.tracks[ti].segments[si].clone();
    match seg {
        Segment::Clip(c) => {
            let dur = c.duration();
            Ok(Batch {
                ops: replace_with_many(
                    ti,
                    si,
                    &Segment::Clip(c),
                    vec![Segment::Gap(Gap { duration: dur })],
                ),
                desc: "delete clip (gap left behind)".to_string(),
            })
        }
        Segment::Gap(g) => Ok(Batch {
            ops: vec![EditOp::Remove {
                track: ti,
                index: si,
                segment: Segment::Gap(g),
            }],
            desc: "delete gap".to_string(),
        }),
    }
}

/// `D` — ripple delete: remove whatever segment is under the playhead.
/// Followers slide left; the timeline shrinks by its duration.
pub fn delete_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let (ti, si) = {
        let ti = video_track_index(project)?;
        let si = project.timeline.tracks[ti]
            .index_at(t)
            .ok_or(EditError::NoSegment)?;
        (ti, si)
    };
    let seg = project.timeline.tracks[ti].segments[si].clone();
    Ok(Batch {
        ops: vec![EditOp::Remove {
            track: ti,
            index: si,
            segment: seg.clone(),
        }],
        desc: match seg {
            Segment::Clip(_) => "ripple delete".to_string(),
            Segment::Gap(_) => "delete gap (ripple)".to_string(),
        },
    })
}

/// `p` — gentle paste: place the clipboard at the playhead only when it
/// fits inside the gap there (the gap donates its space; nothing moves).
pub fn paste(project: &Project, snapshot: &Clip, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let dur = snapshot.duration();
    let seg = track.segments[si].clone();
    let g = match seg {
        Segment::Gap(g) => g,
        Segment::Clip(_) => return Err(EditError::NoRoom),
    };
    let left_len = t - track.start_of(si);
    let right_len = g.duration - left_len - dur;
    if right_len < Seconds::from_integer(0) {
        return Err(EditError::NoRoom);
    }

    let pasted = Segment::Clip(Clip {
        id: project.next_clip_id(),
        asset: snapshot.asset,
        asset_in: snapshot.asset_in,
        asset_out: snapshot.asset_out,
    });
    let mut pieces: Vec<Segment> = Vec::new();
    if let Some(l) = gap_piece(left_len) {
        pieces.push(l);
    }
    pieces.push(pasted);
    if let Some(r) = gap_piece(right_len) {
        pieces.push(r);
    }
    Ok(Batch {
        ops: replace_with_many(ti, si, &Segment::Gap(g), pieces),
        desc: format!("paste at {}", format_seconds(&t)),
    })
}

/// `P` — ripple paste: insert the clipboard at the playhead, splitting
/// whatever is there (clip or gap) and growing the timeline by the
/// clip's duration. At a cut the insert lands before the clip starting
/// there.
pub fn paste_ripple(project: &Project, snapshot: &Clip, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let _dur = snapshot.duration();

    // Appending: the playhead sits exactly at the track end.
    if track.end() == t {
        return Ok(Batch {
            ops: vec![EditOp::Insert {
                track: ti,
                index: track.segments.len(),
                segment: Segment::Clip(Clip {
                    id: project.next_clip_id(),
                    asset: snapshot.asset,
                    asset_in: snapshot.asset_in,
                    asset_out: snapshot.asset_out,
                }),
            }],
            desc: format!("append at {}", format_seconds(&t)),
        });
    }
    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let seg_start = track.start_of(si);

    // The containing segment splits at t; the paste rides between the
    // pieces. Inserting shifts every follower automatically.
    let pieces: Vec<Segment> = match &seg {
        Segment::Clip(c) => {
            let head_len = t - seg_start;
            let at_head_edge = head_len <= Seconds::from_integer(0);
            let mut pieces = Vec::new();
            if !at_head_edge {
                pieces.push(Segment::Clip(Clip {
                    id: c.id,
                    asset: c.asset,
                    asset_in: c.asset_in,
                    asset_out: c.asset_in + head_len,
                }));
            }
            pieces.push(Segment::Clip(Clip {
                id: project.next_clip_id(),
                asset: snapshot.asset,
                asset_in: snapshot.asset_in,
                asset_out: snapshot.asset_out,
            }));
            pieces.push(Segment::Clip(Clip {
                id: project.next_clip_id() + 1,
                asset: c.asset,
                asset_in: c.asset_in + head_len,
                asset_out: c.asset_out,
            }));
            pieces
        }
        Segment::Gap(g) => {
            let mut pieces = Vec::new();
            if let Some(l) = gap_piece(t - seg_start) {
                pieces.push(l);
            }
            pieces.push(Segment::Clip(Clip {
                id: project.next_clip_id(),
                asset: snapshot.asset,
                asset_in: snapshot.asset_in,
                asset_out: snapshot.asset_out,
            }));
            if let Some(r) = gap_piece(g.duration - (t - seg_start)) {
                pieces.push(r);
            }
            pieces
        }
    };
    let desc = format!(
        "ripple paste at {} — clip #{} inserted",
        format_seconds(&t),
        project.next_clip_id()
    );
    Ok(Batch {
        ops: replace_with_many(ti, si, &seg, pieces),
        desc,
    })
}

/// CLI `remove <id>`: replace the clip with a gap (dd semantics).
pub fn delete_clip_by_id(project: &Project, clip_id: u32) -> Result<Batch, EditError> {
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let si = track
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let seg = track.segments[si].clone();
    match seg {
        Segment::Clip(c) => {
            let dur = c.duration();
            Ok(Batch {
                ops: replace_with_many(
                    ti,
                    si,
                    &Segment::Clip(c),
                    vec![Segment::Gap(Gap { duration: dur })],
                ),
                desc: format!("delete clip #{clip_id} (gap left behind)"),
            })
        }
        Segment::Gap(_) => Err(EditError::ClipNotFound(clip_id)),
    }
}
