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
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::project::{Asset, Project, ProjectError};
use crate::time::{Seconds, format_seconds};
use crate::timeline::{Clip, Gap, Segment, TimelineError, Track, TrackKind};

/// Deepest undo stack we keep (changes).
pub const MAX_HISTORY: usize = 100;
/// Sidecar file format version. No migration path is kept (see
/// README "Compatibility: none, by design"): old sidecars start fresh.
/// v7 stored clip speed as raw `speed_num`/`speed_den` integers; v8 stores
/// it as a `rational_ser` fraction like every other time in the Atom.
pub const HISTORY_VERSION: u32 = 8;

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
    #[error("speed must be positive (got {0})")]
    BadSpeed(String),
    #[error("cannot extend clip #{id} — no source after {out} (asset ends at {duration})")]
    BeyondAsset {
        id: u32,
        out: String,
        duration: String,
    },
    #[error("cannot extend clip #{id} — needs {needed} of source before its in-point ({ain})")]
    BeforeAsset {
        id: u32,
        needed: String,
        ain: String,
    },
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
    #[error("history entry does not apply: {0}")]
    History(String),
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
    Insert {
        track: usize,
        index: usize,
        segment: Segment,
    },
    /// The embedded segment is the removed snapshot (its inverse re-adds it).
    Remove {
        track: usize,
        index: usize,
        segment: Segment,
    },
    /// `before` is the safety snapshot: applying verifies it's still there.
    Replace {
        track: usize,
        index: usize,
        before: Segment,
        after: Segment,
    },
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
            EditOp::Replace {
                index,
                before,
                after,
                ..
            } => {
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
            EditOp::Insert {
                track,
                index,
                segment,
            } => EditOp::Remove {
                track: *track,
                index: *index,
                segment: segment.clone(),
            },
            EditOp::Remove {
                track,
                index,
                segment,
            } => EditOp::Insert {
                track: *track,
                index: *index,
                segment: segment.clone(),
            },
            EditOp::Replace {
                track,
                index,
                before,
                after,
            } => EditOp::Replace {
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

/// Undo/redo stack of project changes. `apply` (timeline) and `remove_asset`
/// (pool) are the only ways in: each snapshots the affected state before
/// mutating and stores the before/after pair as a `Change`. Undo and redo
/// verify the live state against the recorded side (segment id/frame for
/// tracks, exact pool equality for assets) and substitute the other.
#[derive(Debug, Default, Clone)]
pub struct History {
    done: Vec<Change>,
    undone: Vec<Change>,
}

impl History {
    /// Apply `batch` (records `playhead` — the timeline frame the edit was
    /// made from — so undo/redo can return there).
    pub fn apply(
        &mut self,
        project: &mut Project,
        batch: Batch,
        playhead: i64,
    ) -> Result<(), EditError> {
        if batch.ops.is_empty() {
            project.validate()?;
            return Ok(());
        }
        let track = batch_track(&batch, project)?;
        let before = snapshot_track(&project.timeline.tracks[track]);
        let saved = project.timeline.tracks[track].segments.clone();
        if let Err(e) = apply_batch(project, &batch.ops)
            .and_then(|_| project.validate().map_err(EditError::from))
        {
            project.timeline.tracks[track].segments = saved;
            return Err(e);
        }
        let after = snapshot_track(&project.timeline.tracks[track]);
        self.push_change(Change::Track(TrackChange {
            track,
            start_frame: 0,
            before,
            after,
            desc: batch.desc,
            playhead,
        }));
        Ok(())
    }

    /// Remove `id` from the media pool and record it so undo restores it.
    /// `Project::remove_asset`'s guard refuses while any clip uses it; the
    /// before/after pool snapshots make the change symmetric.
    pub fn remove_asset(
        &mut self,
        project: &mut Project,
        id: u32,
        playhead: i64,
    ) -> Result<String, EditError> {
        let before = project.media.clone();
        let removed = project.remove_asset(id)?;
        let after = project.media.clone();
        let desc = format!("remove asset #{}", removed.id);
        self.push_change(Change::Assets(AssetChange {
            before,
            after,
            desc: desc.clone(),
            playhead,
        }));
        Ok(desc)
    }

    /// Push onto `done`, clear redo, and enforce the depth cap.
    fn push_change(&mut self, change: Change) {
        self.done.push(change);
        self.undone.clear();
        if self.done.len() > MAX_HISTORY {
            let excess = self.done.len() - MAX_HISTORY;
            self.done.drain(0..excess);
        }
    }

    pub fn undo(&mut self, project: &mut Project) -> Result<UndoOutcome, EditError> {
        let change = self.done.last().ok_or(EditError::NothingToUndo)?.clone();
        let outcome = UndoOutcome {
            desc: change.desc().to_string(),
            playhead: change.playhead(),
        };
        // Snapshot the whole project so a refused substitution (the live
        // state no longer matches the recorded side) leaves it untouched.
        let snapshot = project.clone();
        if let Err(e) = substitute(project, &change, false) {
            *project = snapshot;
            return Err(e);
        }
        self.done.pop();
        self.undone.push(change);
        Ok(outcome)
    }

    pub fn redo(&mut self, project: &mut Project) -> Result<UndoOutcome, EditError> {
        let change = self.undone.last().ok_or(EditError::NothingToRedo)?.clone();
        let outcome = UndoOutcome {
            desc: change.desc().to_string(),
            playhead: change.playhead(),
        };
        let snapshot = project.clone();
        if let Err(e) = substitute(project, &change, true) {
            *project = snapshot;
            return Err(e);
        }
        self.undone.pop();
        self.done.push(change);
        Ok(outcome)
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
        Self {
            done,
            undone: Vec::new(),
        }
    }

    /// Sidecar path for a project file (`nle.json` → `nle.history.json`).
    /// The history lives next to the project so both move together.
    pub fn sidecar_path(project_path: &Path) -> PathBuf {
        project_path.with_extension("history.json")
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

/// One timeline segment, snapshotted for the undo stack. Clips are located
/// by segment id (robust to index shifts) and carry their **exact** source
/// times and speed, so undo/redo restores the clip bit-for-bit. (The old
/// frame-count form quantized source seconds to the project fps, which
/// perturbed speed-changed clips on every undo until sub-frame slivers
/// appeared and a zero-frame span failed validation.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Atom {
    Clip {
        id: u32,
        asset: u32,
        #[serde(with = "crate::time::rational_ser")]
        asset_in: Seconds,
        #[serde(with = "crate::time::rational_ser")]
        asset_out: Seconds,
        /// Playback speed as a reduced fraction; `1/1` is normal. Rational
        /// (via `rational_ser`, like every other time here) so undo/redo is
        /// exact — and so a corrupt sidecar is rejected at parse time
        /// instead of panicking inside `Ratio::new` on a zero denominator.
        #[serde(with = "crate::time::rational_ser")]
        speed: Seconds,
    },
    Gap {
        #[serde(with = "crate::time::rational_ser")]
        duration: Seconds,
    },
}

/// One undoable user action. A timeline edit carries the affected track's
/// before/after topology (verified by segment id/frame counts); a pool edit
/// carries the media list before/after. The enum keeps undo/redo exhaustive:
/// a new change kind is a compile error until it is handled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    Track(TrackChange),
    Assets(AssetChange),
}

impl Change {
    pub fn desc(&self) -> &str {
        match self {
            Change::Track(c) => &c.desc,
            Change::Assets(c) => &c.desc,
        }
    }

    pub fn playhead(&self) -> i64 {
        match self {
            Change::Track(c) => c.playhead,
            Change::Assets(c) => c.playhead,
        }
    }
}

/// A timeline edit: the affected track's segments before and after, in
/// frames. Undo substitutes `before` for `after`; redo does the reverse.
/// `start_frame` is reserved (whole-track snapshots start at 0) for future
/// region-scoped entries. `playhead` is the timeline frame the edit was made
/// from, restored on undo/redo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackChange {
    pub track: usize,
    pub start_frame: i64,
    pub before: Vec<Atom>,
    pub after: Vec<Atom>,
    pub desc: String,
    pub playhead: i64,
}

/// A media-pool edit: the whole pool before and after (probes included).
/// Only removals are recorded today. Undo verifies the pool matches the
/// recorded side, so an out-of-band change refuses loudly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetChange {
    pub before: Vec<Asset>,
    pub after: Vec<Asset>,
    pub desc: String,
    pub playhead: i64,
}

/// What an undo/redo restored: the description and the playhead frame.
#[derive(Debug, Clone, PartialEq)]
pub struct UndoOutcome {
    pub desc: String,
    pub playhead: i64,
}

fn atom_from_segment(seg: &Segment) -> Atom {
    match seg {
        Segment::Clip(c) => Atom::Clip {
            id: c.id,
            asset: c.asset,
            asset_in: c.asset_in,
            asset_out: c.asset_out,
            speed: c.speed,
        },
        Segment::Gap(g) => Atom::Gap {
            duration: g.duration,
        },
    }
}

fn segment_from_atom(atom: &Atom) -> Segment {
    match atom {
        Atom::Clip {
            id,
            asset,
            asset_in,
            asset_out,
            speed,
        } => Segment::Clip(Clip {
            id: *id,
            asset: *asset,
            asset_in: *asset_in,
            asset_out: *asset_out,
            speed: *speed,
        }),
        Atom::Gap { duration } => Segment::Gap(Gap {
            duration: *duration,
        }),
    }
}

