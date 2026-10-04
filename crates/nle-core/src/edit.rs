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
use crate::timeline::{Clip, Gap, Segment, TimelineError, Track, TrackKind};

/// Deepest undo stack we keep (changes).
pub const MAX_HISTORY: usize = 100;
/// Sidecar file format version. No migration path is kept (see
/// README "Compatibility: none, by design"): old sidecars start fresh.
pub const HISTORY_VERSION: u32 = 3;

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

/// One reversible change: a segment inserted / removed / replaced at an
/// index of a track's segment list.
///
/// NOTE: deliberately NOT serializable. The persisted history form is
/// `Change` (frame topologies); op shapes are runtime-only so refactors
/// here cannot break old sidecars.
#[derive(Debug, Clone, PartialEq)]
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
/// Runtime-only (see `EditOp`): the persisted form is `Change`.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub ops: Vec<EditOp>,
    /// Human description, shown in the TUI message line.
    pub desc: String,
}

/// Undo/redo stack of frame-topology changes. `apply` is the only way
/// in: it snapshots the affected track as frames before running the
/// batch, then stores the before/after pair as a `Change`. Undo and redo
/// verify the live track against the recorded topology (by segment id,
/// never by list index) and substitute the other side.
#[derive(Debug, Default, Clone)]
pub struct History {
    done: Vec<Change>,
    undone: Vec<Change>,
}

impl History {
    pub fn apply(&mut self, project: &mut Project, batch: Batch) -> Result<(), EditError> {
        if batch.ops.is_empty() {
            project.validate()?;
            return Ok(());
        }
        let track = batch_track(&batch, project)?;
        let fps = history_fps(project);
        let before = snapshot_track(&project.timeline.tracks[track], fps);
        let saved = project.timeline.tracks[track].segments.clone();
        if let Err(e) = apply_batch(project, &batch.ops).and_then(|_| project.validate().map_err(EditError::from)) {
            project.timeline.tracks[track].segments = saved;
            return Err(e);
        }
        let after = snapshot_track(&project.timeline.tracks[track], fps);
        self.done.push(Change {
            track,
            start_frame: 0,
            before,
            after,
            desc: batch.desc,
        });
        self.undone.clear();
        if self.done.len() > MAX_HISTORY {
            let excess = self.done.len() - MAX_HISTORY;
            self.done.drain(0..excess);
        }
        Ok(())
    }

    pub fn undo(&mut self, project: &mut Project) -> Result<String, EditError> {
        let change = self.done.last().ok_or(EditError::NothingToUndo)?.clone();
        let saved = project
            .timeline
            .tracks
            .get(change.track)
            .map(|t| t.segments.clone());
        let desc = change.desc.clone();
        if let Err(e) =
            substitute_topology(project, &change, false).and_then(|_| project.validate().map_err(EditError::from))
        {
            if let Some(segments) = saved
                && let Some(track) = project.timeline.tracks.get_mut(change.track) {
                    track.segments = segments;
                }
            return Err(e);
        }
        self.done.pop();
        self.undone.push(change);
        Ok(desc)
    }

    pub fn redo(&mut self, project: &mut Project) -> Result<String, EditError> {
        let change = self.undone.last().ok_or(EditError::NothingToRedo)?.clone();
        let saved = project
            .timeline
            .tracks
            .get(change.track)
            .map(|t| t.segments.clone());
        let desc = change.desc.clone();
        if let Err(e) =
            substitute_topology(project, &change, true).and_then(|_| project.validate().map_err(EditError::from))
        {
            if let Some(segments) = saved
                && let Some(track) = project.timeline.tracks.get_mut(change.track) {
                    track.segments = segments;
                }
            return Err(e);
        }
        self.undone.pop();
        self.done.push(change);
        Ok(desc)
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

    /// Serializable snapshot of the undo stack (persisted in a sidecar
    /// file). Only `done` is stored: redo history dies across restarts
    /// and is rebuilt live in-session.
    pub fn state(&self) -> HistoryState {
        HistoryState {
            version: HISTORY_VERSION,
            done: self.done.clone(),
        }
    }

    /// Rebuild a History from a snapshot, honoring the depth cap.
    /// The redo stack always starts empty.
    pub fn restore(state: HistoryState) -> Self {
        if state.version != HISTORY_VERSION {
            return Self::default();
        }
        let mut done = state.done;
        if done.len() > MAX_HISTORY {
            let excess = done.len() - MAX_HISTORY;
            done.drain(0..excess);
        }
        Self { done, undone: Vec::new() }
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

/// Serializable snapshot of the undo stack (the sidecar file).
/// Only the `done` stack is stored; `undone` (redo) is session-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryState {
    pub version: u32,
    pub done: Vec<Change>,
}

/// One timeline segment as frame counts. This is the persisted history
/// unit: positions are frames (robust to fps reinterpretation), clips
/// are located by segment id (robust to index shifts), and the shape
/// mirrors the timeline data model rather than any op implementation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Atom {
    Clip { id: u32, asset: u32, in_frame: i64, out_frame: i64 },
    Gap { frames: i64 },
}

/// One undoable user action as topologies: the affected track's
/// segments before and after, in frames. Undo substitutes `before` for
/// `after`; redo does the reverse. `start_frame` is reserved (whole-track
/// snapshots start at 0) for future region-scoped entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub track: usize,
    pub start_frame: i64,
    pub before: Vec<Atom>,
    pub after: Vec<Atom>,
    pub desc: String,
}

fn history_fps(project: &Project) -> Seconds {
    // Mirrors `snap`'s fallback: without a known fps the grid is whole
    // seconds, so frame counts are second counts. Production always sets
    // fps before the first edit; this only fires for empty projects.
    project.fps.unwrap_or_else(|| Seconds::from_integer(1))
}

fn seconds_to_frames(s: Seconds, fps: Seconds) -> i64 {
    (s * fps).round().to_integer()
}

fn frames_to_seconds(frames: i64, fps: Seconds) -> Seconds {
    Seconds::new(frames, 1) / fps
}

fn atom_from_segment(seg: &Segment, fps: Seconds) -> Atom {
    match seg {
        Segment::Clip(c) => Atom::Clip {
            id: c.id,
            asset: c.asset,
            in_frame: seconds_to_frames(c.asset_in, fps),
            out_frame: seconds_to_frames(c.asset_out, fps),
        },
        Segment::Gap(g) => Atom::Gap {
            frames: seconds_to_frames(g.duration, fps),
        },
    }
}

fn segment_from_atom(atom: &Atom, fps: Seconds) -> Segment {
    match atom {
        Atom::Clip { id, asset, in_frame, out_frame } => Segment::Clip(Clip {
            id: *id,
            asset: *asset,
            asset_in: frames_to_seconds(*in_frame, fps),
            asset_out: frames_to_seconds(*out_frame, fps),
        }),
        Atom::Gap { frames } => Segment::Gap(Gap {
            duration: frames_to_seconds(*frames, fps),
        }),
    }
}

fn snapshot_track(track: &Track, fps: Seconds) -> Vec<Atom> {
    track.segments.iter().map(|s| atom_from_segment(s, fps)).collect()
}

/// Which track a batch touches (all current batches are single-track).
fn batch_track(batch: &Batch, project: &Project) -> Result<usize, EditError> {
    match batch.ops.first() {
        Some(EditOp::Insert { track, .. })
        | Some(EditOp::Remove { track, .. })
        | Some(EditOp::Replace { track, .. }) => Ok(*track),
        None => video_track_index(project),
    }
}

