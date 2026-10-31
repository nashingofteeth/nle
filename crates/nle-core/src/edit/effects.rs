//! Per-clip effects, and the copy a transition side carries.
//!
//! `add`/`replace`/`remove`/`clear` all run through one `EditOp` batch, so
//! undo restores the clip and any transition side that renders it together.

use crate::project::Project;
use crate::time::Seconds;
use crate::timeline::{Clip, Effect, Segment, TrackKind, TransitionSource};

use super::util::{track_at, video_track_index};
use super::{Batch, EditError, EditOp};

/// Change a clip's playback speed. nile's timeline is a sequence, so a new
/// duration reflows every follower — **a speed change always ripples**. The
/// source range is untouched; only the timeline span changes.
/// A transition beside a clip whose side carries part of that clip's
/// material. Its `dur` (the crossfade the user chose) and its *outer* source
/// edge (the cut `x` was made at) stay put; only the inner edge — which is
/// also the clip's in/out point — follows the clip's rate.
pub(super) struct AdjacentTransition {
    /// The transition's index in the track.
    pub(super) index: usize,
    /// The transition is *after* the clip: the clip is its outgoing (left)
    /// neighbour and the `a` side carries the clip's tail.
    pub(super) outgoing: bool,
    /// The transition's timeline length.
    pub(super) dur: Seconds,
    /// The fixed source edge: `a.src_out` when outgoing, else `b.src_in`.
    pub(super) outer: Seconds,
    /// The transition as stored, for the replace op.
    pub(super) segment: Segment,
}

/// The transitions in `segs` whose material abuts clip `c` (at `si`), each
/// with the contiguous source edge `add_transition` left: `a.src_in` is the
/// clip's out-point, `b.src_out` its in-point. A span that no longer lines up
/// (the clip was edited some other way) is left alone.
pub(super) fn adjacent_transitions(
    segs: &[Segment],
    si: usize,
    c: &Clip,
) -> Vec<AdjacentTransition> {
    let mut out = Vec::new();
    if let Some(Segment::Transition(t)) = si.checked_sub(1).and_then(|i| segs.get(i))
        && let TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            ..
        } = &t.b
        && *asset == c.asset
        && *src_out == c.asset_in
    {
        out.push(AdjacentTransition {
            index: si - 1,
            outgoing: false,
            dur: t.dur,
            outer: *src_in,
            segment: Segment::Transition(t.clone()),
        });
    }
    if let Some(Segment::Transition(t)) = segs.get(si + 1)
        && let TransitionSource::Clip {
            asset,
            src_in,
            src_out,
            ..
        } = &t.a
        && *asset == c.asset
        && *src_in == c.asset_out
    {
        out.push(AdjacentTransition {
            index: si + 1,
            outgoing: true,
            dur: t.dur,
            outer: *src_out,
            segment: Segment::Transition(t.clone()),
        });
    }
    out
}

/// Keep the transition sides around `c` carrying its effect list: a transition
/// renders its neighbours' material, effects included, and owns the copy like
/// it owns the trimmed span. Only a side that still lines up with the clip is
/// touched.
fn sync_effect_sides(project: &Project, track: usize, si: usize, c: &Clip, ops: &mut Vec<EditOp>) {
    let Some(tr) = project.timeline.track(track) else {
        return;
    };
    // The transition after this clip uses its tail as `a`.
    if let Some(Segment::Transition(t)) = tr.segments.get(si + 1)
        && let TransitionSource::Clip {
            asset,
            src_in,
            effects,
            ..
        } = &t.a
        && *asset == c.asset
        && *src_in == c.asset_out
        && *effects != c.effects
    {
        let mut after = t.clone();
        if let TransitionSource::Clip { effects, .. } = &mut after.a {
            *effects = c.effects.clone();
        }
        ops.push(EditOp::Replace {
            track,
            index: si + 1,
            before: Segment::Transition(t.clone()),
            after: Segment::Transition(after),
        });
    }
    // The transition before this clip uses its head as `b`.
    if si > 0
        && let Some(Segment::Transition(t)) = tr.segments.get(si - 1)
        && let TransitionSource::Clip {
            asset,
            src_out,
            effects,
            ..
        } = &t.b
        && *asset == c.asset
        && *src_out == c.asset_in
        && *effects != c.effects
    {
        let mut after = t.clone();
        if let TransitionSource::Clip { effects, .. } = &mut after.b {
            *effects = c.effects.clone();
        }
        ops.push(EditOp::Replace {
            track,
            index: si - 1,
            before: Segment::Transition(t.clone()),
            after: Segment::Transition(after),
        });
    }
}

/// Add `effect` to clip `clip_id` on the first video track.
pub fn add_effect(project: &Project, clip_id: u32, effect: Effect) -> Result<Batch, EditError> {
    add_effect_on(project, video_track_index(project)?, clip_id, effect)
}