fn snapshot_track(track: &Track) -> Vec<Atom> {
    track.segments.iter().map(atom_from_segment).collect()
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

/// Verify the live project still matches one side of `change` and substitute
/// the other. `forward == false` is undo (expect `after`, write `before`);
/// `true` is redo. Revalidates afterwards so a stale/foreign entry refuses
/// rather than corrupting the project.
fn substitute(project: &mut Project, change: &Change, forward: bool) -> Result<(), EditError> {
    match change {
        Change::Track(c) => substitute_topology(project, c, forward)?,
        Change::Assets(c) => substitute_assets(project, c, forward)?,
    }
    project.validate().map_err(EditError::from)
}

/// Verify the live track matches one side of `change` (by segment id and
/// frame counts) and substitute the other.
fn substitute_topology(
    project: &mut Project,
    change: &TrackChange,
    forward: bool,
) -> Result<(), EditError> {
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
    let live = snapshot_track(track);
    if live != *expected {
        return Err(EditError::History(
            "timeline changed outside the undo stack".to_string(),
        ));
    }
    track.segments = write.iter().map(segment_from_atom).collect();
    Ok(())
}

/// Verify the live media pool matches one side of `change` (exact equality,
/// including probes) and substitute the other.
fn substitute_assets(
    project: &mut Project,
    change: &AssetChange,
    forward: bool,
) -> Result<(), EditError> {
    let (expected, write) = if forward {
        (&change.before, &change.after)
    } else {
        (&change.after, &change.before)
    };
    if project.media != *expected {
        return Err(EditError::History(
            "media pool changed outside the undo stack".to_string(),
        ));
    }
    project.media = write.clone();
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

/// The track at `ti`, or `TrackNotFound` — `*_on` ops take a caller-supplied
/// index, so a stale/out-of-range one must error, not panic.
fn track_at(project: &Project, ti: usize) -> Result<&Track, EditError> {
    project
        .timeline
        .tracks
        .get(ti)
        .ok_or(EditError::TrackNotFound(ti))
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

/// Frame label for user-facing notices (`f19`) so messages read in frames
/// rather than long decimals. 0-based (frame 0 is the first frame),
/// matching the timecode and history. Without a frame rate it falls back
/// to seconds. (Not for ffmpeg arguments — those stay numeric seconds.)
fn stamp(project: &Project, t: &Seconds) -> String {
    match project.fps.as_ref() {
        Some(fps) => format!("f{}", (secs_f64(t) * secs_f64(fps)).round() as i64),
        None => format_seconds(t),
    }
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

/// Whether a split at timeline offset `off` inside a clip of `dur` leaves a
/// usable head and tail. A piece under one frame counts as "at that edge",
/// so a split can never strand a sub-frame sliver (which would then round to
/// a zero-frame history atom).
fn split_sides(project: &Project, dur: Seconds, off: Seconds) -> (bool, bool) {
    let usable = |d: Seconds| d > Seconds::from_integer(0) && at_least_a_frame(project, &d).is_ok();
    (usable(off), usable(dur - off))
}

/// The clip segment containing the (snapped) playhead.
fn clip_at(
    project: &Project,
    track: usize,
    t: &Seconds,
) -> Result<(usize, usize, Clip), EditError> {
    let ti = track;
    let track = project
        .timeline
        .tracks
        .get(ti)
        .ok_or(EditError::TrackNotFound(ti))?;
    let si = track.index_at(*t).ok_or(EditError::NoSegment)?;
    match &track.segments[si] {
        Segment::Clip(c) => Ok((ti, si, c.clone())),
        Segment::Gap(_) => Err(EditError::NoGap),
    }
}

/// The contiguous gap REGION containing the (snapped) playhead —
/// adjacent gaps read as one region. Returns
/// (track, first index, last index, region start, region end).
fn gap_region_at(
    project: &Project,
    track: usize,
    t: &Seconds,
) -> Result<(usize, usize, usize, Seconds, Seconds), EditError> {
    let ti = track;
    let track = project
        .timeline
        .tracks
        .get(ti)
        .ok_or(EditError::TrackNotFound(ti))?;
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

/// A timeline-space span expressed in a clip's source space
/// (`source = timeline × speed`). Edits are made in timeline time; a clip's
/// `asset_in/out` are source time, so every delta crossing that boundary
/// scales by the clip's speed.
fn source_span(c: &Clip, timeline: Seconds) -> Seconds {
    timeline * c.speed
}

/// Merge two newly-adjacent segments into a seamless whole, if they
/// form one: gaps always merge; clips merge iff same asset + contiguous
/// source (prev.asset_out == next.asset_in, exact) and the same speed. The
/// merged clip keeps the earlier segment's id (mirrors split, which keeps
/// the original id on the left half).
fn merge_pair(left: &Segment, right: &Segment) -> Option<Segment> {
    match (left, right) {
        (Segment::Gap(a), Segment::Gap(b)) => Some(Segment::Gap(Gap {
            duration: a.duration + b.duration,
        })),
        (Segment::Clip(a), Segment::Clip(b))
            if a.asset == b.asset && a.asset_out == b.asset_in && a.speed == b.speed =>
        {
            Some(Segment::Clip(Clip {
                id: a.id,
                asset: a.asset,
                asset_in: a.asset_in,
                asset_out: b.asset_out,
                speed: a.speed,
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
fn replace_with_many(
    track_idx: usize,
    index: usize,
    before: &Segment,
    pieces: Vec<Segment>,
) -> Vec<EditOp> {
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

// ---------------------------------------------- default-track wrappers
//
// The CLI and the single-track tests edit "the" video track. These thin
// wrappers resolve the first video track and delegate to the explicit
// `*_on` operations, which take a track index. The logic lives once, in
// the `_on` functions; the wrappers only pick the default.

/// Change a clip's playback speed. nle's timeline is a sequence, so a new
/// duration reflows every follower — **a speed change always ripples**. The
/// source range is untouched; only the timeline span changes.
pub fn set_speed_on(
    project: &Project,
    track: usize,
    clip_id: u32,
    speed: Seconds,
) -> Result<Batch, EditError> {
    if speed <= Seconds::from_integer(0) {
        return Err(EditError::BadSpeed(format_seconds(&speed)));
    }
    let ti = track;
    let tr = track_at(project, ti)?;
    let si = tr
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let c = match &tr.segments[si] {
        Segment::Clip(c) => c.clone(),
        Segment::Gap(_) => return Err(EditError::ClipNotFound(clip_id)),
    };
    // A fast retime can shrink the timeline span below a frame (a 1-frame
    // clip at 2×): every other duration-changing op refuses sub-frame
    // spans, so this one must too.
    let new_dur = (c.asset_out - c.asset_in) / speed;
    at_least_a_frame(project, &new_dur)?;
    let before = Segment::Clip(c.clone());
    let after = Segment::Clip(Clip { speed, ..c });
    Ok(Batch {
        ops: vec![EditOp::Replace {
            track: ti,
            index: si,
            before,
            after,
        }],
        desc: format!("speed clip #{} to {}x", clip_id, format_seconds(&speed)),
    })
}

pub fn set_speed(project: &Project, clip_id: u32, speed: Seconds) -> Result<Batch, EditError> {
    set_speed_on(project, video_track_index(project)?, clip_id, speed)
}

/// Keep one track's clips inside their media and on the frame grid. The
/// source range is clamped to `[0, media]` (in-point ≥ 0, out-point ≤ the
/// media's duration), then the timeline duration is snapped to whole frames
/// (rounded, falling back to the floor when rounding would run past the media
/// end). A clip shorter than one frame is dropped.
/// `media` maps an asset id to its duration; `fps` is the timeline's frame
/// rate. Returns one note per adjusted or dropped clip.
///
/// Every write path needs this: a Kdenlive rate conform can round an out-point
/// a hair past the file's end, and `reconstruct` snaps a rendered file's end up
/// to the project grid. Either way a clip would read past EOF (the export falls
/// short) and leave a fractional boundary for `snap` to land on the wrong side
/// of.
pub fn clamp_to_media(
    track: &mut Track,
    media: &dyn Fn(u32) -> Option<Seconds>,
    fps: Seconds,
) -> Vec<String> {
    let mut notes = Vec::new();
    if fps <= Seconds::from_integer(0) {
        return notes;
    }
    let zero = Seconds::from_integer(0);
    let frame_secs = |n: i64| Seconds::new(n, 1) / fps;
    track.segments.retain_mut(|seg| {
        let Segment::Clip(c) = seg else {
            return true;
        };
        // A non-positive speed has no sane clamp — leave it for `validate()`
        // to reject rather than dividing by zero here.
        if c.speed <= zero {
            return true;
        }
        let Some(d) = media(c.asset) else {
            return true;
        };
        // Keep the source range inside [0, media]: a negative in-point (an
        // EDL source TC before its offset, say) reads before the file start.
        let in_ = c.asset_in.max(zero);
        let out = c.asset_out.min(d);
        let mut frames = ((out - in_) / c.speed * fps).round().to_integer();
        let mut new_out = in_ + frame_secs(frames) * c.speed;
        if new_out > d {
            frames -= 1;
            new_out = in_ + frame_secs(frames) * c.speed;
        }
        if frames < 1 {
            notes.push(format!("clip #{}: holds no whole frame — dropped", c.id));
            return false;
        }
        if in_ != c.asset_in {
            notes.push(format!(
                "clip #{}: in {} → {}",
                c.id,
                format_seconds(&c.asset_in),
                format_seconds(&in_)
            ));
        }
        if new_out != c.asset_out {
            notes.push(format!(
                "clip #{}: out {} → {}",
                c.id,
                format_seconds(&c.asset_out),
                format_seconds(&new_out)
            ));
        }
        c.asset_in = in_;
        c.asset_out = new_out;
        true
    });
    notes
}

/// Snap a track's segment boundaries back onto the project frame grid and drop
/// any segment that collapses to under one frame (a "stump"), coalescing the
/// gaps a drop leaves adjacent. Boundaries snap *cumulatively*, so each cut
/// stays within half a frame of where it was rather than drifting with the sum
/// of per-segment rounding. Returns the cleaned segments and how many were
/// dropped.
///
/// Repairs a project that accumulated sub-frame drift (an older lossy undo
/// `Atom` could do this; fractional boundaries then made `snap` land edits on
/// the wrong segment). `nle purge-stumps` / `:purge`.
pub fn purge_stumps(track: &Track, fps: Seconds) -> (Vec<Segment>, usize) {
    if fps <= Seconds::from_integer(0) {
        return (track.segments.clone(), 0);
    }
    let frame = Seconds::from_integer(1) / fps;
    let snap = |t: Seconds| Seconds::new((t * fps).round().to_integer(), 1) / fps;
    let mut pos = Seconds::from_integer(0);
    let mut out: Vec<Segment> = Vec::with_capacity(track.segments.len());
    let mut dropped = 0usize;
    for seg in &track.segments {
        let end = snap(pos + seg.duration());
        let dur = end - pos;
        if dur < frame {
            dropped += 1;
            continue;
        }
        let cleaned = match seg {
            Segment::Clip(c) => Segment::Clip(Clip {
                id: c.id,
                asset: c.asset,
                asset_in: c.asset_in,
                asset_out: c.asset_in + dur * c.speed,
                speed: c.speed,
            }),
            Segment::Gap(_) => Segment::Gap(Gap { duration: dur }),
        };
        // Dropping a stump can leave two gaps adjacent; fold them.
        if let (Segment::Gap(g), Some(Segment::Gap(prev))) = (&cleaned, out.last_mut()) {
            prev.duration += g.duration;
        } else {
            out.push(cleaned);
        }
        pos = end;
    }
    (out, dropped)
}

pub fn trim_in(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    trim_in_on(project, video_track_index(project)?, playhead)
}

pub fn trim_out(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    trim_out_on(project, video_track_index(project)?, playhead)
}

pub fn is_gap_at(project: &Project, playhead: f64) -> bool {
    video_track_index(project)
        .map(|ti| is_gap_at_on(project, ti, playhead))
        .unwrap_or(false)
}

pub fn ripple_gap_in(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    ripple_gap_in_on(project, video_track_index(project)?, playhead)
}

pub fn ripple_gap_out(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    ripple_gap_out_on(project, video_track_index(project)?, playhead)
}

pub fn trim_in_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    trim_in_ripple_on(project, video_track_index(project)?, playhead)
}

pub fn trim_out_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    trim_out_ripple_on(project, video_track_index(project)?, playhead)
}

pub fn split(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    split_on(project, video_track_index(project)?, playhead)
}

pub fn delete_under(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    delete_under_on(project, video_track_index(project)?, playhead)
}

pub fn delete_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    delete_ripple_on(project, video_track_index(project)?, playhead)
}

pub fn delete_clip_ripple_by_id(project: &Project, clip_id: u32) -> Result<Batch, EditError> {
    delete_clip_ripple_by_id_on(project, video_track_index(project)?, clip_id)
}

pub fn paste(project: &Project, snapshot: &Segment, playhead: f64) -> Result<Batch, EditError> {
    paste_on(project, video_track_index(project)?, snapshot, playhead)
}

pub fn insert_gap(project: &Project, playhead: f64, frames: i64) -> Result<Batch, EditError> {
    insert_gap_on(project, video_track_index(project)?, playhead, frames)
}

pub fn paste_ripple(
    project: &Project,
    snapshot: &Segment,
    playhead: f64,
) -> Result<Batch, EditError> {
    paste_ripple_on(project, video_track_index(project)?, snapshot, playhead)
}

pub fn delete_segment(project: &Project, index: usize, ripple: bool) -> Result<Batch, EditError> {
    delete_segment_on(project, video_track_index(project)?, index, ripple)
}

pub fn trim_consume_target(project: &Project, playhead: f64, out: bool) -> Option<usize> {
    trim_consume_target_on(project, video_track_index(project).ok()?, playhead, out)
}

pub fn delete_clip_by_id(project: &Project, clip_id: u32) -> Result<Batch, EditError> {
    delete_clip_by_id_on(project, video_track_index(project)?, clip_id)
}

pub fn extend_edge(
    project: &Project,
    clip_id: u32,
    head: bool,
    playhead: f64,
) -> Result<Batch, EditError> {
    extend_edge_on(
        project,
        video_track_index(project)?,
        clip_id,
        head,
        playhead,
    )
}

pub fn extend_gap_end(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    extend_gap_end_on(project, video_track_index(project)?, playhead)
}

pub fn extend_gap_tail(
    project: &Project,
    index: usize,
    playhead: f64,
    ripple: bool,
) -> Result<Batch, EditError> {
    extend_gap_tail_on(
        project,
        video_track_index(project)?,
        index,
        playhead,
        ripple,
    )
}

pub fn extend_gap_head(
    project: &Project,
    index: usize,
    playhead: f64,
    ripple: bool,
) -> Result<Batch, EditError> {
    extend_gap_head_on(
        project,
        video_track_index(project)?,
        index,
        playhead,
        ripple,
    )
}

pub fn absorb_outward(
    project: &Project,
    clip_id: u32,
    head: bool,
    playhead: f64,
) -> Result<Batch, EditError> {
    absorb_outward_on(
        project,
        video_track_index(project)?,
        clip_id,
        head,
        playhead,
    )
}

// ------------------------------------------------------------ operations

/// `i` — trim the in-edge of whatever is under the playhead:
/// - clip: its head is cut at the playhead and a gap of the trimmed
///   material takes its place (everything else stays put);
/// - gap: the previous clip's tail extends into it (material permitting);
///   a leading gap shortens from the front.
pub fn trim_in_on(project: &Project, track: usize, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    let si = track.index_at(t).ok_or(EditError::NoSegment)?;

    if let Segment::Clip(c) = &track.segments[si] {
        let seg_start = track.start_of(si);
        let delta = t - seg_start;
        if delta <= Seconds::from_integer(0) {
            return Err(EditError::NothingToTrim(stamp(project, &t)));
        }
        let after = Clip {
            id: c.id,
            asset: c.asset,
            asset_in: c.asset_in + source_span(c, delta),
            asset_out: c.asset_out,
            speed: c.speed,
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
            && let Segment::Gap(g) = &track.segments[si - 1]
        {
            ops.push(EditOp::Replace {
                track: ti,
                index: si - 1,
                before: Segment::Gap(g.clone()),
                after: Segment::Gap(Gap {
                    duration: g.duration + delta,
                }),
            });
            return Ok(Batch {
                ops,
                desc: format!("trim in to {}", stamp(project, &t)),
            });
        }
        if let Some(g) = gap_piece(delta) {
            ops.push(EditOp::Insert {
                track: ti,
                index: si,
                segment: g,
            });
        }
        return Ok(Batch {
            ops,
            desc: format!("trim in to {}", stamp(project, &t)),
        });
    }

    // Gap region: [first..=last] are Gap segments; the playhead trims the
    // region's in-edge.
    let (ti, first, last, rs, _re) = gap_region_at(project, ti, &t)?;
    let delta = t - rs;
    if delta <= Seconds::from_integer(0) {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    let mut ops = Vec::new();

    // The previous clip's tail extends into the region (if one exists).
    if first > 0 {
        let (pi, prev) = match &track.segments[first - 1] {
            Segment::Clip(c) => (first - 1, c.clone()),
            Segment::Gap(_) => unreachable!("region merges adjacent gaps"),
        };
        let mut after = prev.clone();
        after.asset_out = prev.asset_out + source_span(&prev, delta);
        let asset = project
            .asset(after.asset)
            .ok_or(EditError::ClipNotFound(after.asset))?;
        if let Some(d) = asset.duration()
            && after.asset_out > d
        {
            return Err(EditError::BeyondAsset {
                id: prev.id,
                out: stamp(project, &after.asset_out),
                duration: stamp(project, &d),
            });
        }
        ops.push(EditOp::Replace {
            track: ti,
            index: pi,
            before: Segment::Clip(prev),
            after: Segment::Clip(after),
        });
    }

    // The region loses `delta` from its front.
    ops.extend(shrink_gap_region(
        ti,
        &track.segments,
        first,
        last,
        delta,
        true,
    ));
    Ok(Batch {
        ops,
        desc: format!("trim gap in to {}", stamp(project, &t)),
    })
}

/// `o` — trim the out-edge of whatever is under the playhead:
/// - clip: its tail is cut at the playhead and a gap of the trimmed
///   material takes its place (everything else stays put);
/// - gap: the next clip's head extends back into it (material
///   permitting); a trailing gap shortens to the playhead.
pub fn trim_out_on(project: &Project, track: usize, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    let si = track.index_at(t).ok_or(EditError::NoSegment)?;

    if let Segment::Clip(c) = &track.segments[si] {
        let seg_start = track.start_of(si);
        let new_len = t - seg_start;
        if new_len <= Seconds::from_integer(0) {
            return Err(EditError::NothingToTrim(stamp(project, &t)));
        }
        let after = Clip {
            id: c.id,
            asset: c.asset,
            asset_in: c.asset_in,
            asset_out: c.asset_in + source_span(c, new_len),
            speed: c.speed,
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
            && let Segment::Gap(g) = &track.segments[si + 1]
        {
            ops.push(EditOp::Replace {
                track: ti,
                index: si + 1,
                before: Segment::Gap(g.clone()),
                after: Segment::Gap(Gap {
                    duration: g.duration + trimmed,
                }),
            });
            return Ok(Batch {
                ops,
                desc: format!("trim out to {}", stamp(project, &t)),
            });
        }
        if let Some(g) = gap_piece(trimmed) {
            ops.push(EditOp::Insert {
                track: ti,
                index: si + 1,
                segment: g,
            });
        }
        return Ok(Batch {
            ops,
            desc: format!("trim out to {}", stamp(project, &t)),
        });
    }

    // Gap region: the playhead trims the region's out-edge.
    let (ti, first, last, _rs, re) = gap_region_at(project, ti, &t)?;
    let delta = re - t;
    if delta <= Seconds::from_integer(0) {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    let mut ops = Vec::new();

    // The next clip's head extends back into the region (if one exists).
    if last + 1 < track.segments.len() {
        let (ni, next) = match &track.segments[last + 1] {
            Segment::Clip(c) => (last + 1, c.clone()),
            Segment::Gap(_) => unreachable!("region merges adjacent gaps"),
        };
        let mut after = next.clone();
        after.asset_in = next.asset_in - source_span(&next, delta);
        if after.asset_in < Seconds::from_integer(0) {
            return Err(EditError::BeforeAsset {
                id: next.id,
                needed: stamp(project, &source_span(&next, delta)),
                ain: stamp(project, &next.asset_in),
            });
        }
        ops.push(EditOp::Replace {
            track: ti,
            index: ni,
            before: Segment::Clip(next),
            after: Segment::Clip(after),
        });
    }

    // The region loses `delta` from its back.
    ops.extend(shrink_gap_region(
        ti,
        &track.segments,
        first,
        last,
        delta,
        false,
    ));
    Ok(Batch {
        ops,
        desc: format!("trim gap out to {}", stamp(project, &t)),
    })
}

/// Shrink a gap region `[first..=last]` by `delta`, eating from the front
/// (`front`) or the back. Fully-eaten gap segments are removed and a
/// survivor shortened; the net effect is the gap losing `delta` with the
/// followers sliding left.
fn shrink_gap_region(
    ti: usize,
    segs: &[Segment],
    first: usize,
    last: usize,
    delta: Seconds,
    front: bool,
) -> Vec<EditOp> {
    let mut ops = Vec::new();
    if front {
        let mut remaining = delta;
        for seg in &segs[first..=last] {
            let g_dur = match seg {
                Segment::Gap(g) => g.duration,
                Segment::Clip(_) => unreachable!("region is gaps"),
            };
            if g_dur > remaining {
                ops.push(EditOp::Replace {
                    track: ti,
                    index: first,
                    before: Segment::Gap(Gap { duration: g_dur }),
                    after: Segment::Gap(Gap {
                        duration: g_dur - remaining,
                    }),
                });
                break;
            }
            ops.push(EditOp::Remove {
                track: ti,
                index: first,
                segment: Segment::Gap(Gap { duration: g_dur }),
            });
            remaining -= g_dur;
        }
    } else {
        let mut remaining = delta;
        for k in (first..=last).rev() {
            let g = match &segs[k] {
                Segment::Gap(g) => g.clone(),
                Segment::Clip(_) => unreachable!("region is gaps"),
            };
            let dur = g.duration;
            if dur > remaining {
                ops.push(EditOp::Replace {
                    track: ti,
                    index: k,
                    before: Segment::Gap(g.clone()),
                    after: Segment::Gap(Gap {
                        duration: dur - remaining,
                    }),
                });
                break;
            }
            ops.push(EditOp::Remove {
                track: ti,
                index: k,
                segment: Segment::Gap(g),
            });
            remaining -= dur;
        }
    }
    ops
}

/// Is the (snapped) playhead inside a gap?
pub fn is_gap_at_on(project: &Project, track: usize, playhead: f64) -> bool {
    match snap(project, playhead) {
        Ok(t) => gap_region_at(project, track, &t).is_ok(),
        Err(_) => false,
    }
}

/// `i`/`I` on a gap: ripple-trim the region's in-edge. Unlike `trim_in`,
/// no neighbouring clip grows into the gap — it simply loses `delta` and
/// the followers slide left.
pub fn ripple_gap_in_on(
    project: &Project,
    track: usize,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let (ti, first, last, rs, _re) = gap_region_at(project, track, &t)?;
    let delta = t - rs;
    if delta <= Seconds::from_integer(0) {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    let ops = shrink_gap_region(
        ti,
        &track_at(project, ti)?.segments,
        first,
        last,
        delta,
        true,
    );
    Ok(Batch {
        ops,
        desc: format!("ripple trim gap in to {}", stamp(project, &t)),
    })
}

/// `o`/`O` on a gap: ripple-trim the region's out-edge, the mirror of
/// `ripple_gap_in`.
pub fn ripple_gap_out_on(
    project: &Project,
    track: usize,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let (ti, first, last, _rs, re) = gap_region_at(project, track, &t)?;
    let delta = re - t;
    if delta <= Seconds::from_integer(0) {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    let ops = shrink_gap_region(
        ti,
        &track_at(project, ti)?.segments,
        first,
        last,
        delta,
        false,
    );
    Ok(Batch {
        ops,
        desc: format!("ripple trim gap out to {}", stamp(project, &t)),
    })
}

/// `I` — ripple trim-in: the clip's head is cut at the playhead and the
/// followers slide left (no gap is left behind). On a gap, identical to
/// `i` (gap trims are inherently ripple).
pub fn trim_in_ripple_on(
    project: &Project,
    track: usize,
    playhead: f64,
) -> Result<Batch, EditError> {
    // Ripple strips the leftover-gap ops, but only when the playhead is
    // on a clip (gap-region trims are inherently ripple and keep theirs).
    let on_clip = clip_at(project, track, &snap(project, playhead)?).is_ok();
    match trim_in_on(project, track, playhead) {
        Ok(mut batch) => {
            if on_clip {
                batch.ops.retain(|op| {
                    !matches!(
                        op,
                        EditOp::Insert {
                            segment: Segment::Gap(_),
                            ..
                        } | EditOp::Replace {
                            before: Segment::Gap(_),
                            ..
                        }
                    )
                });
            }
            Ok(batch)
        }
        Err(e) => Err(e),
    }
}

/// `O` — ripple trim-out: the clip's tail is cut at the playhead and the
/// followers slide left (no gap left behind). On a gap, identical to `o`.
pub fn trim_out_ripple_on(
    project: &Project,
    track: usize,
    playhead: f64,
) -> Result<Batch, EditError> {
    let on_clip = clip_at(project, track, &snap(project, playhead)?).is_ok();
    match trim_out_on(project, track, playhead) {
        Ok(mut batch) => {
            if on_clip {
                batch.ops.retain(|op| {
                    !matches!(
                        op,
                        EditOp::Insert {
                            segment: Segment::Gap(_),
                            ..
                        } | EditOp::Replace {
                            before: Segment::Gap(_),
                            ..
                        }
                    )
                });
            }
            Ok(batch)
        }
        Err(e) => Err(e),
    }
}

/// `s` — split the clip under the playhead into two clips.
pub fn split_on(project: &Project, track: usize, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let (ti, si, c) = clip_at(project, track, &t)?;
    let seg_start = track_at(project, ti)?.start_of(si);
    let left_len = t - seg_start;
    if left_len <= Seconds::from_integer(0) || left_len >= c.duration() {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    at_least_a_frame(project, &left_len)?;
    at_least_a_frame(project, &(c.duration() - left_len))?;

    let left = Segment::Clip(Clip {
        id: c.id,
        asset: c.asset,
        asset_in: c.asset_in,
        asset_out: c.asset_in + source_span(&c, left_len),
        speed: c.speed,
    });
    let right = Segment::Clip(Clip {
        id: project.next_clip_id(),
        asset: c.asset,
        asset_in: c.asset_in + source_span(&c, t - seg_start),
        asset_out: c.asset_out,
        speed: c.speed,
    });
    Ok(Batch {
        ops: replace_with_many(ti, si, &Segment::Clip(c.clone()), vec![left, right]),
        desc: format!("split clip #{} at {}", c.id, stamp(project, &t)),
    })
}

/// `dd` — delete what's under the playhead, leaving a gap where a clip
/// was (the timeline end is preserved); a gap under the playhead is
/// removed (shrinking the timeline).
pub fn delete_under_on(project: &Project, track: usize, playhead: f64) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let (ti, si) = {
        let ti = track;
        let si = track_at(project, ti)?
            .index_at(t)
            .ok_or(EditError::NoSegment)?;
        (ti, si)
    };
    let seg = track_at(project, ti)?.segments[si].clone();
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
            let segs = &track_at(project, ti)?.segments;
            let left_gap = si > 0 && matches!(segs[si - 1], Segment::Gap(_));
            let right_gap = si + 1 < segs.len() && matches!(segs[si + 1], Segment::Gap(_));
            let new_gap = Segment::Gap(Gap { duration: dur });
            match (left_gap, right_gap) {
                (true, true) => {
                    let total =
                        segs[si - 1].duration() + new_gap.duration() + segs[si + 1].duration();
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si - 1,
                        before: segs[si - 1].clone(),
                        after: Segment::Gap(Gap { duration: total }),
                    });
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si,
                        segment: new_gap,
                    });
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si,
                        segment: segs[si + 1].clone(),
                    });
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
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si,
                        segment: new_gap,
                    });
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
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si + 1,
                        segment: segs[si + 1].clone(),
                    });
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
            let segs = &track_at(project, ti)?.segments;
            if si > 0
                && si + 1 < segs.len()
                && let Some(merged) = merge_pair(&segs[si - 1], &segs[si + 1])
            {
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
pub fn delete_ripple_on(
    project: &Project,
    track: usize,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let si = track_at(project, ti)?
        .index_at(t)
        .ok_or(EditError::NoSegment)?;
    delete_segment_ripple(project, ti, si)
}

/// Ripple-delete clip `clip_id` by id. Extend mode commits at a consumed
/// edge against the armed clip, not whatever the playhead's boundary
/// resolves to.
pub fn delete_clip_ripple_by_id_on(
    project: &Project,
    track: usize,
    clip_id: u32,
) -> Result<Batch, EditError> {
    let ti = track;
    let si = track_at(project, ti)?
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    delete_segment_ripple(project, ti, si)
}

fn delete_segment_ripple(project: &Project, ti: usize, si: usize) -> Result<Batch, EditError> {
    let seg = track_at(project, ti)?.segments[si].clone();
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
    let segs = &track_at(project, ti)?.segments;
    if si > 0
        && si + 1 < segs.len()
        && let Some(merged) = merge_pair(&segs[si - 1], &segs[si + 1])
    {
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

/// The pasted piece for a clipboard segment: a clip takes a fresh id, a
/// gap just carries its duration.
fn pasted_piece(snapshot: &Segment, new_id: u32) -> Segment {
    match snapshot {
        Segment::Clip(c) => Segment::Clip(Clip {
            id: new_id,
            asset: c.asset,
            asset_in: c.asset_in,
            asset_out: c.asset_out,
            speed: c.speed,
        }),
        Segment::Gap(g) => Segment::Gap(Gap {
            duration: g.duration,
        }),
    }
}

/// Pasting a gap against an existing gap extends it instead of making a
/// second, adjacent gap segment. Delegates to `extend_gap_tail`, so the
/// ripple flag decides whether followers shift (`P`) or are overwritten
/// (`p`). `None` when no gap ends at the playhead.
fn extend_gap_ending_at(
    project: &Project,
    track: usize,
    playhead: f64,
    extra: Seconds,
    ripple: bool,
) -> Result<Option<Batch>, EditError> {
    let Some(si) = trim_consume_target_on(project, track, playhead, false) else {
        return Ok(None);
    };
    let ti = track;
    if !matches!(
        track_at(project, ti)?.segments.get(si),
        Some(Segment::Gap(_))
    ) {
        return Ok(None);
    }
    let t = snap(project, playhead)?;
    let end = secs_f64(&t) + secs_f64(&extra);
    extend_gap_tail_on(project, track, si, end, ripple).map(Some)
}

/// `p` — replace-paste: drop the clipboard at the playhead, overwriting
/// whatever its span covers. Covered gaps vanish, wholly-covered clips
/// are removed, and a straddled follower is trimmed at its near edge;
/// past the track end the timeline simply grows. The pasted clip keeps
/// the yanked source range (splitting a clip hands the surviving right
/// half a fresh id). A yanked GAP pastes empty space of the same length.
pub fn paste_on(
    project: &Project,
    track: usize,
    snapshot: &Segment,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    // A gap pasted at the end of a gap extends it (replace: the covered
    // span is overwritten, followers do not shift).
    if let Segment::Gap(pg) = snapshot
        && let Some(batch) = extend_gap_ending_at(project, ti, playhead, pg.duration, false)?
    {
        return Ok(batch);
    }
    let dur = snapshot.duration();
    let pasted = pasted_piece(snapshot, project.next_clip_id());

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
            segment: pasted.clone(),
        });
        return Ok(Batch {
            ops,
            desc: format!("paste at {}", stamp(project, &t)),
        });
    }

    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let left_len = t - track.start_of(si);
    // A left remainder under one frame is treated as "at the edge" so it
    // can't strand a sub-frame sliver.
    let left_ok =
        left_len > Seconds::from_integer(0) && at_least_a_frame(project, &left_len).is_ok();
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
    let remove_upto = straddle
        .map(|(ei, _)| ei + 1)
        .unwrap_or(track.segments.len());

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
            if rem > Seconds::from_integer(0) && at_least_a_frame(project, &rem).is_ok() {
                match &track.segments[ei] {
                    Segment::Gap(_) => Some(Segment::Gap(Gap { duration: rem })),
                    Segment::Clip(c) => {
                        // A split keeps the head's id on the left, so the
                        // tail needs a fresh id only when the same clip
                        // supplied both pieces.
                        let id = if ei == si && left_ok {
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
                            asset_in: c.asset_in + source_span(c, off),
                            asset_out: c.asset_out,
                            speed: c.speed,
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
    if left_ok {
        match &seg {
            Segment::Gap(_) => pieces.push(Segment::Gap(Gap { duration: left_len })),
            Segment::Clip(c) => pieces.push(Segment::Clip(Clip {
                asset_out: c.asset_in + source_span(c, left_len),
                ..c.clone()
            })),
        }
    }
    pieces.push(pasted.clone());
    if let Some(r) = right {
        pieces.push(r);
    }

    // Coalesce adjacent gaps the paste produced (a pasted gap butting an
    // existing one).
    let pieces: Vec<Segment> = {
        let mut merged: Vec<Segment> = Vec::with_capacity(pieces.len());
        for piece in pieces {
            if let Segment::Gap(g) = &piece
                && let Some(Segment::Gap(prev)) = merged.last_mut()
            {
                let total = prev.duration + g.duration;
                prev.duration = total;
                continue;
            }
            merged.push(piece);
        }
        merged
    };

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

    let mut desc = format!("paste at {}", stamp(project, &t));
    if !notes.is_empty() {
        desc += &format!(" ({})", notes.join(", "));
    }
    Ok(Batch { ops, desc })
}

/// `_` — insert `frames` of gap at the playhead. Over a clip it splits
/// the clip and inserts the gap between the halves (the tail slides
/// right); over a gap it lengthens that gap; past the end the void plus
/// the new gap. Everything grows by `frames`.
pub fn insert_gap_on(
    project: &Project,
    track: usize,
    playhead: f64,
    frames: i64,
) -> Result<Batch, EditError> {
    if frames < 1 {
        return Err(EditError::TooShort);
    }
    let t = snap(project, playhead)?;
    let fps = project.fps.ok_or(EditError::NoFps)?;
    let gap = Segment::Gap(Gap {
        duration: Seconds::new(frames, 1) / fps,
    });
    let ti = track;
    let track = track_at(project, ti)?;

    // A gap ending exactly at the playhead absorbs the insert rather than
    // spawning a second, adjacent gap (the rule `paste` already uses). This
    // also covers a trailing gap at the track end. Ripple growth = followers
    // shift right, which is what inserting frames means.
    if let Some(mut batch) = extend_gap_ending_at(project, ti, playhead, gap.duration(), true)? {
        batch.desc = format!("insert gap at {}", stamp(project, &t));
        return Ok(batch);
    }

    // At/past the end: bridge the void (if any), then the new gap.
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
            segment: gap,
        });
        return Ok(Batch {
            ops,
            desc: format!("insert gap at {}", stamp(project, &t)),
        });
    }

    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let seg_start = track.start_of(si);
    let off = t - seg_start;

    match &seg {
        // Over a gap: add to it.
        Segment::Gap(g) => Ok(Batch {
            ops: replace_with_many(
                ti,
                si,
                &seg,
                vec![Segment::Gap(Gap {
                    duration: g.duration + gap.duration(),
                })],
            ),
            desc: format!("insert gap at {}", stamp(project, &t)),
        }),
        // Over a clip: split it at the playhead, gap between the halves.
        Segment::Clip(c) => {
            // A split within a frame of either edge is that edge: no slivers.
            let (head_ok, tail_ok) = split_sides(project, c.duration(), off);
            let pid = project.next_clip_id();
            let mut pieces: Vec<Segment> = Vec::new();
            if head_ok {
                pieces.push(Segment::Clip(Clip {
                    id: c.id,
                    asset: c.asset,
                    asset_in: c.asset_in,
                    asset_out: c.asset_in + source_span(c, off),
                    speed: c.speed,
                }));
            }
            pieces.push(gap);
            if tail_ok {
                // The tail keeps the id when the head is absent (insert-before),
                // else gets a fresh one (a split).
                pieces.push(Segment::Clip(Clip {
                    id: if head_ok { pid } else { c.id },
                    asset: c.asset,
                    asset_in: c.asset_in + source_span(c, off),
                    asset_out: c.asset_out,
                    speed: c.speed,
                }));
            }
            Ok(Batch {
                ops: replace_with_many(ti, si, &seg, pieces),
                desc: format!("insert gap at {}", stamp(project, &t)),
            })
        }
    }
}

/// `P` — ripple paste: insert the clipboard at the playhead, splitting
/// whatever is there (clip or gap) and growing the timeline by the
/// payload's duration. A clipboard gap inserts empty space; a clipboard
/// clip inserts the clip. At a cut the insert lands before the clip
/// starting there.
pub fn paste_ripple_on(
    project: &Project,
    track: usize,
    snapshot: &Segment,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    // A gap pasted at the end of a gap extends it and pushes followers
    // right, rather than making a second adjacent gap segment.
    if let Segment::Gap(pg) = snapshot
        && let Some(batch) = extend_gap_ending_at(project, ti, playhead, pg.duration, true)?
    {
        return Ok(batch);
    }
    let what = match snapshot {
        Segment::Clip(_) => format!("clip #{}", project.next_clip_id()),
        Segment::Gap(_) => "gap".to_string(),
    };

    // At or past the track end: the void becomes a gap segment (when
    // there's distance to cover), then the payload lands on the playhead.
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
            segment: pasted_piece(snapshot, project.next_clip_id()),
        });
        return Ok(Batch {
            ops,
            desc: format!("ripple paste at {} — {what} inserted", stamp(project, &t)),
        });
    }
    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let seg_start = track.start_of(si);

    // A gap pasted into a gap just lengthens it.
    if let (Segment::Gap(g), Segment::Gap(pg)) = (&seg, snapshot) {
        return Ok(Batch {
            ops: replace_with_many(
                ti,
                si,
                &seg,
                vec![Segment::Gap(Gap {
                    duration: g.duration + pg.duration,
                })],
            ),
            desc: format!("ripple paste at {} — gap inserted", stamp(project, &t)),
        });
    }

    // The containing segment splits at t; the paste rides between the
    // pieces. Inserting shifts every follower automatically.
    let pieces: Vec<Segment> = match &seg {
        Segment::Clip(c) => {
            let head_len = t - seg_start;
            let (head_ok, tail_ok) = split_sides(project, c.duration(), head_len);
            let pid = project.next_clip_id();
            let mut pieces = Vec::new();
            if head_ok {
                pieces.push(Segment::Clip(Clip {
                    id: c.id,
                    asset: c.asset,
                    asset_in: c.asset_in,
                    asset_out: c.asset_in + source_span(c, head_len),
                    speed: c.speed,
                }));
            }
            pieces.push(pasted_piece(snapshot, pid));
            if tail_ok {
                // The right half is a new clip on a split, or keeps the
                // original's identity on an insert-before.
                let id = if head_ok { pid + 1 } else { c.id };
                pieces.push(Segment::Clip(Clip {
                    id,
                    asset: c.asset,
                    asset_in: c.asset_in + source_span(c, head_len),
                    asset_out: c.asset_out,
                    speed: c.speed,
                }));
            }
            pieces
        }
        Segment::Gap(g) => {
            let mut pieces = Vec::new();
            if let Some(l) = gap_piece(t - seg_start) {
                pieces.push(l);
            }
            pieces.push(pasted_piece(snapshot, project.next_clip_id()));
            if let Some(r) = gap_piece(g.duration - (t - seg_start)) {
                pieces.push(r);
            }
            pieces
        }
    };
    Ok(Batch {
        ops: replace_with_many(ti, si, &seg, pieces),
        desc: format!("ripple paste at {} — {what} inserted", stamp(project, &t)),
    })
}

/// Delete the segment at `index`: a gap, or a clip when `ripple`, is
/// removed whole with its neighbours merged; a plain clip delete leaves a
/// gap behind (dd semantics).
pub fn delete_segment_on(
    project: &Project,
    track: usize,
    index: usize,
    ripple: bool,
) -> Result<Batch, EditError> {
    let ti = track;
    match track_at(project, ti)?.segments.get(index) {
        Some(Segment::Gap(_)) | Some(Segment::Clip(_)) if ripple => {
            delete_segment_ripple(project, ti, index)
        }
        Some(Segment::Clip(c)) => delete_clip_by_id_on(project, ti, c.id),
        Some(Segment::Gap(_)) => delete_segment_ripple(project, ti, index),
        None => Err(EditError::SegmentNotFound(index)),
    }
}

/// The segment an edge-trim at the playhead would consume completely (so
/// the trim should delete it instead). `out` (`o`/`O`) targets the
/// segment STARTING at the playhead; `!out` (`i`/`I`) the segment ENDING
/// there — at a shared cut those are the two different neighbours.
pub fn trim_consume_target_on(
    project: &Project,
    track: usize,
    playhead: f64,
    out: bool,
) -> Option<usize> {
    let t = snap(project, playhead).ok()?;
    let track = project.timeline.tracks.get(track)?;
    if out {
        let si = track.index_at(t)?;
        (track.start_of(si) == t).then_some(si)
    } else {
        let mut acc = Seconds::from_integer(0);
        for (i, s) in track.segments.iter().enumerate() {
            acc += s.duration();
            if acc == t {
                return Some(i);
            }
        }
        None
    }
}

/// CLI `remove <id>`: replace the clip with a gap (dd semantics).
pub fn delete_clip_by_id_on(
    project: &Project,
    track: usize,
    clip_id: u32,
) -> Result<Batch, EditError> {
    let ti = track;
    let track = track_at(project, ti)?;
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
            let segs = &track_at(project, ti)?.segments;
            let left_gap = si > 0 && matches!(segs[si - 1], Segment::Gap(_));
            let right_gap = si + 1 < segs.len() && matches!(segs[si + 1], Segment::Gap(_));
            let new_gap = Segment::Gap(Gap { duration: dur });
            match (left_gap, right_gap) {
                (true, true) => {
                    let total =
                        segs[si - 1].duration() + new_gap.duration() + segs[si + 1].duration();
                    ops.push(EditOp::Replace {
                        track: ti,
                        index: si - 1,
                        before: segs[si - 1].clone(),
                        after: Segment::Gap(Gap { duration: total }),
                    });
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si,
                        segment: new_gap,
                    });
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si,
                        segment: segs[si + 1].clone(),
                    });
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
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si,
                        segment: new_gap,
                    });
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
                    ops.push(EditOp::Remove {
                        track: ti,
                        index: si + 1,
                        segment: segs[si + 1].clone(),
                    });
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
pub fn extend_edge_on(
    project: &Project,
    track: usize,
    clip_id: u32,
    head: bool,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    let si = track
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let c = match &track.segments[si] {
        Segment::Clip(c) => c.clone(),
        Segment::Gap(_) => return Err(EditError::ClipNotFound(clip_id)),
    };
    let edge = track.start_of(si)
        + if head {
            Seconds::from_integer(0)
        } else {
            c.duration()
        };
    let after = if head {
        if t >= edge {
            return Err(EditError::NothingToTrim(stamp(project, &t)));
        }
        let delta = edge - t;
        let asset_in = c.asset_in - source_span(&c, delta);
        if asset_in < Seconds::from_integer(0) {
            return Err(EditError::BeforeAsset {
                id: c.id,
                needed: stamp(project, &source_span(&c, delta)),
                ain: stamp(project, &c.asset_in),
            });
        }
        Clip { asset_in, ..c }
    } else {
        if t <= edge {
            return Err(EditError::NothingToTrim(stamp(project, &t)));
        }
        let asset_out = c.asset_out + source_span(&c, t - edge);
        let asset = project
            .asset(c.asset)
            .ok_or(EditError::ClipNotFound(c.asset))?;
        if let Some(d) = asset.duration()
            && asset_out > d
        {
            return Err(EditError::BeyondAsset {
                id: c.id,
                out: stamp(project, &asset_out),
                duration: stamp(project, &d),
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
        desc: format!("extend {side} of clip #{clip_id} to {}", stamp(project, &t)),
    })
}

/// Lengthen the trailing gap to reach `t` (free — gaps have no source
/// material). Used where the timeline itself grows into empty space.
pub fn extend_gap_end_on(
    project: &Project,
    track: usize,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    let last = track
        .segments
        .len()
        .checked_sub(1)
        .ok_or(EditError::NoSegment)?;
    let g = match &track.segments[last] {
        Segment::Gap(g) => g.clone(),
        Segment::Clip(_) => return Err(EditError::NoGap),
    };
    let gap_start = track.start_of(last);
    if t <= gap_start {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    Ok(Batch {
        ops: vec![EditOp::Replace {
            track: ti,
            index: last,
            before: Segment::Gap(g),
            after: Segment::Gap(Gap {
                duration: t - gap_start,
            }),
        }],
        desc: format!("extend gap to {}", stamp(project, &t)),
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
pub fn extend_gap_tail_on(
    project: &Project,
    track: usize,
    index: usize,
    playhead: f64,
    ripple: bool,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    let g = match track.segments.get(index) {
        Some(Segment::Gap(g)) => g.clone(),
        _ => return Err(EditError::NoGap),
    };
    let gap_start = track.start_of(index);
    if t <= gap_start {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
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
                after: Segment::Gap(Gap {
                    duration: t - gap_start,
                }),
            }],
            desc: format!("extend gap to {}", stamp(project, &t)),
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
        after: Segment::Gap(Gap {
            duration: t - gap_start,
        }),
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
            Straddle {
                index: index + 1,
                start: track.start_of(m),
                seg,
            },
            t,
            &mut ops,
            &mut absorbed,
        )?;
    }
    let mut desc = format!("extend gap to {}", stamp(project, &t));
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
pub fn extend_gap_head_on(
    project: &Project,
    track: usize,
    index: usize,
    playhead: f64,
    ripple: bool,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    let g = match track.segments.get(index) {
        Some(Segment::Gap(g)) => g.clone(),
        _ => return Err(EditError::NoGap),
    };
    let gap_start = track.start_of(index);
    let gap_end = gap_start + g.duration;
    if t > gap_start && t < gap_end {
        // Inward trim of the head: ripple. A gap's start is the prefix sum
        // of what precedes it, so it cannot move on its own — the gap just
        // loses `delta` and the followers slide left (never growing the
        // preceding clip).
        return ripple_gap_in_on(project, ti, playhead);
    }
    if t >= gap_start {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    // Ripple: insert empty space at the gap's head. The gap's start is the
    // prefix sum (anchored), so the inserted space merges into the gap —
    // its duration grows by `delta` and everything after shifts right; the
    // preceding segment is left untouched. (There is nowhere to shift the
    // prefix to, so a plain "push the predecessors left" is impossible.)
    if ripple {
        let delta = gap_start - t;
        return Ok(Batch {
            ops: vec![EditOp::Replace {
                track: ti,
                index,
                before: Segment::Gap(g.clone()),
                after: Segment::Gap(Gap {
                    duration: g.duration + delta,
                }),
            }],
            desc: format!("insert gap at {}", stamp(project, &t)),
        });
    }
    let mut ops = vec![EditOp::Replace {
        track: ti,
        index,
        before: Segment::Gap(g),
        after: Segment::Gap(Gap {
            duration: gap_end - t,
        }),
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
            Straddle {
                index: m,
                start: track.start_of(m),
                seg,
            },
            t,
            &mut ops,
            &mut absorbed,
        )?;
    }
    let mut desc = format!("extend gap head to {}", stamp(project, &t));
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
pub fn absorb_outward_on(
    project: &Project,
    track: usize,
    clip_id: u32,
    head: bool,
    playhead: f64,
) -> Result<Batch, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let track = track_at(project, ti)?;
    let si = track
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let c = match &track.segments[si] {
        Segment::Clip(c) => c.clone(),
        Segment::Gap(_) => return Err(EditError::ClipNotFound(clip_id)),
    };
    let start = track.start_of(si);
    let edge = start
        + if head {
            Seconds::from_integer(0)
        } else {
            c.duration()
        };

    // Extend the armed edge first (indices below stay valid throughout).
    let after = if head {
        if t >= edge {
            return Err(EditError::NothingToTrim(stamp(project, &t)));
        }
        let delta = edge - t;
        let asset_in = c.asset_in - source_span(&c, delta);
        if asset_in < Seconds::from_integer(0) {
            return Err(EditError::BeforeAsset {
                id: c.id,
                needed: stamp(project, &source_span(&c, delta)),
                ain: stamp(project, &c.asset_in),
            });
        }
        Clip {
            asset_in,
            ..c.clone()
        }
    } else {
        if t <= edge {
            return Err(EditError::NothingToTrim(stamp(project, &t)));
        }
        let asset_out = c.asset_out + source_span(&c, t - edge);
        let asset = project
            .asset(c.asset)
            .ok_or(EditError::ClipNotFound(c.asset))?;
        if let Some(d) = asset.duration()
            && asset_out > d
        {
            return Err(EditError::BeyondAsset {
                id: c.id,
                out: stamp(project, &asset_out),
                duration: stamp(project, &d),
            });
        }
        Clip {
            asset_out,
            ..c.clone()
        }
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
            trim_or_absorb_tail(
                project,
                ti,
                Straddle {
                    index: m,
                    start: track.start_of(m),
                    seg,
                },
                t,
                &mut ops,
                &mut absorbed,
            )?;
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
                Straddle {
                    index: si + 1,
                    start: track.start_of(m),
                    seg,
                },
                t,
                &mut ops,
                &mut absorbed,
            )?;
        }
    }

    let side = if head { "head" } else { "tail" };
    let mut desc = format!("extend {side} of clip #{clip_id} to {}", stamp(project, &t));
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
                after: Segment::Gap(Gap {
                    duration: start + g.duration - t,
                }),
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
                asset_in: c.asset_in + source_span(&c, t - start),
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
                after: Segment::Gap(Gap {
                    duration: g.duration - (t - start),
                }),
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
                asset_out: c.asset_in + source_span(&c, remainder),
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
        p.video = Some(VideoSettings {
            width: 320,
            height: 240,
        });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(1),
                asset_out: Seconds::from_integer(3),
                speed: Seconds::from_integer(1),
            }),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }),
            Segment::Clip(Clip {
                id: 2,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
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
        h.apply(&mut p, batch, 0).unwrap();
        // [Gap 1 @0..1] [Clip #1 [1..2) src 2..3] [Gap 1 @2..3] [Clip #2]
        let track = p.timeline.video_track().unwrap();
        assert!(
            matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
        let c = clip_by_id(&p, 1);
        assert_eq!(c.asset_in, Seconds::from_integer(2));
        assert_eq!(c.duration(), Seconds::from_integer(1));
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(
            clip_by_id(&p, 2).duration(),
            Seconds::from_integer(2),
            "follower unmoved"
        );
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
        h.apply(&mut p, batch, 0).unwrap();
        let c = clip_by_id(&p, 2);
        assert_eq!(c.asset_out, Seconds::from_integer(1), "src 0..1");
        assert_eq!(c.duration(), Seconds::from_integer(1));
        // the trimmed material's gap sits AFTER the clip (index 3)
        let track = p.timeline.video_track().unwrap();
        assert!(
            matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).asset_out, Seconds::from_integer(2));
    }

    #[test]
    fn i_against_gap_grows_it_instead_of_splitting() {
        let mut p = fixture(); // [Clip #1] [Gap 1] [Clip #2 [3..5)]
        let mut h = History::default();
        let batch = trim_in(&p, 4.0).unwrap(); // 1s off clip #2's head
        h.apply(&mut p, batch, 0).unwrap();
        // no new segment: the existing gap absorbed the trimmed second
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(
            matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
        );
        let c = clip_by_id(&p, 2);
        assert_eq!(c.asset_in, Seconds::from_integer(1));
        assert_eq!(c.duration(), Seconds::from_integer(1));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
        h.undo(&mut p).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(
            matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
    }

    #[test]
    fn o_against_gap_grows_it_instead_of_splitting() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_out(&p, 1.0).unwrap(); // 1s off clip #1's tail
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(
            matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
        );
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
        h.apply(&mut p, batch, 0).unwrap();
        // only the clip shortened: the neighboring gap is untouched
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(
            matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
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
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 3);
        assert!(
            matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
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
        h.apply(&mut p, batch, 0).unwrap();
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
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(
            clip_by_id(&p, 1).duration(),
            Seconds::new(1, 2),
            "head trimmed 1.5"
        );
        assert_eq!(
            clip_by_id(&p, 2).duration(),
            Seconds::from_integer(2),
            "source kept"
        );
        assert_eq!(p.timeline.end(), Seconds::new(7, 2), "shrunk by 1.5");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    }

    #[test]
    fn o_ripple_shrinks_and_slides_followers() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = trim_out_ripple(&p, 4.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
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
        h.apply(&mut p, batch, 0).unwrap();
        let c = clip_by_id(&p, 1);
        assert_eq!(
            c.asset_out,
            Seconds::new(7, 2),
            "tail grew 0.5 into the gap"
        );
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
            speed: Seconds::from_integer(1),
        });
        let mut h = History::default();
        let batch = trim_out(&p, 2.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let c = clip_by_id(&p, 2);
        assert_eq!(c.asset_in, Seconds::new(1, 2), "head grew 0.5 earlier");
        assert_eq!(c.duration(), Seconds::new(5, 2));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(1));
    }

    #[test]
    fn o_on_gap_without_head_material_is_rejected() {
        let p = fixture(); // clip #2 src 0..2 — nothing before 0
        assert!(matches!(
            trim_out(&p, 2.5),
            Err(EditError::BeforeAsset { .. })
        ));
    }

    #[test]
    fn o_on_trailing_gap_shortens_the_tail() {
        let mut p = fixture();
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(Segment::Gap(Gap {
                duration: Seconds::from_integer(3),
            })); // trailing [5..8)
        let mut h = History::default();
        let batch = trim_out(&p, 6.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
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
            Segment::Gap(Gap {
                duration: Seconds::from_integer(2),
            }),
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            }),
        ];
        let mut h = History::default();
        let batch = trim_in(&p, 1.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::new(1, 2)));
        assert_eq!(
            p.timeline.end(),
            Seconds::new(5, 2),
            "everything slid left 1.5"
        );
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(4));
    }

    #[test]
    fn s_splits_a_clip() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap(); // mid clip #2 [3..5)
        h.apply(&mut p, batch, 0).unwrap();
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
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 2);
        assert!(
            matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(3))
        );
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);

        // D the clip: removed, timeline shrinks by 2
        let batch = delete_ripple(&p, 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(clip_ids(&p), vec![2]);
        // the trailing gap survives (tail size preserved): end = 1(gap) + 2(clip)
        assert_eq!(
            p.timeline.end(),
            Seconds::from_integer(3),
            "shrunk by the clip only"
        );

        // D the gap (re-inserted by undo? no — fresh fixture): D on a gap
        // shrinks the timeline by the gap's length
        let mut p2 = fixture();
        let batch = delete_ripple(&p2, 2.5).unwrap(); // the 1s gap
        h.apply(&mut p2, batch, 0).unwrap();
        assert_eq!(
            clip_by_id(&p2, 2).duration(),
            Seconds::from_integer(2),
            "spans intact"
        );
        assert_eq!(p2.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
    }

    #[test]
    fn dd_on_trailing_gap_removes_it() {
        let mut p = fixture();
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(Segment::Gap(Gap {
                duration: Seconds::from_integer(3),
            })); // trailing [5..8)
        let mut h = History::default();
        let batch = delete_under(&p, 6.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
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
                speed: Seconds::from_integer(1),
            }),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }),
            Segment::Clip(Clip {
                id: 5,
                asset: a,
                asset_in: Seconds::from_integer(3),
                asset_out: Seconds::from_integer(5),
                speed: Seconds::from_integer(1),
            }),
        ];
        let mut h = History::default();
        let batch = delete_ripple(&p, 2.5).unwrap();
        assert!(batch.desc.contains("merged clips #1+#5"), "{}", batch.desc);
        h.apply(&mut p, batch, 0).unwrap();
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
        h.apply(&mut p, batch, 0).unwrap();
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
            Segment::Gap(Gap {
                duration: Seconds::from_integer(2),
            }),
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            }),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(3),
            }),
        ];
        let mut h = History::default();
        let batch = delete_under(&p, 3.0).unwrap();
        assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 1);
        assert!(
            matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(7))
        );
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
            Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }),
            Segment::Clip(Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            }),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }),
        ];
        let mut h = History::default();
        let batch = delete_ripple(&p, 2.0).unwrap();
        assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.segments.len(), 1);
        assert!(
            matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
        );
        assert_eq!(
            p.timeline.end(),
            Seconds::from_integer(2),
            "shrunk by the clip"
        );
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
        let batch = paste(&p, &Segment::Clip(snapshot.clone()), 2.5).unwrap();
        assert!(batch.desc.contains("trimmed clip #2"), "{}", batch.desc);
        h.apply(&mut p, batch, 0).unwrap();
        let segs = p.timeline.video_track().unwrap().segments.clone();
        assert_eq!(clip_ids(&p), vec![1, 3, 2]);
        assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::new(1, 2)));
        assert_eq!(
            clip_by_id(&p, 3).duration(),
            Seconds::from_integer(2),
            "pasted"
        );
        let c2 = clip_by_id(&p, 2);
        assert_eq!(c2.asset_in, Seconds::new(3, 2), "trimmed head");
        assert_eq!(c2.duration(), Seconds::new(1, 2));
        assert_eq!(
            p.timeline.end(),
            Seconds::from_integer(5),
            "length unchanged"
        );
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
            speed: Seconds::from_integer(1),
        };
        let batch = paste(&p, &Segment::Clip(three.clone()), 2.0).unwrap();
        assert!(batch.desc.contains("replaced clip #2"), "{}", batch.desc);
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 3]);
        assert_eq!(
            p.timeline.end(),
            Seconds::from_integer(5),
            "length unchanged"
        );
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);

        // Wholly inside clip #1: the clip splits, the right half gets a
        // fresh id, and the paste sits between the halves.
        let half = Clip {
            id: 0,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::new(1, 2),
            speed: Seconds::from_integer(1),
        };
        let batch = paste(&p, &Segment::Clip(half.clone()), 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 3, 4, 2]);
        assert_eq!(
            clip_by_id(&p, 1).duration(),
            Seconds::from_integer(1),
            "head half"
        );
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
        let batch = paste(&p, &Segment::Clip(snapshot.clone()), 6.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(
            matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
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
            speed: Seconds::from_integer(1),
        };

        // mid-clip: split + insert between
        let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 3, 4, 2]);
        assert_eq!(
            clip_by_id(&p, 1).duration(),
            Seconds::from_integer(1),
            "left half"
        );
        assert_eq!(
            clip_by_id(&p, 2).duration(),
            Seconds::from_integer(2),
            "source kept"
        );
        assert_eq!(p.timeline.end(), Seconds::from_integer(7), "grew by 2");
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));

        // in a gap: the gap splits, followers slide, timeline grows
        let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 2.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(7), "grew by 2");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));

        // past the end: the void becomes a gap, then the clip
        let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 7.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(
            matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
        );
        assert_eq!(p.timeline.end(), Seconds::from_integer(9));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));

        // at a cut: the paste lands BEFORE the clip; the clip keeps its id
        let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 3.0).unwrap();
        eprintln!("at-cut batch: {batch:?}");
        eprintln!("before: ids={:?} end={}", clip_ids(&p), p.timeline.end());
        h.apply(&mut p, batch, 0).unwrap();
        eprintln!("after: ids={:?}", clip_ids(&p));
        assert_eq!(
            clip_by_id(&p, 2).duration(),
            Seconds::from_integer(2),
            "source kept"
        );
        assert_eq!(
            clip_by_id(&p, 3).duration(),
            Seconds::from_integer(2),
            "pasted"
        );
        h.undo(&mut p).unwrap();
    }

    #[test]
    fn extend_tail_grows_clip_and_followers_shift() {
        let mut p = fixture(); // [Clip #1 [0..2)] [Gap 1] [Clip #2 [3..5)]
        let mut h = History::default();
        // extend tail of #1 to 2.5 (asset has material to 10): gap stays,
        // followers shift right, timeline grows by 0.5
        let batch = extend_edge(&p, 1, false, 2.5).unwrap();
        assert!(
            batch.desc.contains("extend tail of clip #1"),
            "{}",
            batch.desc
        );
        h.apply(&mut p, batch, 0).unwrap();
        let c = clip_by_id(&p, 1);
        assert_eq!(c.asset_out, Seconds::new(7, 2));
        let track = p.timeline.video_track().unwrap();
        assert!(
            matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
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
        h.apply(&mut p, batch, 0).unwrap();
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
            speed: Seconds::from_integer(1),
        });
        let mut h = History::default();
        let batch = extend_edge(&p, 2, true, 2.5).unwrap();
        assert!(
            batch.desc.contains("extend head of clip #2"),
            "{}",
            batch.desc
        );
        h.apply(&mut p, batch, 0).unwrap();
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
        assert!(matches!(
            extend_edge(&p, 99, true, 1.0),
            Err(EditError::ClipNotFound(99))
        ));
    }

    #[test]
    fn extend_gap_end_lengthens_trailing_gap() {
        let mut p = fixture();
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(Segment::Gap(Gap {
                duration: Seconds::from_integer(3),
            })); // trailing [5..8)
        let mut h = History::default();
        let batch = extend_gap_end(&p, 10.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(
            matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(5))
        );
        assert_eq!(p.timeline.end(), Seconds::from_integer(10));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(8));
        // not past the gap start, and no trailing gap at all, both refuse
        assert!(matches!(
            extend_gap_end(&p, 4.0),
            Err(EditError::NothingToTrim(_))
        ));
        let q = fixture(); // ends with a clip
        assert!(matches!(extend_gap_end(&q, 6.0), Err(EditError::NoGap)));
    }

    #[test]
    fn extend_gap_tail_moves_any_gap() {
        let mut p = fixture(); // [Clip #1 [0..2)] [Gap 1 @2..3] [Clip #2 [3..5)]
        let mut h = History::default();
        // Ripple (E): grow to [2..4), clip #2 shifts right, timeline grows.
        let batch = extend_gap_tail(&p, 1, 4.0, true).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert!(
            matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
        );
        assert_eq!(p.timeline.end(), Seconds::from_integer(6));
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));

        // Replace (e): grow to [2..4.5) overwrites clip #2's head — it is
        // trimmed, not shifted, and the timeline length is preserved.
        let batch = extend_gap_tail(&p, 1, 4.5, false).unwrap();
        assert!(batch.desc.contains("trimmed clip #2"), "{}", batch.desc);
        h.apply(&mut p, batch, 0).unwrap();
        assert!(
            matches!(&p.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::new(5, 2))
        );
        assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::new(3, 2));
        assert_eq!(clip_by_id(&p, 2).duration(), Seconds::new(1, 2));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "length held");
        h.undo(&mut p).unwrap();

        // Shrink to [2..2.5): clip #2 slides left either way.
        let batch = extend_gap_tail(&p, 1, 2.5, false).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(p.timeline.end(), Seconds::new(9, 2));
        // Not a gap / at/before its start both refuse.
        assert!(matches!(
            extend_gap_tail(&p, 0, 1.0, false),
            Err(EditError::NoGap)
        ));
        assert!(matches!(
            extend_gap_tail(&p, 1, 0.0, true),
            Err(EditError::NothingToTrim(_))
        ));
    }

    #[test]
    fn notices_use_frame_stamps() {
        let p = fixture(); // 30 fps
        let b = split(&p, 1.0).unwrap();
        assert!(b.desc.contains("at f30"), "{}", b.desc);
        // Errors too: at the clip's edge there is nothing to trim.
        let e = trim_in(&p, 0.0).unwrap_err();
        assert!(e.to_string().contains("f0"), "{e}");
        assert!(!e.to_string().contains("0.000000"), "{e}");
    }

    #[test]
    fn extend_gap_head_replaces_the_previous_segment() {
        let mut p = fixture(); // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
        let mut h = History::default();
        // Grow the gap's head to 1.0: clip #1 is trimmed to [0..1).
        let batch = extend_gap_head(&p, 1, 1.0, false).unwrap();
        assert!(batch.desc.contains("trimmed clip #1"), "{}", batch.desc);
        h.apply(&mut p, batch, 0).unwrap();
        let segs = &p.timeline.video_track().unwrap().segments;
        assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(1));
        assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2)));
        assert_eq!(p.timeline.end(), Seconds::from_integer(5), "length held");
        h.undo(&mut p).unwrap();
        // Grow to 0.0: clip #1 is absorbed whole.
        let batch = extend_gap_head(&p, 1, 0.0, false).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert!(
            matches!(&p.timeline.video_track().unwrap().segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(3))
        );
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
        // Rightward (at/past the start) refuses; a clip is not a gap.
        let fresh = fixture();
        assert!(matches!(
            extend_gap_head(&fresh, 1, 2.0, false),
            Err(EditError::NothingToTrim(_))
        ));
        assert!(matches!(
            extend_gap_head(&fresh, 1, 3.0, false),
            Err(EditError::NothingToTrim(_))
        ));
        assert!(matches!(
            extend_gap_head(&fresh, 0, 1.0, false),
            Err(EditError::NoGap)
        ));

        // Ripple: insert space at the head — clip #1 is untouched, the gap
        // grows, and clip #2 shifts right.
        let mut q = fixture();
        let mut h2 = History::default();
        let batch = extend_gap_head(&q, 1, 1.0, true).unwrap();
        assert!(batch.desc.contains("insert gap"), "{}", batch.desc);
        h2.apply(&mut q, batch, 0).unwrap();
        assert_eq!(
            clip_by_id(&q, 1).duration(),
            Seconds::from_integer(2),
            "clip #1 kept"
        );
        assert!(
            matches!(&q.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
        );
        assert_eq!(q.timeline.end(), Seconds::from_integer(6), "grew by 1");

        // Inward (head moved right): ripples — the gap shortens and clip #1
        // is untouched.
        let mut r = fixture();
        let mut h3 = History::default();
        let batch = extend_gap_head(&r, 1, 2.5, false).unwrap();
        assert!(batch.desc.contains("ripple trim gap in"), "{}", batch.desc);
        h3.apply(&mut r, batch, 0).unwrap();
        assert_eq!(
            clip_by_id(&r, 1).duration(),
            Seconds::from_integer(2),
            "clip #1 kept"
        );
        assert_eq!(r.timeline.end(), Seconds::new(9, 2));
    }

    #[test]
    fn ripple_gap_trims_shrink_without_growing_neighbours() {
        // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
        let mut p = fixture();
        let mut h = History::default();
        // In-edge at 2.5: remove 0.5 from the gap; clip #1 is untouched.
        let batch = ripple_gap_in(&p, 2.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(
            clip_by_id(&p, 1).duration(),
            Seconds::from_integer(2),
            "clip #1 intact"
        );
        assert!(
            matches!(&p.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::new(1, 2))
        );
        assert_eq!(p.timeline.end(), Seconds::new(9, 2), "shrunk by 0.5");
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.end(), Seconds::from_integer(5));
        // Out-edge at 2.5: same net shrink, from the back.
        let batch = ripple_gap_out(&p, 2.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(2));
        assert_eq!(p.timeline.end(), Seconds::new(9, 2));
        // Not on a gap → NoGap; at the gap's start → nothing to remove.
        assert!(matches!(ripple_gap_in(&p, 0.5), Err(EditError::NoGap)));
        let fresh = fixture();
        assert!(matches!(
            ripple_gap_in(&fresh, 2.0),
            Err(EditError::NothingToTrim(_))
        ));
    }

    #[test]
    fn trim_consume_target_and_delete_segment() {
        // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
        let p = fixture();
        // `o` targets the segment STARTING at the playhead.
        assert_eq!(trim_consume_target(&p, 2.0, true), Some(1)); // the gap
        assert_eq!(trim_consume_target(&p, 3.0, true), Some(2)); // clip #2
        // `i` targets the segment ENDING there.
        assert_eq!(trim_consume_target(&p, 2.0, false), Some(0)); // clip #1
        assert_eq!(trim_consume_target(&p, 3.0, false), Some(1)); // the gap
        // Not at an edge → no consume.
        assert_eq!(trim_consume_target(&p, 1.0, true), None);
        assert_eq!(trim_consume_target(&p, 1.0, false), None);
        assert_eq!(
            trim_consume_target(&p, 0.0, false),
            None,
            "nothing to the left"
        );

        // A plain clip delete leaves a gap (merged with the neighbour);
        // a ripple delete closes; a gap always ripples.
        let mut q = fixture();
        let mut h = History::default();
        let batch = delete_segment(&q, 0, false).unwrap();
        h.apply(&mut q, batch, 0).unwrap();
        assert_eq!(q.timeline.end(), Seconds::from_integer(5), "length held");
        assert!(
            matches!(&q.timeline.video_track().unwrap().segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(3))
        );
        h.undo(&mut q).unwrap();
        let batch = delete_segment(&q, 0, true).unwrap();
        h.apply(&mut q, batch, 0).unwrap();
        assert_eq!(
            q.timeline.end(),
            Seconds::from_integer(3),
            "shrunk by clip #1"
        );
        assert!(matches!(
            &q.timeline.video_track().unwrap().segments[0],
            Segment::Gap(_)
        ));
    }

    #[test]
    fn paste_gap_payloads() {
        // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
        let p = fixture();
        let payload = Segment::Gap(Gap {
            duration: Seconds::from_integer(1),
        });

        // Ripple-paste the gap into clip #1: the clip splits.
        let mut q = p.clone();
        let mut h = History::default();
        let batch = paste_ripple(&q, &payload, 1.0).unwrap();
        assert!(batch.desc.contains("gap inserted"), "{}", batch.desc);
        h.apply(&mut q, batch, 0).unwrap();
        assert!(
            matches!(&q.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
        assert_eq!(
            q.timeline.end(),
            Seconds::from_integer(6),
            "grew by the gap"
        );

        // Ripple-paste into the existing gap: they merge.
        let mut q = p.clone();
        let mut h = History::default();
        let batch = paste_ripple(&q, &payload, 2.5).unwrap();
        h.apply(&mut q, batch, 0).unwrap();
        assert!(
            matches!(&q.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
        );
        assert_eq!(q.timeline.end(), Seconds::from_integer(6));

        // Replace-paste the gap at 1.0 overwrites [1,2) with gap and
        // coalesces with the gap already starting at 2.
        let mut q = p.clone();
        let mut h = History::default();
        let batch = paste(&q, &payload, 1.0).unwrap();
        h.apply(&mut q, batch, 0).unwrap();
        assert!(
            matches!(&q.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2)),
            "coalesced"
        );
        assert_eq!(q.timeline.end(), Seconds::from_integer(5), "length held");

        // Pasting a gap at the END of an existing gap extends it rather
        // than making a second adjacent gap.
        let two = Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        });
        let mut q = p.clone();
        let mut h = History::default();
        let batch = paste_ripple(&q, &two, 3.0).unwrap();
        h.apply(&mut q, batch, 0).unwrap();
        let segs = q.timeline.video_track().unwrap().segments.clone();
        assert_eq!(segs.len(), 3, "no extra gap segment");
        assert!(
            matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(3)),
            "gap extended"
        );
        assert_eq!(
            q.timeline.end(),
            Seconds::from_integer(7),
            "followers push right"
        );

        // Replace-paste at the gap's end extends it and overwrites the
        // covered span (length held).
        let mut q = p.clone();
        let mut h = History::default();
        let batch = paste(&q, &two, 3.0).unwrap();
        h.apply(&mut q, batch, 0).unwrap();
        let segs = q.timeline.video_track().unwrap().segments.clone();
        assert_eq!(segs.len(), 2, "clip #2 overwritten, no extra gap");
        assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(3)));
        assert_eq!(q.timeline.end(), Seconds::from_integer(5), "length held");
    }

    #[test]
    fn insert_gap_splits_clip_adds_to_gap_and_grows() {
        // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
        let mut p = fixture();
        let mut h = History::default();
        // Over clip #1 at 1.0: split, insert 1 frame (30fps) between.
        let batch = insert_gap(&p, 1.0, 1).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let segs = p.timeline.video_track().unwrap().segments.clone();
        assert_eq!(segs.len(), 5, "clip split + gap inserted");
        assert!(
            matches!(&segs[0], Segment::Clip(c) if c.id == 1 && c.duration() == Seconds::from_integer(1))
        );
        assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::new(1, 30)));
        assert_eq!(
            p.timeline.end(),
            Seconds::from_integer(5) + Seconds::new(1, 30)
        );
        h.undo(&mut p).unwrap();

        // Over the gap at 2.5: the gap grows by the count.
        let batch = insert_gap(&p, 2.5, 15).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert!(
            matches!(&p.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::new(3, 2))
        );
        assert_eq!(
            p.timeline.end(),
            Seconds::from_integer(5) + Seconds::new(1, 2)
        );
        h.undo(&mut p).unwrap();

        // Past the end at 7.0: void gap + one frame.
        let batch = insert_gap(&p, 7.0, 1).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(
            p.timeline.end(),
            Seconds::from_integer(7) + Seconds::new(1, 30)
        );
        h.undo(&mut p).unwrap();

        // Zero/negative count refuses.
        assert!(matches!(insert_gap(&p, 1.0, 0), Err(EditError::TooShort)));
    }

    // `_` at a gap's far edge (or a clip head just after a gap) grows that
    // gap instead of spawning a second, adjacent one.
    #[test]
    fn insert_gap_merges_into_an_adjacent_gap() {
        // [Clip #1 [0..2)] [Gap 1 @2..3] [Clip #2 [3..5)]: `_` at C2's head.
        let mut p = fixture();
        let mut h = History::default();
        let batch = insert_gap(&p, 3.0, 5).unwrap();
        assert!(batch.desc.contains("insert gap"), "{}", batch.desc);
        h.apply(&mut p, batch, 0).unwrap();
        let segs = p.timeline.video_track().unwrap().segments.clone();
        assert_eq!(segs.len(), 3, "no extra gap segment");
        assert!(
            matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1) + Seconds::new(5, 30))
        );
        assert_eq!(
            p.timeline.end(),
            Seconds::from_integer(5) + Seconds::new(5, 30),
            "followers shifted"
        );
        h.undo(&mut p).unwrap();
        assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);

        // Trailing gap: `_` at the track end grows it, no new segment.
        let mut q = fixture();
        q.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(Segment::Gap(Gap {
                duration: Seconds::from_integer(3),
            })); // trailing [5..8)
        let mut h = History::default();
        let batch = insert_gap(&q, 8.0, 1).unwrap();
        h.apply(&mut q, batch, 0).unwrap();
        let segs = q.timeline.video_track().unwrap().segments.clone();
        assert_eq!(segs.len(), 4, "no extra gap segment");
        assert!(
            matches!(&segs[3], Segment::Gap(g) if g.duration == Seconds::from_integer(3) + Seconds::new(1, 30))
        );
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
        h.apply(&mut p, batch, 0).unwrap();
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
            track.segments.push(Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }));
            track.segments.push(Segment::Clip(Clip {
                id: 3,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            }));
        }
        let mut h = History::default();
        let batch = absorb_outward(&p, 1, false, 6.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
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
            speed: Seconds::from_integer(1),
        });
        let mut h = History::default();
        let batch = absorb_outward(&p, 2, true, 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
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
            speed: Seconds::from_integer(1),
        }); // C2 [3..4.98)
        let mut h = History::default();
        // 4.97 snaps to frame 149 (4.9667), leaving a 0.013s stump:
        // under half a 30fps frame, so #2 is absorbed whole.
        let batch = absorb_outward(&p, 1, false, 4.97).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(clip_ids(&p), vec![1]);
        assert_eq!(p.timeline.end(), Seconds::new(149, 30));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_ids(&p), vec![1, 2]);
    }

    #[test]
    fn absorb_validates_direction_and_material() {
        let p = fixture();
        // inward is not an extend
        assert!(matches!(
            absorb_outward(&p, 1, false, 1.0),
            Err(EditError::NothingToTrim(_))
        ));
        assert!(matches!(
            absorb_outward(&p, 2, true, 6.0),
            Err(EditError::NothingToTrim(_))
        ));
        // past the asset end refuses
        assert!(matches!(
            absorb_outward(&p, 1, false, 12.0),
            Err(EditError::BeyondAsset { .. })
        ));
        assert!(matches!(
            absorb_outward(&p, 99, false, 3.0),
            Err(EditError::ClipNotFound(99))
        ));
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
    fn sidecar_path_maps_beside_the_project() {
        assert_eq!(
            History::sidecar_path(Path::new("nle.json")),
            PathBuf::from("nle.history.json")
        );
        // A project without an extension still gets one.
        assert_eq!(
            History::sidecar_path(Path::new("dir/proj")),
            PathBuf::from("dir/proj.history.json")
        );
    }

    #[test]
    fn sidecar_persists_done_only_and_round_trips() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let batch = trim_in(&p, 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        h.undo(&mut p).unwrap(); // done=[split], undone=[trim] (dies on save)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        h.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(&format!("\"version\": {HISTORY_VERSION}")),
            "{text}"
        );
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
    fn history_stores_and_returns_the_playhead_frame() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap();
        h.apply(&mut p, batch, 42).unwrap();
        // trim the tail of clip #1 at 1.0 (it spans [0..2)).
        let batch = trim_out(&p, 1.0).unwrap();
        h.apply(&mut p, batch, 90).unwrap();
        // undo returns the frame its edit was made from
        assert_eq!(h.undo(&mut p).unwrap().playhead, 90);
        assert_eq!(h.undo(&mut p).unwrap().playhead, 42);
        // redo returns the same frame (re-applies that edit)
        assert_eq!(h.redo(&mut p).unwrap().playhead, 42);
        // out-of-range frames can't occur (stored verbatim)
        assert_eq!(h.undo(&mut p).unwrap().playhead, 42);
    }

    #[test]
    fn persisted_edits_undo_by_topology_after_reload() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let batch = trim_in(&p, 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let snapshot = clip_by_id(&p, 1);
        let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 2.5).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
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
        let outcome = h2.undo(&mut p2).unwrap(); // split
        assert!(outcome.desc.contains("split"), "{}", outcome.desc);
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
        h.apply(&mut p, batch, 0).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        h.save(&path).unwrap();

        // Hand-edit outside undo: renumber a clip id.
        let mut p2 = fixture();
        if let Segment::Clip(c) = &mut p2.timeline.video_track_mut().unwrap().segments[2] {
            c.id = 99;
        }
        let mut h2 = History::load(&path).unwrap();
        assert!(matches!(h2.undo(&mut p2), Err(EditError::History(_))));
        // project untouched by the failed undo
        assert_eq!(clip_ids(&p2), vec![1, 99]);
        assert_eq!(p2.timeline.end(), Seconds::from_integer(5));
    }

    // An unused pooled asset can be removed and undone; the whole pool is
    // restored (same order/ids).
    #[test]
    fn asset_removal_is_undoable() {
        let mut p = fixture();
        let (spare, _) = p.add_asset("/media/spare.mp4".into(), probe_report("5"));
        let before = p.media.clone();

        let mut h = History::default();
        let desc = h.remove_asset(&mut p, spare, 42).unwrap();
        assert!(desc.contains(&format!("#{spare}")), "{desc}");
        assert!(p.asset(spare).is_none(), "gone after removal");
        assert_eq!(p.media.len(), before.len() - 1);

        let outcome = h.undo(&mut p).unwrap();
        assert_eq!(outcome.desc, desc, "undo restores the same description");
        assert_eq!(outcome.playhead, 42, "and the playhead it was made from");
        assert_eq!(p.media, before, "pool restored exactly (order and ids)");

        h.redo(&mut p).unwrap();
        assert!(p.asset(spare).is_none(), "redo removes it again");
    }

    // If the pool changed outside the stack, an asset undo refuses and leaves
    // the project untouched rather than clobbering the foreign change.
    #[test]
    fn asset_history_refuses_when_pool_changed_outside() {
        let mut p = fixture();
        let (spare, _) = p.add_asset("/media/spare.mp4".into(), probe_report("5"));
        let mut h = History::default();
        h.remove_asset(&mut p, spare, 0).unwrap();
        let _ = p.add_asset("/media/extra.mp4".into(), probe_report("5"));
        let live = p.media.clone();

        assert!(matches!(h.undo(&mut p), Err(EditError::History(_))));
        assert_eq!(p.media, live, "failed undo left the pool untouched");
    }

    // The pool change serializes into the sidecar and undoes after reload.
    #[test]
    fn asset_removal_sidecar_round_trips() {
        let mut p = fixture();
        let (spare, _) = p.add_asset("/media/spare.mp4".into(), probe_report("5"));
        let mut h = History::default();
        h.remove_asset(&mut p, spare, 0).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.history.json");
        h.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"kind\": \"assets\""), "{text}");

        let mut h2 = History::load(&path).unwrap();
        let mut p2 = p.clone();
        h2.undo(&mut p2).unwrap();
        assert!(p2.asset(spare).is_some(), "asset restored after reload");
    }

    #[test]
    fn history_atom_is_lossless_for_speed_clips() {
        // A rate conform's source range is off the project grid. The old
        // frame-count Atom quantized it on every undo, perturbing the clip
        // until a zero-frame sliver failed validation. The Atom now carries
        // exact source times, so it round-trips bit-for-bit.
        let mut p = fixture(); // fps 30, clip #1 first
        let c = Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::new(101 * 1001, 30000),
            asset_out: Seconds::new(921 * 1001, 30000),
            speed: Seconds::new(1001, 1250),
        };
        p.timeline.video_track_mut().unwrap().segments[0] = Segment::Clip(c.clone());
        let mut h = History::default();
        let batch = trim_out(&p, 1.0).unwrap(); // edit the same track
        h.apply(&mut p, batch, 0).unwrap();
        h.undo(&mut p).unwrap();
        let Segment::Clip(restored) = &p.timeline.video_track().unwrap().segments[0] else {
            panic!("expected clip")
        };
        assert_eq!(*restored, c, "the conform clip round-trips exactly");
    }

    #[test]
    fn split_sides_refuses_sub_frame_pieces() {
        let p = fixture(); // fps 30
        let frame = Seconds::new(1, 30);
        let dur = Seconds::from_integer(2); // 60 frames
        // A quarter frame at either edge counts as "at that edge", so no
        // sliver. (Half a frame rounds up to a frame and is kept.)
        assert_eq!(split_sides(&p, dur, frame / 4), (false, true));
        assert_eq!(split_sides(&p, dur, dur - frame / 4), (true, false));
        // A whole-frame split leaves both pieces.
        assert_eq!(split_sides(&p, dur, frame * 10), (true, true));
    }

    #[test]
    fn purge_stumps_snaps_the_grid_and_drops_sub_frame_segments() {
        let mut p = fixture();
        p.fps = Some(Seconds::new(30, 1));
        let t = p.timeline.video_track_mut().unwrap();
        t.segments = vec![
            // 60.4 frames of clip -> snaps to 60
            Segment::Clip(Clip {
                id: 1,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::new(604, 300),
                speed: Seconds::from_integer(1),
            }),
            // a 0.3-frame stump -> dropped
            Segment::Clip(Clip {
                id: 2,
                asset: 1,
                asset_in: Seconds::from_integer(5),
                asset_out: Seconds::new(501, 100),
                speed: Seconds::from_integer(1),
            }),
            // 30.6 frames of gap -> snaps to 31
            Segment::Gap(Gap {
                duration: Seconds::new(51, 50),
            }),
        ];
        let (cleaned, dropped) =
            purge_stumps(p.timeline.video_track().unwrap(), Seconds::new(30, 1));
        assert_eq!(dropped, 1);
        assert_eq!(cleaned.len(), 2);
        let Segment::Clip(c) = &cleaned[0] else {
            panic!("expected clip")
        };
        assert_eq!(c.asset_out, Seconds::from_integer(2), "60 frames");
        let Segment::Gap(g) = &cleaned[1] else {
            panic!("expected gap")
        };
        assert_eq!(g.duration, Seconds::new(31, 30), "31 frames");
    }

    #[test]
    fn clamp_to_media_trims_or_drops_past_the_end() {
        let fps = Seconds::from_integer(30);
        let mut t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![
                Segment::Clip(Clip::new(
                    1,
                    1,
                    Seconds::from_integer(0),
                    Seconds::from_integer(3),
                )),
                Segment::Clip(Clip::new(
                    2,
                    1,
                    Seconds::from_integer(1),
                    Seconds::from_integer(4),
                )),
            ],
        };
        // Media is 2s: both out-points overrun and are clamped onto whole
        // frames inside it.
        let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
        assert_eq!(notes.len(), 2, "{notes:?}");
        let Segment::Clip(c) = &t.segments[0] else {
            panic!("expected clip")
        };
        assert_eq!(c.asset_out, Seconds::from_integer(2));

        // A clip wholly past the media end is dropped.
        let mut t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![Segment::Clip(Clip::new(
                3,
                1,
                Seconds::from_integer(5),
                Seconds::from_integer(8),
            ))],
        };
        let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(t.segments.is_empty());
    }

    #[test]
    fn clamp_to_media_floors_when_rounding_would_pass_the_media_end() {
        let fps = Seconds::from_integer(30);
        let mut t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![Segment::Clip(Clip::new(
                1,
                1,
                Seconds::from_integer(2),
                Seconds::from_integer(4),
            ))],
        };
        // 3.49s of media: rounding the duration up to 45 frames would reach
        // 3.5s — past the file — so it floors to 44 instead.
        clamp_to_media(&mut t, &|_| Some(Seconds::new(349, 100)), fps);
        let Segment::Clip(c) = &t.segments[0] else {
            panic!("expected clip")
        };
        assert_eq!(c.asset_out - c.asset_in, Seconds::new(44, 30));
        assert!(c.asset_out <= Seconds::new(349, 100));
    }

    #[test]
    fn clamp_to_media_clamps_a_negative_in_point() {
        let fps = Seconds::from_integer(30);
        let mut t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![Segment::Clip(Clip {
                id: 1,
                asset: 1,
                asset_in: Seconds::new(-1, 2), // -0.5s: before the file start
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            })],
        };
        let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(3)), fps);
        assert_eq!(notes.len(), 1, "{notes:?}");
        let Segment::Clip(c) = &t.segments[0] else {
            panic!("expected clip")
        };
        assert_eq!(c.asset_in, Seconds::from_integer(0));
        assert_eq!(c.asset_out, Seconds::from_integer(2));
    }

    #[test]
    fn clamp_to_media_scales_by_speed() {
        let fps = Seconds::from_integer(30);
        // A 2× clip: 4s of source occupies 2s of timeline. Media is 3.44s.
        let mut t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![Segment::Clip(Clip {
                id: 1,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(4),
                speed: Seconds::from_integer(2),
            })],
        };
        clamp_to_media(&mut t, &|_| Some(Seconds::new(344, 100)), fps);
        let Segment::Clip(c) = &t.segments[0] else {
            panic!("expected clip")
        };
        // The timeline duration must land on whole frames (51), and the
        // clamped out-point must stay inside the media.
        assert_eq!(c.duration(), Seconds::new(51, 30));
        assert_eq!(c.asset_out, Seconds::new(34, 10));
        assert!(c.asset_out <= Seconds::new(344, 100));
    }

    #[test]
    fn clamp_to_media_ignores_nonpositive_speed() {
        let fps = Seconds::from_integer(30);
        let mut t = Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![Segment::Clip(Clip {
                id: 1,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(3),
                speed: Seconds::from_integer(0),
            })],
        };
        // No panic, no change — `validate()` is the authority on bad speed.
        let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(t.segments.len(), 1);
    }

    #[test]
    fn history_depth_is_capped_and_redo_clears() {
        let mut p = fixture();
        let mut h = History::default();
        let snapshot = clip_by_id(&p, 1);
        for _ in 0..(MAX_HISTORY + 5) {
            let end = p.timeline.end();
            let end_f = *end.numer() as f64 / *end.denom() as f64;
            let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), end_f + 10.0).unwrap();
            h.apply(&mut p, batch, 0).unwrap();
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
        let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), end_f + 10.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
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
        assert!(
            History::restore(HistoryState {
                version: 2,
                done: vec![]
            })
            .depth()
                == 0
        );

        // A corrupt rational (zero denominator) is rejected at parse time —
        // as raw speed_num/speed_den it reached `Ratio::new` and panicked on
        // the first undo instead.
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 4.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let good = dir.path().join("good.json");
        h.save(&good).unwrap();
        let text = std::fs::read_to_string(&good).unwrap();
        let bad = dir.path().join("denom.json");
        std::fs::write(
            &bad,
            text.replace("\"speed\": \"1/1\"", "\"speed\": \"5/0\""),
        )
        .unwrap();
        assert!(History::load(&bad).is_err());
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
                speed: Seconds::from_integer(1),
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
        assert!(h.apply(&mut p, batch, 0).is_err());
        assert_eq!(p, before, "project untouched");
        assert_eq!(h.depth(), 0, "nothing pushed");
    }

    #[test]
    fn failed_undo_keeps_its_history_entry() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = split(&p, 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
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

    /// Two tracks: video track 0 holds clip #1, audio track 1 holds clip #10.
    fn two_track_fixture() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(3),
            speed: Seconds::from_integer(1),
        })];
        let ai = p.timeline.add_track(TrackKind::Audio);
        p.timeline.tracks[ai].segments = vec![Segment::Clip(Clip {
            id: 10,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::from_integer(1),
        })];
        p
    }

    #[test]
    fn track_ops_target_the_chosen_track_only() {
        let mut p = two_track_fixture();
        let video_before = p.timeline.tracks[0].segments.clone();

        let mut h = History::default();
        let batch = trim_in_on(&p, 1, 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();

        assert_eq!(
            p.timeline.tracks[0].segments, video_before,
            "the video track is untouched"
        );
        // Audio track: [Gap 1][Clip #10 src 1..4]
        let audio = &p.timeline.tracks[1].segments;
        assert!(
            matches!(audio[0], Segment::Gap(_)),
            "the trimmed head became a gap"
        );
        let c = match &audio[1] {
            Segment::Clip(c) => c.clone(),
            Segment::Gap(_) => panic!("expected the trimmed clip"),
        };
        assert_eq!((c.id, c.asset_in, c.asset_out), (10, 1.into(), 4.into()));

        // The history entry is track-scoped: undo restores only track 1.
        h.undo(&mut p).unwrap();
        assert!(matches!(
            &p.timeline.tracks[1].segments[0],
            Segment::Clip(c) if c.id == 10 && c.asset_in == Seconds::from_integer(0)
        ));
    }

    #[test]
    fn split_on_edits_the_chosen_track() {
        let mut p = two_track_fixture();
        let video_before = p.timeline.tracks[0].segments.clone();

        let mut h = History::default();
        let batch = split_on(&p, 1, 2.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();

        assert_eq!(p.timeline.tracks[0].segments, video_before);
        let audio = &p.timeline.tracks[1].segments;
        assert_eq!(audio.len(), 2, "the audio clip split in two");
        match (&audio[0], &audio[1]) {
            (Segment::Clip(l), Segment::Clip(r)) => {
                assert_eq!((l.id, l.asset_in, l.asset_out), (10, 0.into(), 2.into()));
                assert_eq!((r.asset_in, r.asset_out), (2.into(), 4.into()));
                assert_ne!(r.id, 10, "the right half gets a fresh id");
            }
            _ => panic!("expected two clips"),
        }
    }

    #[test]
    fn ops_reject_a_missing_track() {
        let p = two_track_fixture();
        assert!(matches!(
            trim_in_on(&p, 9, 1.0),
            Err(EditError::TrackNotFound(9))
        ));
        assert!(matches!(
            split_on(&p, 9, 1.0),
            Err(EditError::TrackNotFound(9))
        ));
        assert!(matches!(
            delete_under_on(&p, 9, 0.0),
            Err(EditError::TrackNotFound(9))
        ));
    }

    #[test]
    fn speed_change_always_ripples() {
        let mut p = fixture(); // #1 [0,2), gap 1s, #2 [0,2)
        let mut h = History::default();
        // Slow #1 to 0.5x: its 2s of source now occupies 4s of timeline, so
        // the gap and #2 slide right by 2s and the track grows.
        let batch = set_speed(&p, 1, Seconds::new(1, 2)).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let t = p.timeline.video_track().unwrap();
        assert_eq!(t.segments[0].duration(), Seconds::from_integer(4));
        assert_eq!(t.start_of(1), Seconds::from_integer(4));
        assert_eq!(t.end(), Seconds::from_integer(7));
        let Segment::Clip(c) = &t.segments[0] else {
            panic!("expected clip")
        };
        assert_eq!(c.source_duration(), Seconds::from_integer(2)); // untouched
        assert_eq!(c.speed, Seconds::new(1, 2));
    }

    #[test]
    fn speed_round_trips_through_undo() {
        let mut p = fixture();
        let mut h = History::default();
        let batch = set_speed(&p, 2, Seconds::from_integer(2)).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(clip_by_id(&p, 2).speed, Seconds::from_integer(2));
        h.undo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).speed, Seconds::from_integer(1));
        h.redo(&mut p).unwrap();
        assert_eq!(clip_by_id(&p, 2).speed, Seconds::from_integer(2));
    }

    #[test]
    fn split_maps_timeline_to_source_at_speed() {
        // A 2x clip reading source [0,4) occupies timeline [0,2).
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::from_integer(2),
        })];
        let mut h = History::default();
        let batch = split(&p, 1.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let segs = &p.timeline.video_track().unwrap().segments;
        let (Segment::Clip(l), Segment::Clip(r)) = (&segs[0], &segs[1]) else {
            panic!("expected two clips")
        };
        // 1s of timeline at 2x = 2s of source
        assert_eq!(l.asset_out, Seconds::from_integer(2));
        assert_eq!(r.asset_in, Seconds::from_integer(2));
        assert_eq!(l.duration(), Seconds::from_integer(1));
        assert_eq!(r.duration(), Seconds::from_integer(1));
        assert_eq!(l.speed, Seconds::from_integer(2));
    }

    #[test]
    fn trim_scales_timeline_delta_into_source() {
        // A 0.5x clip reading source [0,4) occupies timeline [0,8).
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::new(1, 2),
        })];
        let mut h = History::default();
        // trim the head to timeline 2.0: 2s of timeline = 1s of source
        let batch = trim_in(&p, 2.0).unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        assert_eq!(clip_by_id(&p, 1).asset_in, Seconds::from_integer(1));
        // the trimmed head became a 2s gap (timeline space)
        assert_eq!(
            p.timeline.video_track().unwrap().segments[0],
            Segment::Gap(Gap {
                duration: Seconds::from_integer(2)
            })
        );
    }

    #[test]
    fn set_speed_rejects_non_positive() {
        let p = fixture();
        assert!(matches!(
            set_speed(&p, 1, Seconds::from_integer(0)),
            Err(EditError::BadSpeed(_))
        ));
        assert!(matches!(
            set_speed(&p, 1, Seconds::new(-1, 2)),
            Err(EditError::BadSpeed(_))
        ));
    }

    #[test]
    fn set_speed_refuses_a_sub_frame_span() {
        // A 2-frame source clip at 1/30fps retimed to 6× occupies 1/3 frame
        // of timeline: below one frame, so it must be refused — same rule
        // trims and splits enforce. 2× (1 frame exactly) is allowed.
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::new(2, 30),
            speed: Seconds::from_integer(1),
        })];
        assert!(matches!(
            set_speed(&p, 1, Seconds::from_integer(6)),
            Err(EditError::TooShort)
        ));
        let batch = set_speed(&p, 1, Seconds::from_integer(2)).unwrap(); // exactly 1 frame
        History::default().apply(&mut p, batch, 0).unwrap();
        assert_eq!(
            clip_by_id(&p, 1).duration(),
            Seconds::new(1, 30),
            "2 frames at 2x = 1 frame"
        );
    }
}