/// Verify the live track matches one side of `change` (by segment id and
/// frame counts) and substitute the other. `forward == false` is undo
/// (expect `after`, write `before`); `true` is redo.
fn substitute_topology(project: &mut Project, change: &Change, forward: bool) -> Result<(), EditError> {
    let fps = history_fps(project);
    let track = project
        .timeline
        .tracks
        .get_mut(change.track)
        .ok_or(EditError::TrackNotFound(change.track))?;
    let (expected, write) = if forward {
        (&change.before, &change.after)
    } else {
        (&change.after, &change.before)
    };
    let live = snapshot_track(track, fps);
    if live != *expected {
        return Err(EditError::Timeline(TimelineError::Corrupt(
            "timeline changed outside the undo stack — history entry does not apply".to_string(),
        )));
    }
    track.segments = write.iter().map(|a| segment_from_atom(a, fps)).collect();
    Ok(())
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

/// Merge two newly-adjacent segments into a seamless whole, if they
/// form one: gaps always merge; clips merge iff same asset + contiguous
/// source (prev.asset_out == next.asset_in, exact). The merged clip keeps
/// the earlier segment's id (mirrors split, which keeps the original id
/// on the left half).
fn merge_pair(left: &Segment, right: &Segment) -> Option<Segment> {
    match (left, right) {
        (Segment::Gap(a), Segment::Gap(b)) => Some(Segment::Gap(Gap {
            duration: a.duration + b.duration,
        })),
        (Segment::Clip(a), Segment::Clip(b))
            if a.asset == b.asset && a.asset_out == b.asset_in =>
        {
            Some(Segment::Clip(Clip {
                id: a.id,
                asset: a.asset,
                asset_in: a.asset_in,
                asset_out: b.asset_out,
            }))
        }
        _ => None,
    }
}

/// Human note for a merge, appended to the batch description.
fn merge_note(left: &Segment, right: &Segment) -> String {
    match (left, right) {
        (Segment::Clip(a), Segment::Clip(b)) => format!("merged clips #{}+#{}", a.id, b.id),
        _ => "merged gaps".to_string(),
    }
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
        // The trimmed head becomes gap: grow the neighboring gap when
        // there is one instead of fragmenting the track with an adjacent
        // second gap.
        if si > 0
            && let Segment::Gap(g) = &track.segments[si - 1] {
                ops.push(EditOp::Replace {
                    track: ti,
                    index: si - 1,
                    before: Segment::Gap(g.clone()),
                    after: Segment::Gap(Gap { duration: g.duration + delta }),
                });
                return Ok(Batch {
                    ops,
                    desc: format!("trim in to {}", format_seconds(&t)),
                });
            }
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
        let trimmed = seg_start + c.duration() - t;
        let mut ops = vec![EditOp::Replace {
            track: ti,
            index: si,
            before: Segment::Clip(c.clone()),
            after: Segment::Clip(after),
        }];
        // The trimmed tail becomes gap: grow the neighboring gap when
        // there is one instead of fragmenting the track.
        if si + 1 < track.segments.len()
            && let Segment::Gap(g) = &track.segments[si + 1] {
                ops.push(EditOp::Replace {
                    track: ti,
                    index: si + 1,
                    before: Segment::Gap(g.clone()),
                    after: Segment::Gap(Gap { duration: g.duration + trimmed }),
                });
                return Ok(Batch {
                    ops,
                    desc: format!("trim out to {}", format_seconds(&t)),
                });
            }
        if let Some(g) = gap_piece(trimmed) {
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
    // Ripple strips the leftover-gap ops, but only when the playhead is
    // on a clip (gap-region trims are inherently ripple and keep theirs).
    let on_clip = clip_at(project, &snap(project, playhead)?).is_ok();
    match trim_in(project, playhead) {
        Ok(mut batch) => {
            if on_clip {
                batch.ops.retain(|op| !matches!(op,
                    EditOp::Insert { segment: Segment::Gap(_), .. }
                    | EditOp::Replace { before: Segment::Gap(_), .. }));
            }
            Ok(batch)
        }
        Err(e) => Err(e),
    }
}

/// `O` — ripple trim-out: the clip's tail is cut at the playhead and the
/// followers slide left (no gap left behind). On a gap, identical to `o`.
pub fn trim_out_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let on_clip = clip_at(project, &snap(project, playhead)?).is_ok();
    match trim_out(project, playhead) {
        Ok(mut batch) => {
            if on_clip {
                batch
                    .ops
                    .retain(|op| !matches!(op,
                        EditOp::Insert { segment: Segment::Gap(_), .. }
                        | EditOp::Replace { before: Segment::Gap(_), .. }));
            }
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
            let mut ops = replace_with_many(
                ti,
                si,
                &Segment::Clip(c),
                vec![Segment::Gap(Gap { duration: dur })],
            );
            let mut desc = "delete clip (gap left behind)".to_string();
            // Fuse the new gap with neighboring gaps so deletes never
            // fragment the track into adjacent gaps.
            let segs = &project.timeline.tracks[ti].segments;
            let left_gap = si > 0 && matches!(segs[si - 1], Segment::Gap(_));
            let right_gap = si + 1 < segs.len() && matches!(segs[si + 1], Segment::Gap(_));
            let new_gap = Segment::Gap(Gap { duration: dur });
            match (left_gap, right_gap) {
                (true, true) => {
                    let total = segs[si - 1].duration() + new_gap.duration() + segs[si + 1].duration();
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si - 1,
                        before: segs[si - 1].clone(),
                        after: Segment::Gap(Gap { duration: total }),
                    });
                    ops.push(EditOp::Remove { track: ti, index: si, segment: new_gap });
                    ops.push(EditOp::Remove { track: ti, index: si, segment: segs[si + 1].clone() });
                    desc += " (merged gaps)";
                }
                (true, false) => {
                    let total = segs[si - 1].duration() + new_gap.duration();
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si - 1,
                        before: segs[si - 1].clone(),
                        after: Segment::Gap(Gap { duration: total }),
                    });
                    ops.push(EditOp::Remove { track: ti, index: si, segment: new_gap });
                    desc += " (merged gaps)";
                }
                (false, true) => {
                    let total = new_gap.duration() + segs[si + 1].duration();
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si,
                        before: new_gap,
                        after: Segment::Gap(Gap { duration: total }),
                    });
                    ops.push(EditOp::Remove { track: ti, index: si + 1, segment: segs[si + 1].clone() });
                    desc += " (merged gaps)";
                }
                (false, false) => {}
            }
            Ok(Batch { ops, desc })
        }
        Segment::Gap(g) => {
            let mut ops = vec![EditOp::Remove {
                track: ti,
                index: si,
                segment: Segment::Gap(g),
            }];
            let mut desc = "delete gap".to_string();
            // The removal joins si-1 and si+1: merge them when seamless.
            let segs = &project.timeline.tracks[ti].segments;
            if si > 0 && si + 1 < segs.len()
                && let Some(merged) = merge_pair(&segs[si - 1], &segs[si + 1]) {
                    desc += &format!(" ({})", merge_note(&segs[si - 1], &segs[si + 1]));
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si - 1,
                        before: segs[si - 1].clone(),
                        after: merged,
                    });
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si,
                        segment: segs[si + 1].clone(),
                    });
                }
            Ok(Batch { ops, desc })
        }
    }
}

/// `D` — ripple delete: remove whatever segment is under the playhead.
/// Followers slide left; the timeline shrinks by its duration.
pub fn delete_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let si = project.timeline.tracks[ti]
        .index_at(t)
        .ok_or(EditError::NoSegment)?;
    delete_segment_ripple(project, ti, si)
}

/// Ripple-delete clip `clip_id` by id. Extend mode commits at a consumed
/// edge against the armed clip, not whatever the playhead's boundary
/// resolves to.
pub fn delete_clip_ripple_by_id(project: &Project, clip_id: u32) -> Result<Batch, EditError> {
    let ti = video_track_index(project)?;
    let si = project.timeline.tracks[ti]
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    delete_segment_ripple(project, ti, si)
}

fn delete_segment_ripple(project: &Project, ti: usize, si: usize) -> Result<Batch, EditError> {
    let seg = project.timeline.tracks[ti].segments[si].clone();
    let mut ops = vec![EditOp::Remove {
        track: ti,
        index: si,
        segment: seg.clone(),
    }];
    let mut desc = match seg {
        Segment::Clip(_) => "ripple delete".to_string(),
        Segment::Gap(_) => "delete gap (ripple)".to_string(),
    };
    // The removal joins si-1 and si+1: merge them when seamless.
    let segs = &project.timeline.tracks[ti].segments;
    if si > 0 && si + 1 < segs.len()
        && let Some(merged) = merge_pair(&segs[si - 1], &segs[si + 1]) {
            desc += &format!(" ({})", merge_note(&segs[si - 1], &segs[si + 1]));
            ops.push(EditOp::Replace {
                track: ti,
                index: si - 1,
                before: segs[si - 1].clone(),
                after: merged,
            });
            ops.push(EditOp::Remove {
                track: ti,
                index: si,
                segment: segs[si + 1].clone(),
            });
        }
    Ok(Batch { ops, desc })
}

