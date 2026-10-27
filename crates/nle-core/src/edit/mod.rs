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
mod clamp;

pub use clamp::{clamp_to_media, purge_stumps};

mod speed;

pub use speed::{set_speed, set_speed_on};

mod transitions;

pub use transitions::{add_transition, retime_transition_on, transition_at};

use transitions::{
    Straddle, blend_ends_at, blend_starts_at, ensure_blend_fits, move_blend_edge, shift_blend_side,
};

mod effects;

pub use effects::{
    add_effect, add_effect_on, clear_effects_on, remove_effect_on, replace_effect_on,
};

use crate::project::{Project, ProjectError};
use crate::time::Seconds;
use crate::timeline::{
    Clip, Gap, MAX_TIMELINE_SECONDS, Segment, TimelineError, Transition, TransitionSource,
};

mod history;

pub use history::{
    AssetChange, Atom, Change, History, HistoryState, TrackChange, TracksChange, UndoOutcome,
};

/// Deepest undo stack we keep (changes).
pub const MAX_HISTORY: usize = 100;
/// Sidecar file format version. No migration path is kept (see
/// README "Compatibility: none, by design"): old sidecars start fresh.
/// v7 stored clip speed as raw `speed_num`/`speed_den` integers; v8 stores
/// it as a `rational_ser` fraction like every other time in the Atom.
/// v9 persists the redo stack too, so `nle redo` works across processes.
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

mod util;

use util::{
    at_least_a_frame, clip_at, gap_piece, gap_region_at, merge_note, merge_pair, replace_with_many,
    secs_f64, segment_note, snap, source_span, split_sides, stamp, track_at, video_track_index,
};

// ---------------------------------------------- default-track wrappers
//
// The CLI and the single-track tests edit "the" video track. These thin
// wrappers resolve the first video track and delegate to the explicit
// `*_on` operations, which take a track index. The logic lives once, in
// the `_on` functions; the wrappers only pick the default.

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

/// The trim keys' routing (`i`/`I`/`o`/`O`): on a gap every case ripples
/// (the gap just shortens and the followers slide left); on a clip,
/// `i`/`o` leave the trimmed material as a gap and `I`/`O` ripple. At a
/// segment's edge the trim would consume it whole, so delete it instead —
/// `o`/`O` at a segment's start, `i`/`I` at its end.
pub fn trim_at_on(
    project: &Project,
    track: usize,
    playhead: f64,
    out: bool,
    ripple: bool,
) -> Result<Batch, EditError> {
    if let Some(si) = trim_consume_target_on(project, track, playhead, out) {
        return delete_segment_on(project, track, si, ripple);
    }
    // Inside a blend the trim addresses the neighbour the transition is part
    // of (`o`/`O` the preceding clip, `i`/`I` the following one), not the
    // blend itself.
    if let Some(batch) = trim_blend_on(project, track, playhead, out)? {
        return Ok(batch);
    }
    if is_gap_at_on(project, track, playhead) {
        if out {
            ripple_gap_out_on(project, track, playhead)
        } else {
            ripple_gap_in_on(project, track, playhead)
        }
    } else if out {
        if ripple {
            trim_out_ripple_on(project, track, playhead)
        } else {
            trim_out_on(project, track, playhead)
        }
    } else if ripple {
        trim_in_ripple_on(project, track, playhead)
    } else {
        trim_in_on(project, track, playhead)
    }
}

