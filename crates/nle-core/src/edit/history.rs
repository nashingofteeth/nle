//! Undo/redo history: what changed, how to invert it, and the sidecar that
//! carries both stacks across processes.
//!
//! `History::apply` (timeline) and `remove_asset` (pool) are the only ways
//! in: each snapshots the affected state before mutating and records the
//! before/after pair as a `Change`. Undo and redo verify the live state
//! against the recorded side and substitute the other.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::media::RawProbe;
use crate::project::{Asset, Project};
use crate::time::Seconds;
use crate::timeline::{
    Clip, Effect, Gap, Segment, Track, Transition, TransitionKind, TransitionSource,
};

use super::clamp::clamp_project;
use super::util::video_track_index;
use super::{Batch, EditError, EditOp, HISTORY_VERSION, MAX_HISTORY};

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
        // Clamp before snapshotting: the recorded `after` must be the state
        // undo restores, so a batch can never leave a clip reading past EOF
        // or a sub-frame sliver behind.
        if let Err(e) = apply_batch(project, &batch.ops).map(|_| {
            clamp_project(project);
        }) {
            project.timeline.tracks[track].segments = saved;
            return Err(e);
        }
        if let Err(e) = project.validate() {
            project.timeline.tracks[track].segments = saved;
            return Err(e.into());
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

    /// Record a whole-track-list edit (add / mute / remove): `edit` mutates
    /// the project and returns the description, and the track lists before
    /// and after are snapshotted — so undo restores the track *set*, not
    /// just its segments. A refused edit (the last video track, a bad
    /// index) leaves the project untouched.
    pub fn edit_tracks<F>(
        &mut self,
        project: &mut Project,
        playhead: i64,
        edit: F,
    ) -> Result<String, EditError>
    where
        F: FnOnce(&mut Project) -> Result<String, EditError>,
    {
        let before = project.timeline.tracks.clone();
        let desc = match edit(project).and_then(|desc| {
            project.validate().map_err(EditError::from)?;
            Ok(desc)
        }) {
            Ok(desc) => desc,
            Err(e) => {
                project.timeline.tracks = before;
                return Err(e);
            }
        };
        self.push_change(Change::Tracks(TracksChange {
            before,
            after: project.timeline.tracks.clone(),
            desc: desc.clone(),
            playhead,
        }));
        Ok(desc)
    }

    /// Add `path` to the pool and record it, so undo removes it again —
    /// refused, loudly, while a clip still uses it (`validate` catches
    /// that). Returns the asset id and the description.
    pub fn add_asset(
        &mut self,
        project: &mut Project,
        path: PathBuf,
        probe: RawProbe,
        playhead: i64,
    ) -> (u32, String) {
        let before = project.media.clone();
        let (id, _) = project.add_asset(path, probe);
        let after = project.media.clone();
        let desc = format!("import asset #{id}");
        self.push_change(Change::Assets(AssetChange {
            before,
            after,
            desc: desc.clone(),
            playhead,
        }));
        (id, desc)
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

    /// Serializable snapshot of the undo stacks (persisted in a sidecar
    /// file). Both stacks are stored: `nle redo` runs in its own process,
    /// so a session-only redo stack would make it permanently empty.
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
        let mut undone = state.undone;
        if undone.len() > MAX_HISTORY {
            let excess = undone.len() - MAX_HISTORY;
            undone.drain(0..excess);
        }
        Self { done, undone }
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

/// Serializable snapshot of the undo stacks (the sidecar file). Both
/// stacks persist so `nle undo` / `nle redo` work across processes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryState {
    pub version: u32,
    pub done: Vec<Change>,
    pub undone: Vec<Change>,
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
        /// Per-clip effects, so undo/redo restores them bit-for-bit.
        effects: Vec<Effect>,
    },
    Gap {
        #[serde(with = "crate::time::rational_ser")]
        duration: Seconds,
    },
    /// A transition, with both of its source sides (as stored on the model
    /// segment), so undo/redo restores it bit-for-bit.
    Transition {
        id: u32,
        #[serde(with = "crate::time::rational_ser")]
        dur: Seconds,
        kind: TransitionKind,
        a: TransitionSource,
        b: TransitionSource,
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
    Tracks(TracksChange),
    Assets(AssetChange),
}

impl Change {
    pub fn desc(&self) -> &str {
        match self {
            Change::Track(c) => &c.desc,
            Change::Tracks(c) => &c.desc,
            Change::Assets(c) => &c.desc,
        }
    }

    pub fn playhead(&self) -> i64 {
        match self {
            Change::Track(c) => c.playhead,
            Change::Tracks(c) => c.playhead,
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

/// A whole-track-list edit — adding, muting, or removing a track. Segment
/// edits carry a `TrackChange` (one track's topology); this is for the
/// operations that change the track *set*, which segment snapshots can't
/// express.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TracksChange {
    pub before: Vec<Track>,
    pub after: Vec<Track>,
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
            effects: c.effects.clone(),
        },
        Segment::Gap(g) => Atom::Gap {
            duration: g.duration,
        },
        Segment::Transition(t) => Atom::Transition {
            id: t.id,
            dur: t.dur,
            kind: t.kind,
            a: t.a.clone(),
            b: t.b.clone(),
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
            effects,
        } => Segment::Clip(Clip {
            id: *id,
            asset: *asset,
            asset_in: *asset_in,
            asset_out: *asset_out,
            speed: *speed,
            effects: effects.clone(),
        }),
        Atom::Gap { duration } => Segment::Gap(Gap {
            duration: *duration,
        }),
        Atom::Transition {
            id,
            dur,
            kind,
            a,
            b,
        } => Segment::Transition(Transition {
            id: *id,
            dur: *dur,
            kind: *kind,
            a: a.clone(),
            b: b.clone(),
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
        Change::Tracks(c) => substitute_tracks(project, c, forward)?,
        Change::Assets(c) => substitute_assets(project, c, forward)?,
    }
    project.validate().map_err(EditError::from)
}

fn substitute_tracks(
    project: &mut Project,
    change: &TracksChange,
    forward: bool,
) -> Result<(), EditError> {
    let (expected, write) = if forward {
        (&change.before, &change.after)
    } else {
        (&change.after, &change.before)
    };
    if project.timeline.tracks != *expected {
        return Err(EditError::History(
            "track list changed outside the undo stack".to_string(),
        ));
    }
    project.timeline.tracks = write.clone();
    Ok(())
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