/// `p` — replace-paste: drop the clipboard at the playhead, overwriting
/// whatever its span covers. Covered gaps vanish, wholly-covered clips
/// are removed, and a straddled follower is trimmed at its near edge;
/// past the track end the timeline simply grows. The pasted clip keeps
/// the yanked source range (splitting a clip hands the surviving right
/// half a fresh id).
pub fn paste(project: &Project, snapshot: &Clip, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let dur = snapshot.duration();
    let pasted = |id: u32| {
        Segment::Clip(Clip {
            id,
            asset: snapshot.asset,
            asset_in: snapshot.asset_in,
            asset_out: snapshot.asset_out,
        })
    };

    // At/past the end (or an empty track): nothing to overwrite — append,
    // bridging the void with a gap when there's distance to cover.
    if track.segments.is_empty() || t >= track.end() {
        let mut ops = Vec::new();
        let mut index = track.segments.len();
        let void = t - track.end();
        if void > Seconds::from_integer(0) {
            ops.push(EditOp::Insert {
                track: ti,
                index,
                segment: Segment::Gap(Gap { duration: void }),
            });
            index += 1;
        }
        ops.push(EditOp::Insert {
            track: ti,
            index,
            segment: pasted(project.next_clip_id()),
        });
        return Ok(Batch {
            ops,
            desc: format!("paste at {}", format_seconds(&t)),
        });
    }

    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let left_len = t - track.start_of(si);
    let target = t + dur;

    // How far the overwrite reaches (`remove_upto`): the straddled
    // segment containing the paste's end, or the whole tail when the
    // paste runs off the track.
    let mut notes: Vec<String> = Vec::new();
    let straddle = if target < track.end() {
        Some((track.index_at(target).ok_or(EditError::NoSegment)?, target))
    } else {
        None
    };
    let remove_upto = straddle.map(|(ei, _)| ei + 1).unwrap_or(track.segments.len());

    // Wholly-covered segments between the paste and the end boundary go
    // (the straddled segment is re-created as the right piece, so it has
    // its own note instead).
    let covered_upto = straddle.map(|(ei, _)| ei).unwrap_or(track.segments.len());
    for k in si + 1..covered_upto {
        notes.push(format!("replaced {}", segment_note(&track.segments[k])));
    }

    // What survives past the paste: the straddled segment's tail (none
    // when the paste ends exactly on a boundary).
    let right = match straddle {
        Some((ei, at)) => {
            let off = at - track.start_of(ei);
            let rem = track.segments[ei].duration() - off;
            if rem > Seconds::from_integer(0) {
                match &track.segments[ei] {
                    Segment::Gap(_) => Some(Segment::Gap(Gap { duration: rem })),
                    Segment::Clip(c) => {
                        // A split keeps the head's id on the left, so the
                        // tail needs a fresh id only when the same clip
                        // supplied both pieces.
                        let id = if ei == si && left_len > Seconds::from_integer(0) {
                            project.next_clip_id() + 1
                        } else {
                            c.id
                        };
                        if off > Seconds::from_integer(0) {
                            notes.push(format!("trimmed clip #{}", c.id));
                        }
                        Some(Segment::Clip(Clip {
                            id,
                            asset: c.asset,
                            asset_in: c.asset_in + off,
                            asset_out: c.asset_out,
                        }))
                    }
                }
            } else {
                None
            }
        }
        None => None,
    };

    // Compose the replacement for [t, t+dur): left remainder, paste,
    // right remainder.
    let mut pieces: Vec<Segment> = Vec::new();
    if left_len > Seconds::from_integer(0) {
        match &seg {
            Segment::Gap(_) => pieces.push(Segment::Gap(Gap { duration: left_len })),
            Segment::Clip(c) => pieces.push(Segment::Clip(Clip {
                asset_out: c.asset_in + left_len,
                ..c.clone()
            })),
        }
    }
    pieces.push(pasted(project.next_clip_id()));
    if let Some(r) = right {
        pieces.push(r);
    }

    // Remove wholly-covered followers first (descending, so indices above
    // `si` stay valid), then swap `si` for the piece list.
    let mut ops: Vec<EditOp> = Vec::new();
    for k in (si + 1..remove_upto).rev() {
        ops.push(EditOp::Remove {
            track: ti,
            index: k,
            segment: track.segments[k].clone(),
        });
    }
    ops.extend(replace_with_many(ti, si, &seg, pieces));

    let mut desc = format!("paste at {}", format_seconds(&t));
    if !notes.is_empty() {
        desc += &format!(" ({})", notes.join(", "));
    }
    Ok(Batch { ops, desc })
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

    // At or past the track end: the void becomes a gap segment (when
    // there's distance to cover), then the clip lands on the playhead.
    if t >= track.end() {
        let mut ops = Vec::new();
        let void = t - track.end();
        let mut index = track.segments.len();
        if void > Seconds::from_integer(0) {
            ops.push(EditOp::Insert {
                track: ti,
                index,
                segment: Segment::Gap(Gap { duration: void }),
            });
            index += 1;
        }
        ops.push(EditOp::Insert {
            track: ti,
            index,
            segment: Segment::Clip(Clip {
                id: project.next_clip_id(),
                asset: snapshot.asset,
                asset_in: snapshot.asset_in,
                asset_out: snapshot.asset_out,
            }),
        });
        return Ok(Batch {
            ops,
            desc: format!(
                "ripple paste at {} — clip #{} inserted",
                format_seconds(&t),
                project.next_clip_id()
            ),
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
            let pid = project.next_clip_id();
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
                id: pid,
                asset: snapshot.asset,
                asset_in: snapshot.asset_in,
                asset_out: snapshot.asset_out,
            }));
            if at_head_edge {
                // insert-before: the original clip keeps its identity
                pieces.push(Segment::Clip(Clip {
                    id: c.id,
                    asset: c.asset,
                    asset_in: c.asset_in + head_len,
                    asset_out: c.asset_out,
                }));
            } else {
                // split: the right half is a new clip
                pieces.push(Segment::Clip(Clip {
                    id: pid + 1,
                    asset: c.asset,
                    asset_in: c.asset_in + head_len,
                    asset_out: c.asset_out,
                }));
            }
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
            let mut ops = replace_with_many(
                ti,
                si,
                &Segment::Clip(c),
                vec![Segment::Gap(Gap { duration: dur })],
            );
            let mut desc = format!("delete clip #{clip_id} (gap left behind)");
            // Fuse the new gap with neighboring gaps (dd semantics).
            let segs = &project.timeline.tracks[ti].segments;
            let left_gap = si > 0 && matches!(segs[si - 1], Segment::Gap(_));
            let right_gap = si + 1 < segs.len() && matches!(segs[si + 1], Segment::Gap(_));
            let new_gap = Segment::Gap(Gap { duration: dur });
            match (left_gap, right_gap) {
                (true, true) => {
                    let total = segs[si - 1].duration() + new_gap.duration() + segs[si + 1].duration();
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si - 1,
                        before: segs[si - 1].clone(),
                        after: Segment::Gap(Gap { duration: total }),
                    });
                    ops.push(EditOp::Remove { track: ti, index: si, segment: new_gap });
                    ops.push(EditOp::Remove { track: ti, index: si, segment: segs[si + 1].clone() });
                    desc += " (merged gaps)";
                }
                (true, false) => {
                    let total = segs[si - 1].duration() + new_gap.duration();
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si - 1,
                        before: segs[si - 1].clone(),
                        after: Segment::Gap(Gap { duration: total }),
                    });
                    ops.push(EditOp::Remove { track: ti, index: si, segment: new_gap });
                    desc += " (merged gaps)";
                }
                (false, true) => {
                    let total = new_gap.duration() + segs[si + 1].duration();
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si,
                        before: new_gap,
                        after: Segment::Gap(Gap { duration: total }),
                    });
                    ops.push(EditOp::Remove { track: ti, index: si + 1, segment: segs[si + 1].clone() });
                    desc += " (merged gaps)";
                }
                (false, false) => {}
            }
            Ok(Batch { ops, desc })
        }
        Segment::Gap(_) => Err(EditError::ClipNotFound(clip_id)),
    }
}

/// Extend a clip edge outward to `t` without touching neighbors —
/// followers reseat structurally, so the timeline grows by the
/// extension. The extension consumes source material: the head reaches
/// earlier into the asset, the tail further past its out-point.
/// Material-gated (`BeforeAsset` / `BeyondAsset`); `t` must lie strictly
/// outward of the edge.
pub fn extend_edge(project: &Project, clip_id: u32, head: bool, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let si = track
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let c = match &track.segments[si] {
        Segment::Clip(c) => c.clone(),
        Segment::Gap(_) => return Err(EditError::ClipNotFound(clip_id)),
    };
    let edge = track.start_of(si) + if head { Seconds::from_integer(0) } else { c.duration() };
    let after = if head {
        if t >= edge {
            return Err(EditError::NothingToTrim(format_seconds(&t)));
        }
        let delta = edge - t;
        let asset_in = c.asset_in - delta;
        if asset_in < Seconds::from_integer(0) {
            return Err(EditError::BeforeAsset {
                id: c.id,
                needed: format_seconds(&delta),
                ain: format_seconds(&c.asset_in),
            });
        }
        Clip { asset_in, ..c }
    } else {
        if t <= edge {
            return Err(EditError::NothingToTrim(format_seconds(&t)));
        }
        let asset_out = c.asset_out + (t - edge);
        let asset = project
            .asset(c.asset)
            .ok_or(EditError::ClipNotFound(c.asset))?;
        if let Some(d) = asset.duration()
            && asset_out > d
        {
            return Err(EditError::BeyondAsset {
                id: c.id,
                out: format_seconds(&asset_out),
                duration: format_seconds(&d),
            });
        }
        Clip { asset_out, ..c }
    };
    let side = if head { "head" } else { "tail" };
    Ok(Batch {
        ops: vec![EditOp::Replace {
            track: ti,
            index: si,
            before: Segment::Clip(c),
            after: Segment::Clip(after),
        }],
        desc: format!("extend {side} of clip #{clip_id} to {}", format_seconds(&t)),
    })
}

