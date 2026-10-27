//! Transitions: adding a blend, retiming it, and the edge algebra a trim or
//! an extend needs to keep its two sides contiguous.
//!
//! A blend is part of both neighbours — the model and the pitfalls doc are in
//! docs/pitfalls/transitions.md. This module owns the operations; the trim
//! and extend modules call `move_blend_edge` / `shift_blend_side`.

use crate::project::Project;
use crate::time::{Seconds, format_seconds, frames_to_seconds};
use crate::timeline::{
    Clip, MAX_TIMELINE_SECONDS, Segment, Track, TrackKind, Transition, TransitionKind,
    TransitionSource,
};

use super::util::{at_least_a_frame, snap, source_span, stamp, track_at};
use super::{Batch, EditError, EditOp};

/// Add a transition at a boundary (the playhead, snapped to the frame
/// grid). Each **clip** side gives up `dur` of timeline span — the left's
/// tail, the right's head — and the transition occupies `dur`, so a
/// clip↔clip seam shortens the timeline by `dur` while a clip↔black seam
/// leaves it unchanged (the fade sits inside the clip). A side that is a
/// gap or the timeline edge contributes black; at least one side must be a
/// clip. The transition owns the two trimmed-away source spans.
pub fn add_transition(
    project: &Project,
    track: usize,
    playhead: f64,
    kind: TransitionKind,
    dur: Seconds,
) -> Result<Batch, EditError> {
    let fps = project.fps.ok_or(EditError::NoFps)?;
    let t = snap(project, playhead)?;
    let tr = track_at(project, track)?;

    // A transition renders as video (`xfade`); on an audio track the export
    // graph would carry a filter it can't apply, so refuse rather than emit a
    // broken graph (the model must not hold what export can't render).
    if tr.kind != TrackKind::Video {
        return Err(EditError::TransitionsNeedVideo);
    }

    // Whole frames, at least one.
    let frames = (dur * fps).round().to_integer();
    if frames < 1 {
        return Err(EditError::TooShort);
    }
    let dur = frames_to_seconds(frames, fps);

    // The seam must be a boundary: the segment starting at `t` (or the
    // timeline end) and the one ending at `t` (or the start).
    let right = tr.index_at(t);
    match right {
        Some(si) if tr.start_of(si) != t => {
            return Err(EditError::NothingToTrim(stamp(project, &t)));
        }
        None if t != tr.end() => return Err(EditError::NoSegment),
        _ => {}
    }
    let (left_i, right_i) = match right {
        Some(si) => (si.checked_sub(1), Some(si)),
        None => (tr.segments.len().checked_sub(1), None),
    };

    let mut a = TransitionSource::Black;
    let mut b = TransitionSource::Black;
    let mut ops: Vec<EditOp> = Vec::new();
    let mut clip_sides = 0usize;

    // The left side: a clip's tail is trimmed into the transition.
    if let Some(li) = left_i {
        match &tr.segments[li] {
            Segment::Transition(_) => return Err(EditError::Transition),
            Segment::Gap(_) => {}
            Segment::Clip(c) => {
                let rem = c.duration() - dur;
                at_least_a_frame(project, &rem)?;
                let tail = source_span(c, dur);
                let new_out = c.asset_out - tail;
                a = TransitionSource::Clip {
                    asset: c.asset,
                    src_in: new_out,
                    src_out: c.asset_out,
                    effects: c.effects.clone(),
                };
                ops.push(EditOp::Replace {
                    track,
                    index: li,
                    before: tr.segments[li].clone(),
                    after: Segment::Clip(Clip {
                        asset_out: new_out,
                        ..c.clone()
                    }),
                });
                clip_sides += 1;
            }
        }
    }

    // The right side: a clip's head is trimmed into the transition.
    if let Some(ri) = right_i {
        match &tr.segments[ri] {
            Segment::Transition(_) => return Err(EditError::Transition),
            Segment::Gap(_) => {}
            Segment::Clip(c) => {
                let rem = c.duration() - dur;
                at_least_a_frame(project, &rem)?;
                let head = source_span(c, dur);
                let new_in = c.asset_in + head;
                b = TransitionSource::Clip {
                    asset: c.asset,
                    src_in: c.asset_in,
                    src_out: new_in,
                    effects: c.effects.clone(),
                };
                ops.push(EditOp::Replace {
                    track,
                    index: ri,
                    before: tr.segments[ri].clone(),
                    after: Segment::Clip(Clip {
                        asset_in: new_in,
                        ..c.clone()
                    }),
                });
                clip_sides += 1;
            }
        }
    }

    if clip_sides == 0 {
        return Err(EditError::NoClip); // black↔black is meaningless
    }

    ops.push(EditOp::Insert {
        track,
        index: left_i.map(|i| i + 1).unwrap_or(0),
        segment: Segment::Transition(Transition {
            id: project.next_clip_id(),
            dur,
            kind,
            a,
            b,
        }),
    });
    let what = match kind {
        TransitionKind::Dissolve => "dissolve",
        TransitionKind::FadeBlack => "fade black",
    };
    Ok(Batch {
        ops,
        desc: format!("transition ({what}) at {}", stamp(project, &t)),
    })
}