/// `i`/`o` with the playhead strictly inside a blend. The transition is part
/// of both neighbours, so its edges are their far ends: `o`/`O` moves the
/// blend's right edge (the preceding clip's out, and the following clip's in),
/// `i`/`I` its left edge. The clip on the moved side covers what the blend
/// gives up, so `a`/`b` stay contiguous and `span / dur` stays 1× — the blend
/// shrinks instead of playing faster. A fade's black side has no clip, so it
/// is refused rather than guessed at.
fn trim_blend_on(
    project: &Project,
    track: usize,
    playhead: f64,
    out: bool,
) -> Result<Option<Batch>, EditError> {
    let t = snap(project, playhead)?;
    let ti = track;
    let tr = track_at(project, ti)?;
    let Some(si) = tr.index_at(t) else {
        return Ok(None);
    };
    let Segment::Transition(trans) = tr.segments[si].clone() else {
        return Ok(None);
    };
    let start = tr.start_of(si);
    let end = start + trans.dur;
    // At the blend's own far edge there is nothing to trim: the neighbour's
    // effective edge is the blend's *other* end, so `o` at the end and `i` at
    // the start are no-ops (they used to collapse the neighbour into a gap).
    if (out && t == end) || (!out && t == start) {
        return Err(EditError::NothingToTrim(stamp(project, &t)));
    }
    // Strictly inside: at an edge the trim consumes the segment whole.
    if t <= start || t >= end {
        return Ok(None);
    }
    let (Some(Segment::Clip(prev)), Some(Segment::Clip(next))) = (
        si.checked_sub(1).and_then(|i| tr.segments.get(i)),
        tr.segments.get(si + 1),
    ) else {
        return Err(EditError::NoClipSide);
    };
    let (prev, next) = (prev.clone(), next.clone());
    let (
        TransitionSource::Clip {
            src_in: a_in,
            src_out: a_out,
            ..
        },
        TransitionSource::Clip {
            src_in: b_in,
            src_out: b_out,
            ..
        },
    ) = (&trans.a, &trans.b)
    else {
        return Err(EditError::NoClipSide);
    };
    let a_asset = trans.a.asset().ok_or(EditError::NoClipSide)?;
    let b_asset = trans.b.asset().ok_or(EditError::NoClipSide)?;
    let dur = if out { t - start } else { end - t };
    at_least_a_frame(project, &dur)?;

    let (a, b, clip_op) = if out {
        // The right edge moves left: `a` keeps its start and gives up its far
        // end; the following clip's visible in follows the edge and `b` with
        // it, so the blend keeps `dur` of each side.
        let next_in = next.asset_in - source_span(&next, end - t);
        (
            TransitionSource::Clip {
                asset: a_asset,
                src_in: *a_in,
                src_out: *a_in + source_span(&prev, dur),
                effects: prev.effects.clone(),
            },
            TransitionSource::Clip {
                asset: b_asset,
                src_in: *b_in,
                src_out: next_in,
                effects: next.effects.clone(),
            },
            EditOp::Replace {
                track: ti,
                index: si + 1,
                before: Segment::Clip(next.clone()),
                after: Segment::Clip(Clip {
                    asset_in: next_in,
                    ..next.clone()
                }),
            },
        )
    } else {
        // The left edge moves right: `b` keeps its end; the preceding clip's
        // visible out follows the edge and `a` with it.
        let prev_out = prev.asset_out + source_span(&prev, t - start);
        (
            TransitionSource::Clip {
                asset: a_asset,
                src_in: prev_out,
                src_out: *a_out,
                effects: prev.effects.clone(),
            },
            TransitionSource::Clip {
                asset: b_asset,
                src_in: *b_out - source_span(&next, dur),
                src_out: *b_out,
                effects: next.effects.clone(),
            },
            EditOp::Replace {
                track: ti,
                index: si - 1,
                before: Segment::Clip(prev.clone()),
                after: Segment::Clip(Clip {
                    asset_out: prev_out,
                    ..prev.clone()
                }),
            },
        )
    };
    let blend = EditOp::Replace {
        track: ti,
        index: si,
        before: Segment::Transition(trans.clone()),
        after: Segment::Transition(Transition { dur, a, b, ..trans }),
    };
    // Ascending index order, so a Replace pair can't trip an index check.
    let ops = if out {
        vec![blend, clip_op]
    } else {
        vec![clip_op, blend]
    };
    Ok(Some(Batch {
        ops,
        desc: format!(
            "trim {} through the blend to {}",
            if out { "out" } else { "in" },
            stamp(project, &t)
        ),
    }))
}