/// Lengthen the trailing gap to reach `t` (free — gaps have no source
/// material). Used where the timeline itself grows into empty space.
pub fn extend_gap_end(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let last = track.segments.len().checked_sub(1).ok_or(EditError::NoSegment)?;
    let g = match &track.segments[last] {
        Segment::Gap(g) => g.clone(),
        Segment::Clip(_) => return Err(EditError::NoGap),
    };
    let gap_start = track.start_of(last);
    if t <= gap_start {
        return Err(EditError::NothingToTrim(format_seconds(&t)));
    }
    Ok(Batch {
        ops: vec![EditOp::Replace {
            track: ti,
            index: last,
            before: Segment::Gap(g),
            after: Segment::Gap(Gap { duration: t - gap_start }),
        }],
        desc: format!("extend gap to {}", format_seconds(&t)),
    })
}

/// Move a gap's tail (right edge) to `playhead`, for ANY gap — not just
/// the trailing one.
///
/// Shrinking always just reduces the gap (followers follow the prefix
/// sum). Growing splits by `ripple`: `E` (`ripple`) inserts empty space
/// and shifts the following segments right; `e` (replace) overwrites the
/// covered span — gaps merge away and clips are trimmed or removed, the
/// timeline length staying put. Gaps have no source material, so there is
/// no upper bound.
pub fn extend_gap_tail(
    project: &Project,
    index: usize,
    playhead: f64,
    ripple: bool,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let g = match track.segments.get(index) {
        Some(Segment::Gap(g)) => g.clone(),
        _ => return Err(EditError::NoGap),
    };
    let gap_start = track.start_of(index);
    if t <= gap_start {
        return Err(EditError::NothingToTrim(format_seconds(&t)));
    }
    let gap_end = gap_start + g.duration;

    // Shrinking, or ripple-growth: just resize the gap. Followers (prefix
    // sum) shift on their own.
    if t <= gap_end || ripple {
        return Ok(Batch {
            ops: vec![EditOp::Replace {
                track: ti,
                index,
                before: Segment::Gap(g),
                after: Segment::Gap(Gap { duration: t - gap_start }),
            }],
            desc: format!("extend gap to {}", format_seconds(&t)),
        });
    }

    // Replace-growth: the gap overwrites everything up to `t`. Split the
    // duration, then remove wholly-covered followers (descending) and
    // reconcile a straddled one — the same shape as absorb_outward, but
    // the grower is a gap with no material bound.
    let mut ops = vec![EditOp::Replace {
        track: ti,
        index,
        before: Segment::Gap(g),
        after: Segment::Gap(Gap { duration: t - gap_start }),
    }];
    let mut absorbed: Vec<String> = Vec::new();
    let mut covered: Vec<usize> = Vec::new();
    let mut straddler: Option<usize> = None;
    let mut acc = gap_end;
    for k in index + 1..track.segments.len() {
        let dur = track.segments[k].duration();
        if acc + dur <= t {
            covered.push(k);
        } else if acc < t && t < acc + dur {
            straddler = Some(k);
            break;
        } else {
            break;
        }
        acc += dur;
    }
    for k in covered.iter().rev() {
        absorbed.push(segment_note(&track.segments[*k]));
        ops.push(EditOp::Remove {
            track: ti,
            index: *k,
            segment: track.segments[*k].clone(),
        });
    }
    if let Some(m) = straddler {
        // All covered followers sat between `index` and `m`, so it lands
        // right after the gap.
        let seg = track.segments[m].clone();
        trim_or_absorb_head(
            project,
            ti,
            Straddle { index: index + 1, start: track.start_of(m), seg },
            t,
            &mut ops,
            &mut absorbed,
        )?;
    }
    let mut desc = format!("extend gap to {}", format_seconds(&t));
    if !absorbed.is_empty() {
        desc += &format!(" (absorbed {})", absorbed.join(", "));
    }
    Ok(Batch { ops, desc })
}

/// Move a gap's HEAD (left edge) leftward to `playhead`, growing it over
/// the preceding segment(s) — the mirror of `extend_gap_tail`. Only
/// leftward growth is meaningful: a gap's start is the prefix sum of what
/// precedes it, so moving it left can only be done by shrinking/removing
/// predecessors (there is nothing to ripple — the prefix is anchored at
/// 0). The gap overwrites them: covered gaps merge, a straddled clip is
/// trimmed at its tail.
pub fn extend_gap_head(project: &Project, index: usize, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let g = match track.segments.get(index) {
        Some(Segment::Gap(g)) => g.clone(),
        _ => return Err(EditError::NoGap),
    };
    let gap_start = track.start_of(index);
    let gap_end = gap_start + g.duration;
    if t >= gap_start {
        return Err(EditError::NothingToTrim(format_seconds(&t)));
    }
    let mut ops = vec![EditOp::Replace {
        track: ti,
        index,
        before: Segment::Gap(g),
        after: Segment::Gap(Gap { duration: gap_end - t }),
    }];
    let mut absorbed: Vec<String> = Vec::new();
    let mut covered: Vec<usize> = Vec::new();
    let mut straddler: Option<usize> = None;
    let mut acc = gap_start;
    for k in (0..index).rev() {
        let dur = track.segments[k].duration();
        let ks = acc - dur;
        if ks >= t {
            covered.push(k);
        } else if ks < t && t < acc {
            straddler = Some(k);
            break;
        } else {
            break;
        }
        acc = ks;
    }
    // `covered` is already descending, so removals keep lower indices valid.
    for k in &covered {
        absorbed.push(segment_note(&track.segments[*k]));
        ops.push(EditOp::Remove {
            track: ti,
            index: *k,
            segment: track.segments[*k].clone(),
        });
    }
    if let Some(m) = straddler {
        let seg = track.segments[m].clone();
        trim_or_absorb_tail(
            project,
            ti,
            Straddle { index: m, start: track.start_of(m), seg },
            t,
            &mut ops,
            &mut absorbed,
        )?;
    }
    let mut desc = format!("extend gap head to {}", format_seconds(&t));
    if !absorbed.is_empty() {
        desc += &format!(" (absorbed {})", absorbed.join(", "));
    }
    Ok(Batch { ops, desc })
}

/// Extend a clip edge outward past neighboring segments (`e`): everything
/// the span covers is reconciled destructively — gaps and wholly-covered
/// clips are removed, a straddled follower is trimmed at its near edge
/// (absorbed whole when under a frame remains). One batch, one undo.
/// (`E` never reaches here: it pushes everything aside via `extend_edge`.)
pub fn absorb_outward(project: &Project, clip_id: u32, head: bool, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = video_track_index(project)?;
    let track = &project.timeline.tracks[ti];
    let si = track
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let c = match &track.segments[si] {
        Segment::Clip(c) => c.clone(),
        Segment::Gap(_) => return Err(EditError::ClipNotFound(clip_id)),
    };
    let start = track.start_of(si);
    let edge = start + if head { Seconds::from_integer(0) } else { c.duration() };

    // Extend the armed edge first (indices below stay valid throughout).
    let after = if head {
        if t >= edge {
            return Err(EditError::NothingToTrim(format_seconds(&t)));
        }
        let delta = edge - t;
        let asset_in = c.asset_in - delta;
        if asset_in < Seconds::from_integer(0) {
            return Err(EditError::BeforeAsset {
                id: c.id,
                needed: format_seconds(&delta),
                ain: format_seconds(&c.asset_in),
            });
        }
        Clip { asset_in, ..c.clone() }
    } else {
        if t <= edge {
            return Err(EditError::NothingToTrim(format_seconds(&t)));
        }
        let asset_out = c.asset_out + (t - edge);
        let asset = project
            .asset(c.asset)
            .ok_or(EditError::ClipNotFound(c.asset))?;
        if let Some(d) = asset.duration()
            && asset_out > d
        {
            return Err(EditError::BeyondAsset {
                id: c.id,
                out: format_seconds(&asset_out),
                duration: format_seconds(&d),
            });
        }
        Clip { asset_out, ..c.clone() }
    };
    let mut ops = vec![EditOp::Replace {
        track: ti,
        index: si,
        before: Segment::Clip(c),
        after: Segment::Clip(after),
    }];
    let mut absorbed: Vec<String> = Vec::new();

    if head {
        // Covered predecessors, descending (higher indices first so
        // lower ones stay valid), then the straddled one (if any).
        let mut straddler: Option<usize> = None;
        let mut covered: Vec<usize> = Vec::new();
        let mut acc = start;
        for k in (0..si).rev() {
            let dur = track.segments[k].duration();
            let ks = acc - dur;
            if ks >= t {
                covered.push(k);
            } else if ks < t && t < acc {
                straddler = Some(k);
                break;
            } else {
                break;
            }
            acc = ks;
        }
        for k in covered {
            absorbed.push(segment_note(&track.segments[k]));
            ops.push(EditOp::Remove {
                track: ti,
                index: k,
                segment: track.segments[k].clone(),
            });
        }
        if let Some(m) = straddler {
            let seg = track.segments[m].clone();
            trim_or_absorb_tail(project, ti, Straddle { index: m, start: track.start_of(m), seg }, t, &mut ops, &mut absorbed)?;
        }
    } else {
        // Covered followers, descending, then the straddled one (if any).
        // NOTE: after the removals the straddler sits at si+1, so its
        // pre-state content is passed explicitly with the adjusted index.
        let mut straddler: Option<usize> = None;
        let mut covered: Vec<usize> = Vec::new();
        let mut acc = edge;
        for k in si + 1..track.segments.len() {
            let dur = track.segments[k].duration();
            if acc + dur <= t {
                covered.push(k);
            } else if acc < t && t < acc + dur {
                straddler = Some(k);
                break;
            } else {
                break;
            }
            acc += dur;
        }
        for k in covered.iter().rev() {
            absorbed.push(segment_note(&track.segments[*k]));
            ops.push(EditOp::Remove {
                track: ti,
                index: *k,
                segment: track.segments[*k].clone(),
            });
        }
        if let Some(m) = straddler {
            let seg = track.segments[m].clone();
            trim_or_absorb_head(
                project,
                ti,
                Straddle { index: si + 1, start: track.start_of(m), seg },
                t,
                &mut ops,
                &mut absorbed,
            )?;
        }
    }

    let side = if head { "head" } else { "tail" };
    let mut desc = format!("extend {side} of clip #{clip_id} to {}", format_seconds(&t));
    if !absorbed.is_empty() {
        desc += &format!(" (absorbed {})", absorbed.join(", "));
    }
    Ok(Batch { ops, desc })
}

