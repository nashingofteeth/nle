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
mod clip;

pub use clip::{
    delete_clip_by_id, delete_clip_by_id_on, delete_clip_ripple_by_id, delete_clip_ripple_by_id_on,
    delete_ripple, delete_ripple_on, delete_segment, delete_segment_on, delete_under,
    delete_under_on, insert_gap, insert_gap_on, paste, paste_on, paste_ripple, paste_ripple_on,
    split, split_on, trim_consume_target, trim_consume_target_on,
};

mod extend;

pub use extend::{
    absorb_outward, absorb_outward_on, extend_edge, extend_edge_on, extend_gap_end,
    extend_gap_end_on, extend_gap_head, extend_gap_head_on, extend_gap_tail, extend_gap_tail_on,
};

mod clamp;

pub use clamp::{clamp_to_media, purge_stumps};

mod speed;

pub use speed::{set_speed, set_speed_on};

mod transitions;

pub use transitions::{add_transition, retime_transition_on, transition_at};

use transitions::{Straddle, ensure_blend_fits, move_blend_edge};

mod effects;

pub use effects::{
    add_effect, add_effect_on, clear_effects_on, remove_effect_on, replace_effect_on,
};

use crate::project::{Project, ProjectError};
use crate::time::Seconds;
use crate::timeline::{Clip, Gap, Segment, TimelineError, Transition, TransitionSource};

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
    at_least_a_frame, clip_at, gap_piece, gap_region_at, snap, source_span, stamp, track_at,
    video_track_index,
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
