//! Deleting and consuming: remove a segment (with or without ripple — a
//! track is a sequence, so removal ripples), or consume the segment an edge
//! would eat. `trim_consume_target_on` answers "which one", which is why the
//! trim keys live here too.

use crate::project::Project;
use crate::time::Seconds;
use crate::timeline::{Clip, Gap, Segment, Transition, TransitionSource};

use super::super::transitions::{blend_ends_at, blend_starts_at};
use super::super::util::{
    merge_note, merge_pair, replace_with_many, snap, track_at, video_track_index,
};
use super::super::{Batch, EditError, EditOp};

pub fn delete_under(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    delete_under_on(project, video_track_index(project)?, playhead)
}

pub fn delete_ripple(project: &Project, playhead: f64) -> Result<Batch, EditError> {
    delete_ripple_on(project, video_track_index(project)?, playhead)
}

pub fn delete_clip_ripple_by_id(project: &Project, clip_id: u32) -> Result<Batch, EditError> {
    delete_clip_ripple_by_id_on(project, video_track_index(project)?, clip_id)
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
pub(super) fn delete_transition_on(
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

pub(super) fn delete_segment_ripple(
    project: &Project,
    ti: usize,
    si: usize,
) -> Result<Batch, EditError> {
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