/// Describe an absorbed segment for the batch description.
fn segment_note(seg: &Segment) -> String {
    match seg {
        Segment::Gap(_) => "gap".to_string(),
        Segment::Clip(c) => format!("clip #{}", c.id),
    }
}

/// Where a straddled segment sits when its reconciling op applies
/// (`index`), and what it looked like before any removals (`seg` at
/// `start`). Removals ahead of it may have shifted it from its pre-state
/// position, so the two are tracked separately.
struct Straddle {
    index: usize,
    start: Seconds,
    seg: Segment,
}

/// Trim a straddled follower's head to `t`, or absorb it whole when under
/// a frame would remain. A straddled gap simply shrinks to its uncovered
/// remainder. Appends to `ops`/`absorbed`.
fn trim_or_absorb_head(
    project: &Project,
    ti: usize,
    target: Straddle,
    t: Seconds,
    ops: &mut Vec<EditOp>,
    absorbed: &mut Vec<String>,
) -> Result<(), EditError> {
    let (index, start, seg) = (target.index, target.start, target.seg);
    match seg {
        // A straddled gap shrinks to its uncovered remainder.
        Segment::Gap(g) => {
            ops.push(EditOp::Replace {
                track: ti,
                index,
                before: Segment::Gap(g.clone()),
                after: Segment::Gap(Gap { duration: start + g.duration - t }),
            });
            Ok(())
        }
        Segment::Clip(c) => {
            let remainder = start + c.duration() - t;
            if at_least_a_frame(project, &remainder).is_err() {
                absorbed.push(format!("clip #{}", c.id));
                ops.push(EditOp::Remove {
                    track: ti,
                    index,
                    segment: Segment::Clip(c),
                });
                return Ok(());
            }
            let after = Clip {
                asset_in: c.asset_in + (t - start),
                ..c.clone()
            };
            absorbed.push(format!("trimmed clip #{}", c.id));
            ops.push(EditOp::Replace {
                track: ti,
                index,
                before: Segment::Clip(c),
                after: Segment::Clip(after),
            });
            Ok(())
        }
    }
}

