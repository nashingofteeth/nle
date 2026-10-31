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

use thiserror::Error;

use crate::project::{Project, ProjectError};
use crate::timeline::{Segment, TimelineError};

mod clamp;
mod clip;
mod effects;
mod extend;
mod history;
mod speed;
mod transitions;
mod trim;
mod util;

pub use clamp::{clamp_project, clamp_to_media, purge_stumps};
pub use clip::{
    delete_clip_by_id, delete_clip_by_id_on, delete_clip_ripple_by_id, delete_clip_ripple_by_id_on,
    delete_ripple, delete_ripple_on, delete_segment, delete_segment_on, delete_under,
    delete_under_on, insert_gap, insert_gap_on, paste, paste_on, paste_ripple, paste_ripple_on,
    split, split_on, trim_consume_target, trim_consume_target_on,
};
pub use effects::{
    add_effect, add_effect_on, clear_effects_on, remove_effect_on, replace_effect_on,
};
pub use extend::{
    absorb_outward, absorb_outward_on, extend_edge, extend_edge_on, extend_gap_end,
    extend_gap_end_on, extend_gap_head, extend_gap_head_on, extend_gap_tail, extend_gap_tail_on,
};
pub use history::{
    AssetChange, Atom, Change, History, HistoryState, TrackChange, TracksChange, UndoOutcome,
};
pub use speed::{set_speed, set_speed_on};
pub use transitions::{add_transition, retime_transition_on, transition_at};
pub use trim::{
    is_gap_at, is_gap_at_on, ripple_gap_in, ripple_gap_in_on, ripple_gap_out, ripple_gap_out_on,
    trim_at_on, trim_in, trim_in_on, trim_in_ripple, trim_in_ripple_on, trim_out, trim_out_on,
    trim_out_ripple, trim_out_ripple_on,
};

/// Deepest undo stack we keep (changes).
pub const MAX_HISTORY: usize = 100;
/// Sidecar file format version. No migration path is kept (see
/// README "Compatibility: none, by design"): old sidecars start fresh.
/// v7 stored clip speed as raw `speed_num`/`speed_den` integers; v8 stores
/// it as a `rational_ser` fraction like every other time in the Atom.
/// v9 persists the redo stack too, so `nile redo` works across processes.
/// v10 adds `Change::Tracks` (track add/mute/rm are undoable).
/// v13 follows the project's percentage `crop` (segments carry effects);
/// v14 follows the per-edge crop insets and percentage `scale`;
/// v15 follows the effects carried on a transition side.
pub const HISTORY_VERSION: u32 = 15;

#[derive(Debug, Error)]
pub enum EditError {
    #[error("clip #{0} not found")]
    ClipNotFound(u32),
    #[error("no track #{0}")]
    TrackNotFound(usize),
    #[error("no segment at index {0}")]
    SegmentNotFound(usize),
    #[error("a transition can't be split or trimmed — remove it (dd) instead")]
    Transition,
    #[error("no transition at {0} — --percent only retimes an existing blend")]
    NoTransition(String),
    #[error("a fade's black side has no clip to trim through")]
    NoClipSide,
    #[error("transition #{id} can't absorb the trim — {detail}")]
    BlendNoHandle { id: u32, detail: String },
    #[error("effects apply to video clips — that track is audio")]
    EffectsNeedVideo,
    #[error("clip #{0} has no effects")]
    NoEffects(u32),
    #[error("clip #{0} has no effect #{1}")]
    EffectNotFound(u32, usize),
    #[error("clip #{0} already has a {1} effect — edit that row instead")]
    DuplicateEffect(u32, &'static str),
    #[error("transitions apply to video tracks — that track is audio")]
    TransitionsNeedVideo,
    #[error("playhead is not in a gap")]
    NoGap,
    #[error("no clip at that position — it is in a gap")]
    NoClip,
    #[error("no segment at that position — the timeline is empty or the position is outside it")]
    NoSegment,
    #[error("nothing to trim at {0} — the playhead is on a segment edge")]
    NothingToTrim(String),
    #[error("result would be shorter than one frame")]
    TooShort,
    #[error("that would make the timeline longer than {0} s")]
    TooLong(i64),
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
    #[error("the yank register is empty — yank a segment first")]
    RegisterEmpty,
    #[error("the register was yanked from a different project — {0} is not in this pool")]
    RegisterForeign(String),
    #[error("can't remove the only video track")]
    LastVideoTrack,
    #[error("{0}")]
    Extend(String),
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

#[cfg(test)]
mod tests;