/// The transition containing `at` on `track`: its segment index, a clone,
/// and its start. `None` when the position is not inside a blend.
pub fn transition_at(
    project: &Project,
    track: usize,
    at: f64,
) -> Result<Option<(usize, Transition, Seconds)>, EditError> {
    let t = snap(project, at)?;
    let tr = track_at(project, track)?;
    let Some(mut si) = tr.index_at(t) else {
        return Ok(None);
    };
    if !matches!(tr.segments[si], Segment::Transition(_)) {
        // The original cut is the blend's far edge; accept it too.
        if si > 0 && tr.start_of(si) == t && matches!(tr.segments[si - 1], Segment::Transition(_)) {
            si -= 1;
        } else {
            return Ok(None);
        }
    }
    match &tr.segments[si] {
        Segment::Transition(tt) => Ok(Some((si, tt.clone(), tr.start_of(si)))),
        _ => Ok(None),
    }
}

/// Resize an existing transition to `dur`, keeping each clip side's rate.
/// A longer blend takes more material from both neighbours (and shortens a
/// clip↔clip timeline by the added length); a shorter one gives it back.
/// `None` when the length is unchanged. `at` is any time inside the blend.
pub fn retime_transition_on(
    project: &Project,
    track: usize,
    at: f64,
    dur: Seconds,
) -> Result<Option<Batch>, EditError> {
    let fps = project.fps.ok_or(EditError::NoFps)?;
    let t = snap(project, at)?;
    let tr = track_at(project, track)?;
    if tr.kind != TrackKind::Video {
        return Err(EditError::TransitionsNeedVideo);
    }
    let mut si = tr.index_at(t).ok_or(EditError::NoSegment)?;
    if !matches!(tr.segments[si], Segment::Transition(_))
        && si > 0
        && tr.start_of(si) == t
        && matches!(tr.segments[si - 1], Segment::Transition(_))
    {
        si -= 1; // the original cut is the blend's far edge
    }
    let Segment::Transition(trans) = tr.segments[si].clone() else {
        return Err(EditError::NoTransition(stamp(project, &t)));
    };
    let frames = (dur * fps).round().to_integer();
    if frames < 1 {
        return Err(EditError::TooShort);
    }
    let new_dur = frames_to_seconds(frames, fps);
    if new_dur > Seconds::from_integer(MAX_TIMELINE_SECONDS) {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
    if new_dur == trans.dur {
        return Ok(None);
    }
    let factor = new_dur / trans.dur;

    let mut a_op: Option<EditOp> = None;
    let mut b_op: Option<EditOp> = None;
    let a = match &trans.a {
        TransitionSource::Black => TransitionSource::Black,
        TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            effects,
        } => {
            let Some(Segment::Clip(c)) = si.checked_sub(1).and_then(|i| tr.segments.get(i)) else {
                return Err(EditError::NoClipSide);
            };
            if c.asset != *asset || c.asset_out != *src_in {
                return Err(EditError::NoClipSide);
            }
            let new_in = *src_out - (*src_out - *src_in) * factor;
            if new_in < c.asset_in {
                return Err(EditError::BlendNoHandle {
                    id: trans.id,
                    detail: format!("clip #{} has no more tail", c.id),
                });
            }
            let left = (new_in - c.asset_in) / c.speed;
            if at_least_a_frame(project, &left).is_err() {
                return Err(EditError::BlendNoHandle {
                    id: trans.id,
                    detail: format!("clip #{} would be under a frame", c.id),
                });
            }
            a_op = Some(EditOp::Replace {
                track,
                index: si - 1,
                before: Segment::Clip(c.clone()),
                after: Segment::Clip(Clip {
                    asset_out: new_in,
                    ..c.clone()
                }),
            });
            TransitionSource::Clip {
                asset: *asset,
                src_in: new_in,
                src_out: *src_out,
                effects: effects.clone(),
            }
        }
    };
    let b = match &trans.b {
        TransitionSource::Black => TransitionSource::Black,
        TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            effects,
        } => {
            let Some(Segment::Clip(c)) = tr.segments.get(si + 1) else {
                return Err(EditError::NoClipSide);
            };
            if c.asset != *asset || c.asset_in != *src_out {
                return Err(EditError::NoClipSide);
            }
            let new_out = *src_in + (*src_out - *src_in) * factor;
            if new_out > c.asset_out {
                return Err(EditError::BlendNoHandle {
                    id: trans.id,
                    detail: format!("clip #{} has no more head", c.id),
                });
            }
            let right = (c.asset_out - new_out) / c.speed;
            if at_least_a_frame(project, &right).is_err() {
                return Err(EditError::BlendNoHandle {
                    id: trans.id,
                    detail: format!("clip #{} would be under a frame", c.id),
                });
            }
            b_op = Some(EditOp::Replace {
                track,
                index: si + 1,
                before: Segment::Clip(c.clone()),
                after: Segment::Clip(Clip {
                    asset_in: new_out,
                    ..c.clone()
                }),
            });
            TransitionSource::Clip {
                asset: *asset,
                src_in: *src_in,
                src_out: new_out,
                effects: effects.clone(),
            }
        }
    };
    let mut ops: Vec<EditOp> = Vec::new();
    ops.extend(a_op);
    ops.push(EditOp::Replace {
        track,
        index: si,
        before: Segment::Transition(trans.clone()),
        after: Segment::Transition(Transition {
            dur: new_dur,
            a,
            b,
            ..trans.clone()
        }),
    });
    ops.extend(b_op);
    // Each clip side gives/gets `delta` of visible time; the blend gives it
    // back, so two clip sides change the total by `-delta` and a fade's one
    // clip side leaves it alone.
    let delta = new_dur - trans.dur;
    let clip_sides = i64::from(matches!(trans.a, TransitionSource::Clip { .. }))
        + i64::from(matches!(trans.b, TransitionSource::Clip { .. }));
    let total_delta = delta - delta * clip_sides;
    if tr.end() + total_delta > Seconds::from_integer(MAX_TIMELINE_SECONDS) {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
    Ok(Some(Batch {
        ops,
        desc: format!("retime transition #{} to {frames} frames", trans.id),
    }))
}