/// A trim on a clip that borders a blend on the trimmed side (`out` = the
/// clip's tail, `in` = its head). The blend is part of the clip, so its side
/// on that neighbour follows the new edge instead of a gap opening:
///
/// - `i`/`o` (not ripple) grow the blend over the trimmed span — the timeline
///   keeps its length and the material moves into the transition;
/// - `I`/`O` (ripple) keep the blend's `dur` and let the sequence slide: the
///   side follows the edge, and because the clip got shorter the blend and
///   every follower shift for free (a track is a sequence).
///
/// Either way `span / dur` stays the neighbour's rate and the side stays
/// contiguous with the clip. `None` when there is no blend on that side.
fn trim_beside_blend_on(
    project: &Project,
    track: usize,
    si: usize,
    t: Seconds,
    out: bool,
    ripple: bool,
) -> Result<Option<Batch>, EditError> {
    let tr = track_at(project, track)?;
    let Segment::Clip(c) = &tr.segments[si] else {
        return Ok(None);
    };
    let bi = if out {
        let Some(bi) = (si + 1 < tr.segments.len()).then_some(si + 1) else {
            return Ok(None);
        };
        bi
    } else {
        let Some(bi) = si.checked_sub(1) else {
            return Ok(None);
        };
        bi
    };
    let Segment::Transition(blend) = &tr.segments[bi] else {
        return Ok(None);
    };
    let seg_start = tr.start_of(si);
    let delta = if out {
        seg_start + c.duration() - t
    } else {
        t - seg_start
    };
    if delta <= Seconds::from_integer(0) {
        return Ok(None);
    }
    let after = if out {
        Clip {
            asset_out: c.asset_in + source_span(c, t - seg_start),
            ..c.clone()
        }
    } else {
        Clip {
            asset_in: c.asset_in + source_span(c, delta),
            ..c.clone()
        }
    };
    at_least_a_frame(project, &after.duration())?;

    let bstart = tr.start_of(bi);
    let next = if ripple {
        // The side on the trimmed neighbour follows its new edge; the blend's
        // `dur` and its other side stay put, so the rate is unchanged.
        let side = if out { &blend.a } else { &blend.b };
        let TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            effects,
        } = side
        else {
            return Ok(None);
        };
        let rate = (*src_out - *src_in) / blend.dur;
        let tracked = if out {
            TransitionSource::Clip {
                asset: *asset,
                src_in: after.asset_out,
                src_out: after.asset_out + blend.dur * rate,
                effects: effects.clone(),
            }
        } else {
            TransitionSource::Clip {
                asset: *asset,
                src_in: after.asset_in - blend.dur * rate,
                src_out: after.asset_in,
                effects: effects.clone(),
            }
        };
        if out {
            Transition {
                a: tracked,
                ..blend.clone()
            }
        } else {
            Transition {
                b: tracked,
                ..blend.clone()
            }
        }
    } else if out {
        move_blend_edge(blend, bstart, t, bstart + blend.dur, true)
    } else {
        move_blend_edge(blend, bstart, bstart, t, false)
    };
    ensure_blend_fits(project, &next)?;
    let clip_op = EditOp::Replace {
        track,
        index: si,
        before: Segment::Clip(c.clone()),
        after: Segment::Clip(after),
    };
    let blend_op = EditOp::Replace {
        track,
        index: bi,
        before: Segment::Transition(blend.clone()),
        after: Segment::Transition(next),
    };
    let ops = if out {
        vec![clip_op, blend_op]
    } else {
        vec![blend_op, clip_op]
    };
    Ok(Some(Batch {
        ops,
        desc: format!(
            "trim {} to {}",
            if out { "out" } else { "in" },
            stamp(project, &t)
        ),
    }))
}

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
        // A blend on the left is part of this clip: the trim moves its right
        // edge instead of leaving a gap.
        if let Some(batch) = trim_beside_blend_on(project, ti, si, t, false, false)? {
            return Ok(batch);
        }
        let after = Clip {
            id: c.id,
            asset: c.asset,
            asset_in: c.asset_in + source_span(c, delta),
            asset_out: c.asset_out,
            speed: c.speed,
            effects: c.effects.clone(),
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

    // The previous clip's tail extends into the region (if one exists). A
    // transition has no material to stretch in, so the region just shrinks.
    if first > 0
        && let Segment::Clip(prev) = &track.segments[first - 1]
    {
        let pi = first - 1;
        let prev = prev.clone();
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
        // A blend on the right is part of this clip: the trim moves its left
        // edge instead of leaving a gap.
        if let Some(batch) = trim_beside_blend_on(project, ti, si, t, true, false)? {
            return Ok(batch);
        }
        let after = Clip {
            id: c.id,
            asset: c.asset,
            asset_in: c.asset_in,
            asset_out: c.asset_in + source_span(c, new_len),
            speed: c.speed,
            effects: c.effects.clone(),
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

    // The next clip's head extends back into the region (if one exists). A
    // transition has no material to stretch in, so the region just shrinks.
    if last + 1 < track.segments.len()
        && let Segment::Clip(next) = &track.segments[last + 1]
    {
        let ni = last + 1;
        let next = next.clone();
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
                Segment::Clip(_) | Segment::Transition(_) => unreachable!("region is gaps"),
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
                Segment::Clip(_) | Segment::Transition(_) => unreachable!("region is gaps"),
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
    let t = snap(project, playhead)?;
    if let Ok((_, si, _)) = clip_at(project, track, &t)
        && let Some(batch) = trim_beside_blend_on(project, track, si, t, false, true)?
    {
        return Ok(batch);
    }
    let on_clip = clip_at(project, track, &t).is_ok();
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
    let t = snap(project, playhead)?;
    if let Ok((_, si, _)) = clip_at(project, track, &t)
        && let Some(batch) = trim_beside_blend_on(project, track, si, t, true, true)?
    {
        return Ok(batch);
    }
    let on_clip = clip_at(project, track, &t).is_ok();
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
        effects: c.effects.clone(),
    });
    let right = Segment::Clip(Clip {
        id: project.next_clip_id(),
        asset: c.asset,
        asset_in: c.asset_in + source_span(&c, t - seg_start),
        asset_out: c.asset_out,
        speed: c.speed,
        effects: c.effects.clone(),
    });
    Ok(Batch {
        ops: replace_with_many(ti, si, &Segment::Clip(c.clone()), vec![left, right]),
        desc: format!("split clip #{} at {}", c.id, stamp(project, &t)),
    })
}

/// `dd` — delete what's under the playhead, leaving a gap where a clip
/// was (the timeline end is preserved); a gap under the playhead is
/// removed (shrinking the timeline); a transition is removed and hands the
/// frames it had trimmed off its neighbours back to them.
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
        // Removing a transition gives its trimmed spans back to the
        // neighbours (see `delete_transition_on`).
        Segment::Transition(_) => delete_segment_ripple(project, ti, si),
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

/// Remove a transition, handing each side's trimmed span back to the
/// neighbour it was taken from — so `x` then `dd` round-trips. A side is only
/// restored when its neighbour still lines up exactly (same asset, contiguous
/// source); a neighbour edited since `x` is left alone and the material that
/// transition held is simply gone (undo is the way back). The neighbours are
/// *not* merged: before `x` they were two segments, and removing it must not
/// fuse them.
fn delete_transition_on(
    project: &Project,
    ti: usize,
    si: usize,
    t: Transition,
) -> Result<Batch, EditError> {
    let segs = &track_at(project, ti)?.segments;
    let mut ops: Vec<EditOp> = Vec::new();
    let mut restored: Vec<&str> = Vec::new();

    // Outgoing side: the transition holds the left neighbour's trimmed tail,
    // so giving it back means extending that clip's out-point.
    if let Some(Segment::Clip(left)) = si.checked_sub(1).and_then(|i| segs.get(i))
        && let TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            ..
        } = &t.a
        && left.asset == *asset
        && left.asset_out == *src_in
    {
        let after = Clip {
            asset_out: *src_out,
            ..left.clone()
        };
        ops.push(EditOp::Replace {
            track: ti,
            index: si - 1,
            before: Segment::Clip(left.clone()),
            after: Segment::Clip(after),
        });
        restored.push("left");
    }

    // Incoming side: the transition holds the right neighbour's trimmed head,
    // so giving it back means moving that clip's in-point earlier.
    if let Some(Segment::Clip(right)) = segs.get(si + 1)
        && let TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            ..
        } = &t.b
        && right.asset == *asset
        && right.asset_in == *src_out
    {
        let after = Clip {
            asset_in: *src_in,
            ..right.clone()
        };
        ops.push(EditOp::Replace {
            track: ti,
            index: si + 1,
            before: Segment::Clip(right.clone()),
            after: Segment::Clip(after),
        });
        restored.push("right");
    }

    // One gap, never two: a gap neighbour that was NOT restored (the clip
    // holding the handle was deleted) leaves the removed span as a second
    // adjacent gap — the one shape track deletes never produce
    // (`delete_under_on` fuses). The span must not vanish, which would yank
    // the followers: it joins the neighbouring gap(s). Any restored side
    // already covers the whole blend span (both sides overlay it, black
    // included), so fusing only applies when *nothing* was restored — a
    // restored handle beside a gap leaves that gap alone.
    let dur = t.dur;
    let blend = Segment::Transition(t);
    let plain = restored.is_empty();
    let left_gap = plain && si > 0 && matches!(segs[si - 1], Segment::Gap(_));
    let right_gap = plain && si + 1 < segs.len() && matches!(segs[si + 1], Segment::Gap(_));
    let mut desc = if restored.is_empty() {
        "delete transition".to_string()
    } else {
        format!(
            "delete transition (restored the {} handle)",
            restored.join(" and ")
        )
    };
    match (left_gap, right_gap) {
        (true, true) => {
            let total = segs[si - 1].duration() + dur + segs[si + 1].duration();
            desc += " (merged gaps)";
            ops.extend([
                EditOp::Replace {
                    track: ti,
                    index: si - 1,
                    before: segs[si - 1].clone(),
                    after: Segment::Gap(Gap { duration: total }),
                },
                EditOp::Remove {
                    track: ti,
                    index: si,
                    segment: blend.clone(),
                },
                EditOp::Remove {
                    track: ti,
                    index: si,
                    segment: segs[si + 1].clone(),
                },
            ]);
        }
        _ if left_gap => {
            desc += " (the span joins the gap)";
            ops.extend([
                EditOp::Replace {
                    track: ti,
                    index: si - 1,
                    before: segs[si - 1].clone(),
                    after: Segment::Gap(Gap {
                        duration: segs[si - 1].duration() + dur,
                    }),
                },
                EditOp::Remove {
                    track: ti,
                    index: si,
                    segment: blend.clone(),
                },
            ]);
        }
        _ if right_gap => {
            desc += " (the span joins the gap)";
            ops.extend([
                EditOp::Replace {
                    track: ti,
                    index: si + 1,
                    before: segs[si + 1].clone(),
                    after: Segment::Gap(Gap {
                        duration: segs[si + 1].duration() + dur,
                    }),
                },
                EditOp::Remove {
                    track: ti,
                    index: si,
                    segment: blend.clone(),
                },
            ]);
        }
        // Stale clip sides on both sides: the blend's span is really gone
        // (see the stale-neighbour test), so plain deletion shortens.
        _ => ops.push(EditOp::Remove {
            track: ti,
            index: si,
            segment: blend,
        }),
    }
    Ok(Batch { ops, desc })
}