/// Add `effect` to clip `clip_id`'s effect list (on `track`). One effect per
/// kind: re-adding an existing kind replaces that row in place, so repeated
/// `:effect crop` (or the panel's `c`) adjusts the crop instead of stacking.
pub fn add_effect_on(
    project: &Project,
    track: usize,
    clip_id: u32,
    effect: Effect,
) -> Result<Batch, EditError> {
    let tr = track_at(project, track)?;
    // Effects are video filters: on an audio track the model would hold an
    // effect the export graph never applies, so refuse rather than no-op.
    if tr.kind != TrackKind::Video {
        return Err(EditError::EffectsNeedVideo);
    }
    let si = tr
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let before = tr.segments[si].clone();
    let Segment::Clip(c) = &before else {
        return Err(EditError::ClipNotFound(clip_id));
    };
    let kind = effect.kind();
    let mut c = c.clone();
    let desc = match c.effects.iter().position(|e| e.kind() == kind) {
        Some(i) => {
            c.effects[i] = effect;
            format!("edit {kind} on clip #{clip_id}")
        }
        None => {
            c.effects.push(effect);
            format!("effect on clip #{clip_id}")
        }
    };
    let mut ops = vec![EditOp::Replace {
        track,
        index: si,
        before,
        after: Segment::Clip(c.clone()),
    }];
    sync_effect_sides(project, track, si, &c, &mut ops);
    Ok(Batch { ops, desc })
}

/// Replace the effect at `index` on clip `clip_id` (the TUI's edit gesture).
/// One batch, so one undo puts the old effect back.
pub fn replace_effect_on(
    project: &Project,
    track: usize,
    clip_id: u32,
    index: usize,
    effect: Effect,
) -> Result<Batch, EditError> {
    let tr = track_at(project, track)?;
    if tr.kind != TrackKind::Video {
        return Err(EditError::EffectsNeedVideo);
    }
    let si = tr
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let before = tr.segments[si].clone();
    let Segment::Clip(c) = &before else {
        return Err(EditError::ClipNotFound(clip_id));
    };
    if index >= c.effects.len() {
        return Err(EditError::EffectNotFound(clip_id, index));
    }
    // One effect per kind (see `add_effect_on`): rewriting a row into a kind
    // another row already holds would stack the kind and silently drop the
    // row being replaced. The TUI's edit gesture always replaces a row with
    // its own kind; only the CLI's/explicit `--index` can change kinds.
    let kind = effect.kind();
    if let Some(dup) = c.effects.iter().position(|e| e.kind() == kind)
        && dup != index
    {
        return Err(EditError::DuplicateEffect(clip_id, kind));
    }
    let mut after = c.clone();
    after.effects[index] = effect;
    let kind = after.effects[index].kind();
    let mut ops = vec![EditOp::Replace {
        track,
        index: si,
        before,
        after: Segment::Clip(after.clone()),
    }];
    sync_effect_sides(project, track, si, &after, &mut ops);
    Ok(Batch {
        ops,
        desc: format!("edit {kind} on clip #{clip_id}"),
    })
}

/// `:effect rm` — drop one effect from a clip. `index = None` drops the last
/// one (what a shell user gets without counting rows).
pub fn remove_effect_on(
    project: &Project,
    track: usize,
    clip_id: u32,
    index: Option<usize>,
) -> Result<Batch, EditError> {
    let tr = track_at(project, track)?;
    if tr.kind != TrackKind::Video {
        return Err(EditError::EffectsNeedVideo);
    }
    let si = tr
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let before = tr.segments[si].clone();
    let Segment::Clip(c) = &before else {
        return Err(EditError::ClipNotFound(clip_id));
    };
    let at = match index {
        Some(i) if i < c.effects.len() => i,
        Some(i) => return Err(EditError::EffectNotFound(clip_id, i)),
        None => c
            .effects
            .len()
            .checked_sub(1)
            .ok_or(EditError::NoEffects(clip_id))?,
    };
    let mut after = c.clone();
    let removed = after.effects.remove(at);
    let mut ops = vec![EditOp::Replace {
        track,
        index: si,
        before,
        after: Segment::Clip(after.clone()),
    }];
    sync_effect_sides(project, track, si, &after, &mut ops);
    Ok(Batch {
        ops,
        desc: format!("drop {} from clip #{clip_id}", removed.kind()),
    })
}

/// `:effect clear` — drop every effect from a clip (one undo step).
pub fn clear_effects_on(project: &Project, track: usize, clip_id: u32) -> Result<Batch, EditError> {
    let tr = track_at(project, track)?;
    if tr.kind != TrackKind::Video {
        return Err(EditError::EffectsNeedVideo);
    }
    let si = tr
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let before = tr.segments[si].clone();
    let Segment::Clip(c) = &before else {
        return Err(EditError::ClipNotFound(clip_id));
    };
    if c.effects.is_empty() {
        return Err(EditError::NoEffects(clip_id));
    }
    let n = c.effects.len();
    let mut after = c.clone();
    after.effects.clear();
    let mut ops = vec![EditOp::Replace {
        track,
        index: si,
        before,
        after: Segment::Clip(after.clone()),
    }];
    sync_effect_sides(project, track, si, &after, &mut ops);
    Ok(Batch {
        ops,
        desc: format!("clear {n} effect(s) from clip #{clip_id}"),
    })
}