/// Is a transition the segment whose span *ends* at `t`? A blend is part of
/// both neighbours, so its edge is not the neighbour's edge for the collapse
/// rule — `o`/`O` there must not delete the following clip.
pub(super) fn blend_ends_at(track: &Track, t: Seconds) -> bool {
    let mut acc = Seconds::from_integer(0);
    for s in &track.segments {
        acc += s.duration();
        if acc == t && matches!(s, Segment::Transition(_)) {
            return true;
        }
    }
    false
}

/// …or *starts* at `t` (`i`/`I` there must not delete the preceding clip).
pub(super) fn blend_starts_at(track: &Track, t: Seconds) -> bool {
    matches!(
        track.index_at(t),
        Some(i)
            if matches!(track.segments[i], Segment::Transition(_))
                && track.start_of(i) == t
    )
}

/// Where a straddled segment sits when its reconciling op applies
/// (`index`), and what it looked like before any removals (`seg` at
/// `start`). Removals ahead of it may have shifted it from its pre-state
/// position, so the two are tracked separately.
pub(super) struct Straddle {
    pub(super) index: usize,
    pub(super) start: Seconds,
    pub(super) seg: Segment,
}

/// A transition side's source span, or `None` for black.
pub(super) fn side_span(side: &TransitionSource) -> Option<Seconds> {
    match side {
        TransitionSource::Clip {
            src_in, src_out, ..
        } => Some(*src_out - *src_in),
        TransitionSource::Black => None,
    }
}

