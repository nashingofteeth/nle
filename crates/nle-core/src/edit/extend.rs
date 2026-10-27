//! Extend mode's write side: move one edge past a segment instead of
//! overwriting it. The non-ripple form grows into the neighbour and absorbs
//! it when nothing is left; the ripple form shifts the followers.

use crate::project::Project;
use crate::time::Seconds;
use crate::timeline::{Clip, Gap, Segment};

use super::clip::trim_consume_target_on;
use super::transitions::{Straddle, shift_blend_side};
use super::trim::{ripple_gap_in_on, trim_or_absorb_head, trim_or_absorb_tail};
use super::util::{secs_f64, segment_note, snap, source_span, stamp, track_at, video_track_index};
use super::{Batch, EditError, EditOp};

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

/// Pasting a gap against an existing gap extends it instead of making a
/// second, adjacent gap segment. Delegates to `extend_gap_tail`, so the
/// ripple flag decides whether followers shift (`P`) or are overwritten
/// (`p`). `None` when no gap ends at the playhead.
pub(super) fn extend_gap_ending_at(
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