/// Trim a straddled predecessor's tail to `t`, or absorb it whole when
/// under a frame would remain.
fn trim_or_absorb_tail(
    project: &Project,
    ti: usize,
    target: Straddle,
    t: Seconds,
    ops: &mut Vec<EditOp>,
    absorbed: &mut Vec<String>,
) -> Result<(), EditError> {
    let (index, start, seg) = (target.index, target.start, target.seg);
    match seg {
        // A straddled gap shrinks to its uncovered remainder.
        Segment::Gap(g) => {
            ops.push(EditOp::Replace {
                track: ti,
                index,
                before: Segment::Gap(g.clone()),
                after: Segment::Gap(Gap { duration: g.duration - (t - start) }),
            });
            Ok(())
        }
        Segment::Clip(c) => {
            let remainder = t - start;
            if at_least_a_frame(project, &remainder).is_err() {
                absorbed.push(format!("clip #{}", c.id));
                ops.push(EditOp::Remove {
                    track: ti,
                    index,
                    segment: Segment::Clip(c),
                });
                return Ok(());
            }
            let after = Clip {
                asset_out: c.asset_in + remainder,
                ..c.clone()
            };
            absorbed.push(format!("trimmed clip #{}", c.id));
            ops.push(EditOp::Replace {
                track: ti,
                index,
                before: Segment::Clip(c),
                after: Segment::Clip(after),
            });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RawFormat, RawProbe, RawStream};
    use crate::project::VideoSettings;

    fn probe_report(duration: &str) -> RawProbe {
        RawProbe {
            streams: vec![RawStream {
                index: 0,
                codec_type: Some("video".into()),
                codec_name: Some("h264".into()),
                width: Some(320),
                height: Some(240),
                r_frame_rate: Some("30/1".into()),
                ..Default::default()
            }],
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    /// [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5) src 0..2]
    fn fixture() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings { width: 320, height: 240 });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(1),
                asset_out: Seconds::from_integer(3),
            }),
            Segment::Gap(Gap { duration: Seconds::from_integer(1) }),
            Segment::Clip(Clip {
                id: 2,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
            }),
        ];
        p
    }

    fn clip_by_id(p: &Project, id: u32) -> Clip {
        p.timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Clip(c) => Some(c),
                Segment::Gap(_) => None,
            })
            .find(|c| c.id == id)
            .unwrap()
            .clone()
    }

    fn clip_ids(p: &Project) -> Vec<u32> {
        p.timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Clip(c) => Some(c.id),
                Segment::Gap(_) => None,
            })
            .collect()
    }

    #[test]
    fn i_trims_clip_head_and_leaves_a_gap() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_in(&p, 1.0).unwrap();
        assert!(batch.desc.contains("trim in"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        // [Gap 1 @0..1] [Clip #1 [1..2) src 2..3] [Gap 1 @2..3] [Clip #2]
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(1)));
        let c = clip_by_id(&p, 1);
        assert_eq!(c.asset_in, Seconds::from_integer(2));
        assert_eq!(c.duration(), Seconds::from_integer(1));
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(2), "follower unmoved");
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_in, Seconds::from_integer(1));
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);
    }

    #[test]
    fn o_trims_clip_tail_and_leaves_a_gap() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_out(&p, 4.0).unwrap(); // mid clip #2 [3..5)
        h.apply(&mut p, batch).unwrap();
        let c = clip_by_id(&p, 2);
        assert_eq!(c.asset_out, Seconds::from_integer(1), "src 0..1");
        assert_eq!(c.duration(), Seconds::from_integer(1));
        // the trimmed material's gap sits AFTER the clip (index 3)
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(1)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).asset_out, Seconds::from_integer(2));
    }

    #[test]
    fn i_against_gap_grows_it_instead_of_splitting() {
        let mut p = fixture(); // [Clip #1] [Gap 1] [Clip #2 [3..5)]
        let mut h = History::default();
        let batch = trim_in(&p, 4.0).unwrap(); // 1s off clip #2's head
        h.apply(&mut p, batch).unwrap();
        // no new segment: the existing gap absorbed the trimmed second
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2)));
        let c = clip_by_id(&p, 2);
        assert_eq!(c.asset_in, Seconds::from_integer(1));
        assert_eq!(c.duration(), Seconds::from_integer(1));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
        h.undo(&mut p).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1)));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
    }

    #[test]
    fn o_against_gap_grows_it_instead_of_splitting() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_out(&p, 1.0).unwrap(); // 1s off clip #1's tail
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2)));
        let c = clip_by_id(&p, 1);
        assert_eq!(c.asset_out, Seconds::from_integer(2));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
    }

    #[test]
    fn i_ripple_against_gap_leaves_no_gap_trace() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_in_ripple(&p, 4.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        // only the clip shortened: the neighboring gap is untouched
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1)));
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(1));
        assert_eq!(p.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn o_ripple_against_gap_leaves_no_gap_trace() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_out_ripple(&p, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1)));
        assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(1));
        assert_eq!(p.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn i_against_clip_still_inserts_a_gap() {
        // [Clip #1] [Clip #2], no gap between: trimming still splits.
        let mut p = fixture();
        p.timeline.video_track_mut().unwrap().segments.remove(1);
        let mut h = History::default();
        let batch = trim_in(&p, 3.5).unwrap(); // 1.5 off clip #2's head
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::new(3, 2)));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 2);
    }

    #[test]
    fn i_ripple_shrinks_and_slides_followers() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_in_ripple(&p, 1.5).unwrap(); // mid clip #1
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 1).duration(), Seconds::new(1, 2), "head trimmed 1.5");
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(2), "source kept");
        assert_eq!(p.timeline.end(), Seconds::new(7, 2), "shrunk by 1.5");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn o_ripple_shrinks_and_slides_followers() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_out_ripple(&p, 4.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(1));
        assert_eq!(p.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn i_on_gap_extends_prev_clip_tail() {
        let mut p = fixture(); // gap [2..3)
        let mut h = History::default();
        let batch = trim_in(&p, 2.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        let c = clip_by_id(&p, 1);
        assert_eq!(c.asset_out, Seconds::new(7, 2), "tail grew 0.5 into the gap");
        // the region's remaining half stays a gap
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::new(1, 2)));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
    }

    #[test]
    fn o_on_gap_extends_next_clip_head() {
        let mut p = fixture();
        // clip #2's head needs source before its in-point: src 1..3
        p.timeline.video_track_mut().unwrap().segments[2] = Segment::Clip(Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(3),
        });
        let mut h = History::default();
        let batch = trim_out(&p, 2.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        let c = clip_by_id(&p, 2);
        assert_eq!(c.asset_in, Seconds::new(1, 2), "head grew 0.5 earlier");
        assert_eq!(c.duration(), Seconds::new(5, 2));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(1));
    }

    #[test]
    fn o_on_gap_without_head_material_is_rejected() {
        let p = fixture(); // clip #2 src 0..2 — nothing before 0
        assert!(matches!(trim_out(&p, 2.5), Err(EditError::BeforeAsset { .. })));
    }

    #[test]
    fn o_on_trailing_gap_shortens_the_tail() {
        let mut p = fixture();
        p.timeline.video_track_mut().unwrap().segments.push(Segment::Gap(Gap {
            duration: Seconds::from_integer(3),
        })); // trailing [5..8)
        let mut h = History::default();
        let batch = trim_out(&p, 6.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::new(3, 2)));
        assert_eq!(p.timeline.end(), Seconds::new(13, 2), "6.5");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(8));
    }

    #[test]
    fn i_on_leading_gap_shortens_from_the_front() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            Segment::Gap(Gap { duration: Seconds::from_integer(2) }),
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
            }),
        ];
        let mut h = History::default();
        let batch = trim_in(&p, 1.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::new(1, 2)));
        assert_eq!(p.timeline.end(), Seconds::new(5, 2), "everything slid left 1.5");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(4));
    }

    #[test]
    fn s_splits_a_clip() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap(); // mid clip #2 [3..5)
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2, 3]);
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(1));
        assert_eq!(clip_by_id(&p, 3).duration(), Seconds::from_integer(1));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
    }

    #[test]
    fn dd_replaces_clip_with_gap_d_removes() {
        let mut p = fixture();
        let mut h = History::default();
        // dd the first clip: a gap replaces it, fusing with the
        // neighboring gap into one — end preserved
        let batch = delete_under(&p, 1.0).unwrap();
        assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 2);
        assert!(matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(3)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);

        // D the clip: removed, timeline shrinks by 2
        let batch = delete_ripple(&p, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_ids(&p), vec![2]);
        // the trailing gap survives (tail size preserved): end = 1(gap) + 2(clip)
        assert_eq!(p.timeline.end(), Seconds::from_integer(3), "shrunk by the clip only");

        // D the gap (re-inserted by undo? no — fresh fixture): D on a gap
        // shrinks the timeline by the gap's length
        let mut p2 = fixture();
        let batch = delete_ripple(&p2, 2.5).unwrap(); // the 1s gap
        h.apply(&mut p2, batch).unwrap();
        assert_eq!(clip_by_id(&p2, 2).duration(), Seconds::from_integer(2), "spans intact");
        assert_eq!(p2.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
    }

    #[test]
    fn dd_on_trailing_gap_removes_it() {
        let mut p = fixture();
        p.timeline.video_track_mut().unwrap().segments.push(Segment::Gap(Gap {
            duration: Seconds::from_integer(3),
        })); // trailing [5..8)
        let mut h = History::default();
        let batch = delete_under(&p, 6.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "tail gone");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(8));
    }

    #[test]
    fn d_on_gap_between_matching_clips_rejoins_them() {
        // [Clip #1 [0..2) src 1..3] [Gap 1] [Clip #5 [3..5) src 3..5]:
        // contiguous halves of one source range.
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(1),
                asset_out: Seconds::from_integer(3),
            }),
            Segment::Gap(Gap { duration: Seconds::from_integer(1) }),
            Segment::Clip(Clip {
                id: 5,
                asset: a,
                asset_in: Seconds::from_integer(3),
                asset_out: Seconds::from_integer(5),
            }),
        ];
        let mut h = History::default();
        let batch = delete_ripple(&p, 2.5).unwrap();
        assert!(batch.desc.contains("merged clips #1+#5"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        // one clip, earlier id, whole source range
        assert_eq!(clip_ids(&p), vec![1]);
        let c = clip_by_id(&p, 1);
        assert_eq!(c.asset_in, Seconds::from_integer(1));
        assert_eq!(c.asset_out, Seconds::from_integer(5));
        assert_eq!(p.timeline.end(), Seconds::from_integer(4));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 5]);
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn d_on_gap_between_unrelated_clips_leaves_both() {
        let mut p = fixture(); // clip #1 src 1..3, clip #2 src 0..2: not contiguous
        let mut h = History::default();
        let batch = delete_ripple(&p, 2.5).unwrap();
        assert!(!batch.desc.contains("merged"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(p.timeline.end(), Seconds::from_integer(4));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
    }

    #[test]
    fn dd_clip_between_gaps_merges_into_one() {
        // [Gap 2] [Clip #1 dur 2] [Gap 3]: dd the clip.
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            Segment::Gap(Gap { duration: Seconds::from_integer(2) }),
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
            }),
            Segment::Gap(Gap { duration: Seconds::from_integer(3) }),
        ];
        let mut h = History::default();
        let batch = delete_under(&p, 3.0).unwrap();
        assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 1);
        assert!(matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(7)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(7), "end preserved");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);
        assert_eq!(clip_ids(&p), vec![1]);
    }

    #[test]
    fn d_on_clip_between_gaps_merges_the_gaps() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            Segment::Gap(Gap { duration: Seconds::from_integer(1) }),
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
            }),
            Segment::Gap(Gap { duration: Seconds::from_integer(1) }),
        ];
        let mut h = History::default();
        let batch = delete_ripple(&p, 2.0).unwrap();
        assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 1);
        assert!(matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(2)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(2), "shrunk by the clip");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1]);
    }

    #[test]
    fn p_replaces_underneath() {
        // fixture: [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5) src 0..2]
        let mut p = fixture();
        let mut h = History::default();
        let snapshot = clip_by_id(&p, 1); // 2s

        // Over the tail of the gap and 1.5s into clip #2: the gap is
        // replaced up to the paste, #2 is trimmed at its near edge.
        let batch = paste(&p, &snapshot, 2.5).unwrap();
        assert!(batch.desc.contains("trimmed clip #2"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        let segs = p.timeline.video_track().unwrap().segments.clone();
        assert_eq!(clip_ids(&p), vec![1, 3, 2]);
        assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::new(1, 2)));
        assert_eq!(clip_by_id(&p, 3).duration(), Seconds::from_integer(2), "pasted");
        let c2 = clip_by_id(&p, 2);
        assert_eq!(c2.asset_in, Seconds::new(3, 2), "trimmed head");
        assert_eq!(c2.duration(), Seconds::new(1, 2));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "length unchanged");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);

        // Fully covering the gap and #2: #2 is removed, the paste grows to
        // fill exactly the space it overwrote.
        let three = Clip {
            id: 0,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(3),
        };
        let batch = paste(&p, &three, 2.0).unwrap();
        assert!(batch.desc.contains("replaced clip #2"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 3]);
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "length unchanged");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);

        // Wholly inside clip #1: the clip splits, the right half gets a
        // fresh id, and the paste sits between the halves.
        let half = Clip {
            id: 0,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::new(1, 2),
        };
        let batch = paste(&p, &half, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 3, 4, 2]);
        assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(1), "head half");
        assert_eq!(
            (clip_by_id(&p, 4).asset_in, clip_by_id(&p, 4).duration()),
            (Seconds::new(5, 2), Seconds::new(1, 2)),
            "tail half keeps the source tail"
        );
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);

        // Past the end: the void becomes a gap, then the clip; the
        // timeline grows.
        let batch = paste(&p, &snapshot, 6.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(1)));
        assert_eq!(clip_ids(&p), vec![1, 2, 3]);
        assert_eq!(p.timeline.end(), Seconds::from_integer(8));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn p_ripple_inserts_and_grows() {
        let mut p = fixture();
        let mut h = History::default();
        let snapshot = clip_by_id(&p, 1); // 2s
        let _tiny = Clip {
            id: 0,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::new(1, 2), // 0.5s
        };

        // mid-clip: split + insert between
        let batch = paste_ripple(&p, &snapshot, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 3, 4, 2]);
        assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(1), "left half");
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(2), "source kept");
        assert_eq!(p.timeline.end(), Seconds::from_integer(7), "grew by 2");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));

        // in a gap: the gap splits, followers slide, timeline grows
        let batch = paste_ripple(&p, &snapshot, 2.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(7), "grew by 2");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));

        // past the end: the void becomes a gap, then the clip
        let batch = paste_ripple(&p, &snapshot, 7.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(2)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(9));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));

        // at a cut: the paste lands BEFORE the clip; the clip keeps its id
        let batch = paste_ripple(&p, &snapshot, 3.0).unwrap();
        eprintln!("at-cut batch: {batch:?}");
        eprintln!("before: ids={:?} end={}", clip_ids(&p), p.timeline.end());
        h.apply(&mut p, batch).unwrap();
        eprintln!("after: ids={:?}", clip_ids(&p));
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(2), "source kept");
        assert_eq!(clip_by_id(&p, 3).duration(), Seconds::from_integer(2), "pasted");
        h.undo(&mut p).unwrap();
    }

    #[test]
    fn extend_tail_grows_clip_and_followers_shift() {
        let mut p = fixture(); // [Clip #1 [0..2)] [Gap 1] [Clip #2 [3..5)]
        let mut h = History::default();
        // extend tail of #1 to 2.5 (asset has material to 10): gap stays,
        // followers shift right, timeline grows by 0.5
        let batch = extend_edge(&p, 1, false, 2.5).unwrap();
        assert!(batch.desc.contains("extend tail of clip #1"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        let c = clip_by_id(&p, 1);
        assert_eq!(c.asset_out, Seconds::new(7, 2));
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1)));
        assert_eq!(p.timeline.end(), Seconds::new(11, 2));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn extend_tail_past_end_grows_timeline_from_material() {
        let mut p = fixture();
        p.timeline.video_track_mut().unwrap().segments.pop(); // drop clip #2
        p.timeline.video_track_mut().unwrap().segments.pop(); // drop the gap: [#1 [0..2)]
        let mut h = History::default();
        let batch = extend_edge(&p, 1, false, 4.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(5));
        assert_eq!(p.timeline.end(), Seconds::from_integer(4));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(2));
    }

    #[test]
    fn extend_tail_refuses_without_material() {
        let p = fixture(); // asset ends at 10; clip #1 out is 3
        assert!(matches!(
            extend_edge(&p, 1, false, 12.0),
            Err(EditError::BeyondAsset { .. })
        ));
        // wrong side is not an extend
        assert!(matches!(
            extend_edge(&p, 1, false, 1.0),
            Err(EditError::NothingToTrim(_))
        ));
    }

    #[test]
    fn extend_head_reaches_earlier_material() {
        let mut p = fixture();
        // clip #2 src 0..2 has nothing before 0: give it headroom first
        p.timeline.video_track_mut().unwrap().segments[2] = Segment::Clip(Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(3),
        });
        let mut h = History::default();
        let batch = extend_edge(&p, 2, true, 2.5).unwrap();
        assert!(batch.desc.contains("extend head of clip #2"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        let c = clip_by_id(&p, 2);
        assert_eq!(c.asset_in, Seconds::new(1, 2));
        assert_eq!(p.timeline.end(), Seconds::new(11, 2));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(1));
    }

    #[test]
    fn extend_head_refuses_without_material() {
        let p = fixture(); // clip #2 src 0..2 — nothing before 0
        assert!(matches!(
            extend_edge(&p, 2, true, 2.5),
            Err(EditError::BeforeAsset { .. })
        ));
        assert!(matches!(
            extend_edge(&p, 2, true, 4.0),
            Err(EditError::NothingToTrim(_))
        ));
        assert!(matches!(extend_edge(&p, 99, true, 1.0), Err(EditError::ClipNotFound(99))));
    }

    #[test]
    fn extend_gap_end_lengthens_trailing_gap() {
        let mut p = fixture();
        p.timeline.video_track_mut().unwrap().segments.push(Segment::Gap(Gap {
            duration: Seconds::from_integer(3),
        })); // trailing [5..8)
        let mut h = History::default();
        let batch = extend_gap_end(&p, 10.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(5)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(10));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(8));
        // not past the gap start, and no trailing gap at all, both refuse
        assert!(matches!(extend_gap_end(&p, 4.0), Err(EditError::NothingToTrim(_))));
        let q = fixture(); // ends with a clip
        assert!(matches!(extend_gap_end(&q, 6.0), Err(EditError::NoGap)));
    }

    #[test]
    fn extend_gap_tail_moves_any_gap() {
        let mut p = fixture(); // [Clip #1 [0..2)] [Gap 1 @2..3] [Clip #2 [3..5)]
        let mut h = History::default();
        // Ripple (E): grow to [2..4), clip #2 shifts right, timeline grows.
        let batch = extend_gap_tail(&p, 1, 4.0, true).unwrap();
        h.apply(&mut p, batch).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(6));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));

        // Replace (e): grow to [2..4.5) overwrites clip #2's head — it is
        // trimmed, not shifted, and the timeline length is preserved.
        let batch = extend_gap_tail(&p, 1, 4.5, false).unwrap();
        assert!(batch.desc.contains("trimmed clip #2"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        assert!(matches!(&p.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::new(5, 2)));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::new(3, 2));
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::new(1, 2));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "length held");
        h.undo(&mut p).unwrap();

        // Shrink to [2..2.5): clip #2 slides left either way.
        let batch = extend_gap_tail(&p, 1, 2.5, false).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(p.timeline.end(), Seconds::new(9, 2));
        // Not a gap / at/before its start both refuse.
        assert!(matches!(extend_gap_tail(&p, 0, 1.0, false), Err(EditError::NoGap)));
        assert!(matches!(extend_gap_tail(&p, 1, 0.0, true), Err(EditError::NothingToTrim(_))));
    }

    #[test]
    fn extend_gap_head_replaces_the_previous_segment() {
        let mut p = fixture(); // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
        let mut h = History::default();
        // Grow the gap's head to 1.0: clip #1 is trimmed to [0..1).
        let batch = extend_gap_head(&p, 1, 1.0).unwrap();
        assert!(batch.desc.contains("trimmed clip #1"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        let segs = &p.timeline.video_track().unwrap().segments;
        assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(1));
        assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "length held");
        h.undo(&mut p).unwrap();
        // Grow to 0.0: clip #1 is absorbed whole.
        let batch = extend_gap_head(&p, 1, 0.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert!(matches!(&p.timeline.video_track().unwrap().segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(3)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
        // Rightward (at/past the start) refuses; a clip is not a gap.
        let fresh = fixture();
        assert!(matches!(extend_gap_head(&fresh, 1, 2.0), Err(EditError::NothingToTrim(_))));
        assert!(matches!(extend_gap_head(&fresh, 1, 3.0), Err(EditError::NothingToTrim(_))));
        assert!(matches!(extend_gap_head(&fresh, 0, 1.0), Err(EditError::NoGap)));
    }

    #[test]
    fn absorb_over_gap_and_clip_removes_and_trims() {
        // [Clip #1 [0..2) src 1..3] [Gap 1] [Clip #2 [3..5) src 0..2],
        // asset is 10s: extend tail of #1 to 4.0.
        let mut p = fixture();
        let mut h = History::default();
        let batch = absorb_outward(&p, 1, false, 4.0).unwrap();
        assert!(batch.desc.contains("absorbed"), "{}", batch.desc);
        assert!(batch.desc.contains("trimmed clip #2"), "{}", batch.desc);
        h.apply(&mut p, batch).unwrap();
        // #1 spans [0..4), the gap is gone, #2 keeps [4..5) src 1..2.
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(5));
        let c2 = clip_by_id(&p, 2);
        assert_eq!(c2.asset_in, Seconds::from_integer(1));
        assert_eq!(c2.duration(), Seconds::from_integer(1));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);
    }

    #[test]
    fn absorb_over_two_gaps_and_a_clip() {
        // [Clip #1 [0..2)] [Gap 1] [Clip #2 [3..5)] [Gap 1] [Clip #3 [6..8)]:
        // extend tail of #1 to 6.5 absorbs both gaps, removes #2, trims #3.
        let mut p = fixture();
        {
            let track = p.timeline.video_track_mut().unwrap();
            track.segments.push(Segment::Gap(Gap { duration: Seconds::from_integer(1) }));
            track.segments.push(Segment::Clip(Clip {
                id: 3,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
            }));
        }
        let mut h = History::default();
        let batch = absorb_outward(&p, 1, false, 6.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 3]);
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::new(15, 2));
        assert_eq!(clip_by_id(&p, 3).asset_in, Seconds::new(1, 2));
        assert_eq!(p.timeline.end(), Seconds::from_integer(8));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2, 3]);
        assert_eq!(p.timeline.end(), Seconds::from_integer(8));
    }

    #[test]
    fn absorb_head_mirrors_tail() {
        // Extend head of #2 left to 1.0: absorbs the gap, trims nothing
        // else (#1 ends at 2.0 > 1.0, so #1 is only... wait: covered is
        // [1.0, 3.0): the gap [2..3) fully, #1 [0..2) straddled -> trim
        // #1's tail to 1.0.
        let mut p = fixture();
        // clip #2 needs 2s of head material for the reach to 1.0.
        p.timeline.video_track_mut().unwrap().segments[2] = Segment::Clip(Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(2),
            asset_out: Seconds::from_integer(4),
        });
        let mut h = History::default();
        let batch = absorb_outward(&p, 2, true, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
        let c1 = clip_by_id(&p, 1);
        assert_eq!(c1.asset_out, Seconds::from_integer(2), "trimmed tail");
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(2));
    }

    #[test]
    fn absorb_sub_frame_remainder_removes_whole() {
        // A straddled follower left with under a frame is absorbed whole
        // instead of leaving a sliver. Reachable via hand-placed
        // off-grid ends (snapped positions always leave whole frames).
        let mut p = fixture();
        p.timeline.video_track_mut().unwrap().segments[2] = Segment::Clip(Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::new(99, 50),
        }); // C2 [3..4.98)
        let mut h = History::default();
        // 4.97 snaps to frame 149 (4.9667), leaving a 0.013s stump:
        // under half a 30fps frame, so #2 is absorbed whole.
        let batch = absorb_outward(&p, 1, false, 4.97).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert_eq!(clip_ids(&p), vec![1]);
        assert_eq!(p.timeline.end(), Seconds::new(149, 30));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
    }

    #[test]
    fn absorb_validates_direction_and_material() {
        let p = fixture();
        // inward is not an extend
        assert!(matches!(absorb_outward(&p, 1, false, 1.0), Err(EditError::NothingToTrim(_))));
        assert!(matches!(absorb_outward(&p, 2, true, 6.0), Err(EditError::NothingToTrim(_))));
        // past the asset end refuses
        assert!(matches!(
            absorb_outward(&p, 1, false, 12.0),
            Err(EditError::BeyondAsset { .. })
        ));
        assert!(matches!(absorb_outward(&p, 99, false, 3.0), Err(EditError::ClipNotFound(99))));
    }

    #[test]
    fn failed_batches_leave_no_trace() {
        let p = fixture();
        let h = History::default();
        assert!(trim_in(&p, 99.0).is_err()); // past the end
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
        let _ = h;
    }

    #[test]
    fn sidecar_persists_done_only_and_round_trips() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let batch = trim_in(&p, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        h.undo(&mut p).unwrap(); // done=[split], undone=[trim] (dies on save)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        h.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 3"), "{text}");
        assert!(!text.contains("undone"), "{text}");
        assert!(!text.contains("30/1"), "{text}");

        let mut loaded = History::load(&path).unwrap();
        assert!(loaded.can_undo());
        assert!(!loaded.can_redo(), "redo dies across restarts");
        let mut q = p.clone();
        assert!(matches!(loaded.redo(&mut q), Err(EditError::NothingToRedo)));
        // the surviving entry still undoes
        loaded.undo(&mut q).unwrap();
        assert_eq!(clip_ids(&q), vec![1, 2]);
    }

    #[test]
    fn persisted_edits_undo_by_topology_after_reload() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let batch = trim_in(&p, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let snapshot = clip_by_id(&p, 1);
        let batch = paste_ripple(&p, &snapshot, 2.5).unwrap();
        h.apply(&mut p, batch).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        h.save(&path).unwrap();

        // Restart simulation: project and history both reloaded from disk
        // (production saves both after every edit). The reloaded project
        // is in the post-edit state; history is purely from the sidecar.
        let saved_project = serde_json::to_string(&p).unwrap();
        let mut p2: Project = serde_json::from_str(&saved_project).unwrap();
        let mut h2 = History::load(&path).unwrap();
        h2.undo(&mut p2).unwrap(); // paste_ripple
        h2.undo(&mut p2).unwrap(); // trim_in
        let desc = h2.undo(&mut p2).unwrap(); // split
        assert!(desc.contains("split"), "{desc}");
        assert_eq!(clip_ids(&p2), vec![1, 2]);
        assert_eq!(clip_by_id(&p2, 2).asset_out, Seconds::from_integer(2));
        assert_eq!(p2.timeline.end(), Seconds::from_integer(5));
        // redo was rebuilt live in-session
        assert!(h2.can_redo());
        h2.redo(&mut p2).unwrap();
        assert_eq!(clip_ids(&p2), vec![1, 2, 3]);
    }

    #[test]
    fn stale_history_fails_cleanly_on_edited_project() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        h.save(&path).unwrap();

        // Hand-edit outside undo: renumber a clip id.
        let mut p2 = fixture();
        if let Segment::Clip(c) = &mut p2.timeline.video_track_mut().unwrap().segments[2] {
            c.id = 99;
        }
        let mut h2 = History::load(&path).unwrap();
        assert!(matches!(
            h2.undo(&mut p2),
            Err(EditError::Timeline(TimelineError::Corrupt(_)))
        ));
        // project untouched by the failed undo
        assert_eq!(clip_ids(&p2), vec![1, 99]);
        assert_eq!(p2.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn recorded_frames_scale_with_fps() {
        // The same 1s trim records 30 gap frames at 30fps, 60 at 60fps.
        let mut p30 = fixture();
        let mut h30 = History::default();
        let batch = trim_in(&p30, 1.0).unwrap();
        h30.apply(&mut p30, batch).unwrap();
        let first_gap = |h: &History| {
            h.state().done[0]
                .after
                .iter()
                .find_map(|a| match a {
                    Atom::Gap { frames } => Some(*frames),
                    Atom::Clip { .. } => None,
                })
                .unwrap()
        };
        assert_eq!(first_gap(&h30), 30);

        let mut p60 = fixture();
        p60.fps = Some(Seconds::new(60, 1));
        let mut h60 = History::default();
        let batch = trim_in(&p60, 1.0).unwrap();
        h60.apply(&mut p60, batch).unwrap();
        assert_eq!(first_gap(&h60), 60);

        // Reinterpreting the project at another fps invalidates loudly:
        // frame counts no longer match, so undo refuses instead of
        // mis-restoring.
        p30.fps = Some(Seconds::new(60, 1));
        assert!(matches!(
            h30.undo(&mut p30),
            Err(EditError::Timeline(TimelineError::Corrupt(_)))
        ));
    }

    #[test]
    fn history_depth_is_capped_and_redo_clears() {
        let mut p = fixture();
        let mut h = History::default();
        let snapshot = clip_by_id(&p, 1);
        for _ in 0..(MAX_HISTORY + 5) {
            let end = p.timeline.end();
            let end_f = *end.numer() as f64 / *end.denom() as f64;
            let batch = paste_ripple(&p, &snapshot, end_f + 10.0).unwrap();
            h.apply(&mut p, batch).unwrap();
        }
        assert_eq!(h.depth(), MAX_HISTORY);
        // cap survives a save/load round trip
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        h.save(&path).unwrap();
        assert_eq!(History::load(&path).unwrap().depth(), MAX_HISTORY);

        // a new edit clears the redo stack
        h.undo(&mut p).unwrap();
        assert!(h.can_redo());
        let end = p.timeline.end();
        let end_f = *end.numer() as f64 / *end.denom() as f64;
        let batch = paste_ripple(&p, &snapshot, end_f + 10.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        assert!(!h.can_redo());
    }

    #[test]
    fn corrupt_and_foreign_sidecars_start_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(History::load(&bad).is_err());

        // v2 stored raw op shapes: rejected, never misread
        let old = dir.path().join("old.json");
        std::fs::write(&old, "{\"version\": 2, \"done\": [], \"undone\": []}").unwrap();
        assert!(History::load(&old).is_err());
        assert!(History::restore(HistoryState { version: 2, done: vec![] }).depth() == 0);
    }

    #[test]
    fn failed_apply_restores_project_and_keeps_history_empty() {
        let mut p = fixture();
        let mut h = History::default();
        let before = p.clone();
        // Zero-duration replacement passes op application but fails validate.
        let seg = p.timeline.video_track().unwrap().segments[0].clone();
        let bad = match seg {
            Segment::Clip(c) => Segment::Clip(Clip {
                asset_out: c.asset_in,
                ..c
            }),
            Segment::Gap(_) => unreachable!(),
        };
        let batch = Batch {
            ops: vec![EditOp::Replace {
                track: 0,
                index: 0,
                before: p.timeline.video_track().unwrap().segments[0].clone(),
                after: bad,
            }],
            desc: "bad edit".into(),
        };
        assert!(h.apply(&mut p, batch).is_err());
        assert_eq!(p, before, "project untouched");
        assert_eq!(h.depth(), 0, "nothing pushed");
    }

    #[test]
    fn failed_undo_keeps_its_history_entry() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 1.0).unwrap();
        h.apply(&mut p, batch).unwrap();
        let expected = p.clone();
        // External mutation: undo must refuse and keep its entry.
        p.timeline.video_track_mut().unwrap().segments.pop();
        assert!(h.undo(&mut p).is_err());
        assert_eq!(h.depth(), 1, "entry preserved for retry");
        assert!(!h.can_redo());
        // Restoring the expected topology makes undo work again.
        p = expected;
        assert!(h.undo(&mut p).is_ok());
        assert_eq!(h.depth(), 0);
    }
}