/// The blend after one edge moves to `[new_start, new_end)`. `dur` follows the
/// new span and each clip side keeps its own rate (`span / dur`), so the
/// crossfade stays whole at the neighbours' speed instead of playing faster —
/// `left` says which edge moved, which is which side's outer source stays put.
/// A black side has no span to adjust.
pub(super) fn move_blend_edge(
    tr: &Transition,
    start: Seconds,
    new_start: Seconds,
    new_end: Seconds,
    left: bool,
) -> Transition {
    let dur = new_end - new_start;
    let a_rate = side_span(&tr.a).map(|s| s / tr.dur);
    let b_rate = side_span(&tr.b).map(|s| s / tr.dur);
    let a = match (tr.a.clone(), a_rate) {
        (
            TransitionSource::Clip {
                asset,
                src_in,
                src_out,
                effects,
            },
            Some(rate),
        ) => TransitionSource::Clip {
            asset,
            src_in: if left {
                src_in + (new_start - start) * rate
            } else {
                src_in
            },
            src_out: if left { src_out } else { src_in + dur * rate },
            effects,
        },
        (side, _) => side,
    };
    let b = match (tr.b.clone(), b_rate) {
        (
            TransitionSource::Clip {
                asset,
                src_in,
                src_out,
                effects,
            },
            Some(rate),
        ) => TransitionSource::Clip {
            asset,
            src_in: if left { src_out - dur * rate } else { src_in },
            src_out: if left { src_out } else { src_in + dur * rate },
            effects,
        },
        (side, _) => side,
    };
    Transition {
        dur,
        a,
        b,
        ..tr.clone()
    }
}

/// Every clip side of `blend` must still fit inside its asset. Growing a blend
/// takes handle material off a neighbour (a following clip needs pre-roll
/// before its in-point, a preceding one post-roll after its out), and that
/// handle may not exist — which is exactly why the trim used to leave a gap
/// instead of handing the material to the transition.
pub(super) fn ensure_blend_fits(project: &Project, blend: &Transition) -> Result<(), EditError> {
    for side in [&blend.a, &blend.b] {
        let TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            ..
        } = side
        else {
            continue;
        };
        let Some(asset) = project.asset(*asset) else {
            continue;
        };
        let Some(duration) = asset.duration() else {
            continue;
        };
        let name = asset
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| asset.path.display().to_string());
        let zero = Seconds::from_integer(0);
        if *src_in < zero {
            return Err(EditError::BlendNoHandle {
                id: blend.id,
                detail: format!(
                    "{name} has only {} of material before its in-point — the blend needs {}",
                    format_seconds(src_out),
                    format_seconds(&(*src_out - *src_in))
                ),
            });
        }
        if *src_out > duration {
            return Err(EditError::BlendNoHandle {
                id: blend.id,
                detail: format!(
                    "{name} has only {} of material after its out-point — the blend needs {}",
                    format_seconds(&(duration - *src_in)),
                    format_seconds(&(*src_out - *src_in))
                ),
            });
        }
    }
    Ok(())
}

/// A blend whose neighbouring clip's edge moved *outward* (a ripple extend):
/// the span is unchanged, so the side on that neighbour follows its new edge
/// and keeps its rate — otherwise the clip's material would play twice.
pub(super) fn shift_blend_side(tr: &Transition, a_side: bool, edge_source: Seconds) -> Transition {
    let rate = |s: &TransitionSource| side_span(s).map(|span| span / tr.dur);
    let mut out = tr.clone();
    if a_side {
        if let (TransitionSource::Clip { asset, effects, .. }, Some(r)) = (&tr.a, rate(&tr.a)) {
            out.a = TransitionSource::Clip {
                asset: *asset,
                src_in: edge_source,
                src_out: edge_source + tr.dur * r,
                effects: effects.clone(),
            };
        }
    } else if let (TransitionSource::Clip { asset, effects, .. }, Some(r)) = (&tr.b, rate(&tr.b)) {
        out.b = TransitionSource::Clip {
            asset: *asset,
            src_in: edge_source - tr.dur * r,
            src_out: edge_source,
            effects: effects.clone(),
        };
    }
    out
}