fn delete_segment_ripple(project: &Project, ti: usize, si: usize) -> Result<Batch, EditError> {
    let seg = track_at(project, ti)?.segments[si].clone();
    // A transition owns the material either side of it: removing one hands
    // that back rather than rippling, so `x` then `dd` round-trips.
    if let Segment::Transition(t) = seg {
        return delete_transition_on(project, ti, si, t);
    }
    let mut ops = vec![EditOp::Remove {
        track: ti,
        index: si,
        segment: seg.clone(),
    }];
    let mut desc = match seg {
        Segment::Clip(_) => "ripple delete".to_string(),
        Segment::Gap(_) => "delete gap (ripple)".to_string(),
        Segment::Transition(_) => unreachable!("handled above"),
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

/// The ops appending `payload` at/past the track end: the void between the
/// track's end and the playhead becomes a gap — coalesced with the payload when
/// it is itself a gap, so the track never holds two adjacent gap segments.
fn append_at_end(ti: usize, len: usize, void: Seconds, payload: Segment) -> Vec<EditOp> {
    if void <= Seconds::from_integer(0) {
        return vec![EditOp::Insert {
            track: ti,
            index: len,
            segment: payload,
        }];
    }
    match payload {
        Segment::Gap(pg) => vec![EditOp::Insert {
            track: ti,
            index: len,
            segment: Segment::Gap(Gap {
                duration: void + pg.duration,
            }),
        }],
        payload => vec![
            EditOp::Insert {
                track: ti,
                index: len,
                segment: Segment::Gap(Gap { duration: void }),
            },
            EditOp::Insert {
                track: ti,
                index: len + 1,
                segment: payload,
            },
        ],
    }
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
            effects: c.effects.clone(),
        }),
        Segment::Gap(g) => Segment::Gap(Gap {
            duration: g.duration,
        }),
        Segment::Transition(t) => Segment::Transition(Transition {
            id: new_id,
            ..t.clone()
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
        let void = t - track.end();
        return Ok(Batch {
            ops: append_at_end(ti, track.segments.len(), void, pasted.clone()),
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
                    // A transition is opaque: the paste consumes it whole.
                    Segment::Transition(_) => None,
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
                            effects: c.effects.clone(),
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
            // A transition is opaque: the paste consumes it whole.
            Segment::Transition(_) => {}
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
    // Bound the length *before* converting frames to a duration: `frames`
    // near i64::MAX would overflow the conversion itself.
    let limit = Seconds::from_integer(MAX_TIMELINE_SECONDS);
    let max_frames = (limit * fps).to_integer();
    if frames > max_frames {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
    let gap = Segment::Gap(Gap {
        duration: Seconds::new(frames, 1) / fps,
    });
    if t + gap.duration() > limit {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
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

    // At/past the end: bridge the void (if any), coalesced with the new gap.
    if track.segments.is_empty() || t >= track.end() {
        let void = t - track.end();
        return Ok(Batch {
            ops: append_at_end(ti, track.segments.len(), void, gap),
            desc: format!("insert gap at {}", stamp(project, &t)),
        });
    }

    let si = track.index_at(t).ok_or(EditError::NoSegment)?;
    let seg = track.segments[si].clone();
    let seg_start = track.start_of(si);
    let off = t - seg_start;

    match &seg {
        // `_` on a transition would split it; refuse (phase 1).
        Segment::Transition(_) => Err(EditError::Transition),
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
                    effects: c.effects.clone(),
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
                    effects: c.effects.clone(),
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
        Segment::Transition(_) => "transition".to_string(),
    };

    // At or past the track end: the void becomes a gap (coalesced with the
    // payload when it is itself a gap), then the payload lands on the playhead.
    if t >= track.end() {
        let void = t - track.end();
        return Ok(Batch {
            ops: append_at_end(
                ti,
                track.segments.len(),
                void,
                pasted_piece(snapshot, project.next_clip_id()),
            ),
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
        // A transition owns its neighbours' trimmed spans and can't be split,
        // so an insert at/over one is refused (as `_` does) rather than
        // consuming it — a ripple paste would have to cut it in two.
        Segment::Transition(_) => return Err(EditError::Transition),
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
                    effects: c.effects.clone(),
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
                    effects: c.effects.clone(),
                }));
            }
            pieces
        }
        Segment::Gap(g) => {
            // Guard the remainders like `paste_on` does: an off-grid gap start
            // could otherwise strand a sub-frame sliver.
            let mut pieces = Vec::new();
            let head = t - seg_start;
            let tail = g.duration - head;
            if head > Seconds::from_integer(0) && at_least_a_frame(project, &head).is_ok() {
                pieces.push(Segment::Gap(Gap { duration: head }));
            }
            pieces.push(pasted_piece(snapshot, project.next_clip_id()));
            if tail > Seconds::from_integer(0) && at_least_a_frame(project, &tail).is_ok() {
                pieces.push(Segment::Gap(Gap { duration: tail }));
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
/// gap behind (dd semantics); a transition is removed and gives its trimmed
/// spans back to the neighbours.
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
        // Removing a transition gives its trimmed spans back to the
        // neighbours (see `delete_transition_on`).
        Some(Segment::Transition(_)) => delete_segment_ripple(project, ti, index),
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
        // A blend ending here is part of this segment: the playhead is inside
        // its effective span, not at its edge.
        (!blend_ends_at(track, t) && track.start_of(si) == t).then_some(si)
    } else {
        // A blend starting here is part of the segment ending here.
        if blend_starts_at(track, t) {
            return None;
        }
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
        // Found by clip id, so a transition here is impossible.
        Segment::Transition(_) => Err(EditError::ClipNotFound(clip_id)),
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
        Segment::Gap(_) | Segment::Transition(_) => return Err(EditError::ClipNotFound(clip_id)),
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
    let side = if head { "head" } else { "tail" };
    let mut ops = vec![EditOp::Replace {
        track: ti,
        index: si,
        before: Segment::Clip(c),
        after: Segment::Clip(after.clone()),
    }];
    // A ripple extend shifts the blend beside the moved edge: its side follows
    // the clip's new edge, or the clip's material would play twice (once in its
    // own span, once inside the crossfade).
    let neighbour = if head {
        si.checked_sub(1)
    } else {
        Some(si + 1)
    };
    if let Some(ni) = neighbour
        && let Some(Segment::Transition(t)) = track.segments.get(ni)
    {
        let edge_source = if head {
            after.asset_in
        } else {
            after.asset_out
        };
        ops.push(EditOp::Replace {
            track: ti,
            index: ni,
            before: Segment::Transition(t.clone()),
            after: Segment::Transition(shift_blend_side(t, !head, edge_source)),
        });
    }
    Ok(Batch {
        ops,
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
        Segment::Clip(_) | Segment::Transition(_) => return Err(EditError::NoGap),
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
        Segment::Gap(_) | Segment::Transition(_) => return Err(EditError::ClipNotFound(clip_id)),
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
        // A straddled transition shrinks to its uncovered tail: the blend's
        // left edge moves to `t` and each side follows its neighbour, so the
        // crossfade stays whole at the neighbours' rate. Under a frame is a
        // sliver, not a transition: drop it whole.
        Segment::Transition(tr) => {
            let remainder = start + tr.dur - t;
            if at_least_a_frame(project, &remainder).is_err() {
                absorbed.push(format!("transition #{}", tr.id));
                ops.push(EditOp::Remove {
                    track: ti,
                    index,
                    segment: Segment::Transition(tr),
                });
                return Ok(());
            }
            ops.push(EditOp::Replace {
                track: ti,
                index,
                before: Segment::Transition(tr.clone()),
                after: Segment::Transition(move_blend_edge(&tr, start, t, start + tr.dur, true)),
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
        // A straddled transition shrinks to its uncovered head: the blend's
        // right edge moves to `t`, each side following its neighbour. Under a
        // frame is a sliver, not a transition: drop it whole.
        Segment::Transition(tr) => {
            let remainder = tr.dur - (t - start);
            if at_least_a_frame(project, &remainder).is_err() {
                absorbed.push(format!("transition #{}", tr.id));
                ops.push(EditOp::Remove {
                    track: ti,
                    index,
                    segment: Segment::Transition(tr),
                });
                return Ok(());
            }
            ops.push(EditOp::Replace {
                track: ti,
                index,
                before: Segment::Transition(tr.clone()),
                after: Segment::Transition(move_blend_edge(&tr, start, start, t, false)),
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
mod tests;
