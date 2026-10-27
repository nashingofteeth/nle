//! Clip speed: a rational rate whose timeline span is the source span over
//! that rate. A speed change always ripples, and a transition side the clip
//! owns is retimed with it.

use crate::project::Project;
use crate::time::{Seconds, checked_div, format_seconds};
use crate::timeline::{MAX_TIMELINE_SECONDS, Segment, TransitionSource};

use super::effects::adjacent_transitions;
use super::util::{at_least_a_frame, track_at, video_track_index};
use super::{Batch, EditError, EditOp};

pub fn set_speed_on(
    project: &Project,
    track: usize,
    clip_id: u32,
    requested: Seconds,
) -> Result<Batch, EditError> {
    if requested <= Seconds::from_integer(0) {
        return Err(EditError::BadSpeed(format_seconds(&requested)));
    }
    let ti = track;
    let tr = track_at(project, ti)?;
    let si = tr
        .clip_index(clip_id)
        .ok_or(EditError::ClipNotFound(clip_id))?;
    let c = match &tr.segments[si] {
        Segment::Clip(c) => c.clone(),
        Segment::Gap(_) | Segment::Transition(_) => return Err(EditError::ClipNotFound(clip_id)),
    };

    // A transition beside this clip owns a source span *and* renders it into
    // its own `dur`, so its side plays at `span / dur` — which is this clip's
    // rate, the way `add_transition` built it. Retiming the clip must retime
    // those sides too, or the crossfade keeps a rate the neighbouring clip no
    // longer plays (a speed step at the seam).
    let adjacent = adjacent_transitions(&tr.segments, si, &c);
    let owned: Seconds = adjacent.iter().map(|a| a.dur).sum();
    let src_lo = adjacent
        .iter()
        .find(|a| !a.outgoing)
        .map(|a| a.outer)
        .unwrap_or(c.asset_in);
    let src_hi = adjacent
        .iter()
        .find(|a| a.outgoing)
        .map(|a| a.outer)
        .unwrap_or(c.asset_out);

    // One rate plays the whole contribution — this clip plus the transition
    // sides it owns — and the timeline duration lands on the frame grid, like
    // every other duration-changing op. `speed` is then `source / duration`,
    // the rate the render applies, so the model and the export can't disagree.
    // The source spans stay exactly as selected; only how they are split
    // between the clip and the transition sides changes. (Bending a span to
    // fit instead reads one extra source frame, and the render grows a frame
    // per retimed clip — measured 87, 174 against the model's 86, 172.)
    let fps = project.fps.ok_or(EditError::NoFps)?;
    let limit = Seconds::from_integer(MAX_TIMELINE_SECONDS);
    let span = src_hi - src_lo;
    // A rate so slow that the contribution can't fit the timeline limit
    // overflows the rational long before the limit check below could see it
    // (`num-rational`'s `Div` panics in debug and wraps in release). Divide
    // checked, and refuse anything past the limit before the frame arithmetic
    // multiplies it.
    let Some(want) = checked_div(&span, &requested) else {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    };
    if want > limit {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
    // A fast retime can shrink the span below a frame (a 1-frame clip at 2×):
    // refuse rather than round the *request* up to a frame, which would apply
    // a rate nobody asked for (3 s at "100×" must not quietly become 90×).
    at_least_a_frame(project, &(want - owned))?;
    let total = Seconds::from_integer((want * fps).round().to_integer()) / fps;
    let mine_dur = total - owned;
    at_least_a_frame(project, &mine_dur)?;
    let Some(speed) = checked_div(&span, &total) else {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    };

    let mut after = c.clone();
    after.speed = speed;
    if let Some(a) = adjacent.iter().find(|a| !a.outgoing) {
        after.asset_in = a.outer + a.dur * speed;
    }
    if let Some(a) = adjacent.iter().find(|a| a.outgoing) {
        after.asset_out = a.outer - a.dur * speed;
    }

    // A slow-down lengthens the track: keep it inside the timeline limit
    // (the validate backstop would call it "corrupt", which it isn't).
    if tr.end() - c.duration() + mine_dur > Seconds::from_integer(MAX_TIMELINE_SECONDS) {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }

    let mut ops: Vec<EditOp> = Vec::new();
    for a in &adjacent {
        let Segment::Transition(t) = &a.segment else {
            unreachable!("adjacent_transitions only yields transitions")
        };
        let mut t = t.clone();
        if a.outgoing {
            // The `a` side carries the clip's tail: its inner edge is the
            // clip's new out-point.
            if let TransitionSource::Clip { src_in, .. } = &mut t.a {
                *src_in = after.asset_out;
            }
        } else if let TransitionSource::Clip { src_out, .. } = &mut t.b {
            // The `b` side carries the clip's head.
            *src_out = after.asset_in;
        }
        ops.push(EditOp::Replace {
            track: ti,
            index: a.index,
            before: a.segment.clone(),
            after: Segment::Transition(t),
        });
    }
    ops.push(EditOp::Replace {
        track: ti,
        index: si,
        before: Segment::Clip(c.clone()),
        after: Segment::Clip(after),
    });

    // 0.7× of a 2 s clip is 85.714 frames: the grid takes 86, and the rate
    // that fits is 30/43. The notice says both, so a typed rate that can't
    // land exactly doesn't look like a bug.
    let mut desc = if speed == requested {
        format!("speed clip #{clip_id} to {}x", format_seconds(&speed))
    } else {
        format!(
            "speed clip #{clip_id} to {}x ({}x rounded to the frame grid)",
            format_seconds(&speed),
            format_seconds(&requested)
        )
    };
    if !adjacent.is_empty() {
        desc += " · transition side(s) retimed";
    }
    Ok(Batch { ops, desc })
}

pub fn set_speed(project: &Project, clip_id: u32, speed: Seconds) -> Result<Batch, EditError> {
    set_speed_on(project, video_track_index(project)?, clip_id, speed)
}
