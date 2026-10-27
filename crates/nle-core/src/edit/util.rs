//! Small pure helpers shared by the edit operations: track lookup, frame
//! snapping, segment search and gap/merge plumbing. Everything here is
//! `pub(super)`: the operations are the public surface, these are not.

use crate::project::Project;
use crate::time::{Seconds, format_seconds};
use crate::timeline::{Clip, Gap, MAX_TIMELINE_SECONDS, Segment, TimelineError, Track, TrackKind};

use super::{EditError, EditOp};

pub(super) fn video_track_index(project: &Project) -> Result<usize, EditError> {
    project
        .timeline
        .tracks
        .iter()
        .position(|t| t.kind == TrackKind::Video)
        .ok_or(EditError::Timeline(TimelineError::NoVideoTrack))
}

/// The track at `ti`, or `TrackNotFound` — `*_on` ops take a caller-supplied
/// index, so a stale/out-of-range one must error, not panic.
pub(super) fn track_at(project: &Project, ti: usize) -> Result<&Track, EditError> {
    project
        .timeline
        .tracks
        .get(ti)
        .ok_or(EditError::TrackNotFound(ti))
}

/// Snap a float playhead to the nearest frame of the project's fps.
/// Without a known fps (empty project), whole seconds are the grid.
/// Snap a position to the frame grid. Positions beyond the timeline limit
/// are refused *here* — before any duration arithmetic can overflow (a
/// `--at 1e18` used to saturate to `i64::MAX` frames and wrap the track's
/// length negative).
pub(super) fn snap(project: &Project, t: f64) -> Result<Seconds, EditError> {
    let limit = MAX_TIMELINE_SECONDS as f64;
    if !t.is_finite() || t.abs() > limit {
        return Err(EditError::TooLong(MAX_TIMELINE_SECONDS));
    }
    match project.fps {
        Some(fps) => {
            let n = (t * secs_f64(&fps)).round() as i64;
            Ok(Seconds::new(n, 1) / fps)
        }
        None => Ok(Seconds::from_integer(t.round() as i64)),
    }
}

pub(super) fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

/// Frame label for user-facing notices (`f19`) so messages read in frames
/// rather than long decimals. 0-based (frame 0 is the first frame),
/// matching the timecode and history. Without a frame rate it falls back
/// to seconds. (Not for ffmpeg arguments — those stay numeric seconds.)
pub(super) fn stamp(project: &Project, t: &Seconds) -> String {
    match project.fps.as_ref() {
        Some(fps) => format!("f{}", (secs_f64(t) * secs_f64(fps)).round() as i64),
        None => format_seconds(t),
    }
}

/// Result of a trim must leave at least one frame.
/// Every duration-changing op refuses a span that can't be represented on
/// the frame grid: a *sub-frame* span (0.9 frames is not one frame —
/// rounding it up to pass was a bug that let `speed` leave a 0.03 s clip
/// in a 30 fps timeline).
pub(super) fn at_least_a_frame(project: &Project, dur: &Seconds) -> Result<(), EditError> {
    let fps = project.fps.ok_or(EditError::NoFps)?;
    if (*dur * fps).floor().to_integer() >= 1 {
        Ok(())
    } else {
        Err(EditError::TooShort)
    }
}

/// Whether a split at timeline offset `off` inside a clip of `dur` leaves a
/// usable head and tail. A piece under one frame counts as "at that edge",
/// so a split can never strand a sub-frame sliver (which would then round to
/// a zero-frame history atom).
pub(super) fn split_sides(project: &Project, dur: Seconds, off: Seconds) -> (bool, bool) {
    let usable = |d: Seconds| d > Seconds::from_integer(0) && at_least_a_frame(project, &d).is_ok();
    (usable(off), usable(dur - off))
}

/// The clip segment containing the (snapped) playhead.
pub(super) fn clip_at(
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
        Segment::Gap(_) | Segment::Transition(_) => Err(EditError::NoClip),
    }
}

/// The contiguous gap REGION containing the (snapped) playhead —
/// adjacent gaps read as one region. Returns
/// (track, first index, last index, region start, region end).
pub(super) fn gap_region_at(
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
pub(super) fn source_span(c: &Clip, timeline: Seconds) -> Seconds {
    timeline * c.speed
}

/// Merge two newly-adjacent segments into a seamless whole, if they
/// form one: gaps always merge; clips merge iff same asset + contiguous
/// source (prev.asset_out == next.asset_in, exact) and the same speed. The
/// merged clip keeps the earlier segment's id (mirrors split, which keeps
/// the original id on the left half).
pub(super) fn merge_pair(left: &Segment, right: &Segment) -> Option<Segment> {
    match (left, right) {
        (Segment::Gap(a), Segment::Gap(b)) => Some(Segment::Gap(Gap {
            duration: a.duration + b.duration,
        })),
        (Segment::Clip(a), Segment::Clip(b))
            if a.asset == b.asset
                && a.asset_out == b.asset_in
                && a.speed == b.speed
                && a.effects == b.effects =>
        {
            Some(Segment::Clip(Clip {
                id: a.id,
                asset: a.asset,
                asset_in: a.asset_in,
                asset_out: b.asset_out,
                speed: a.speed,
                effects: a.effects.clone(),
            }))
        }
        _ => None,
    }
}

/// Human note for a merge, appended to the batch description.
pub(super) fn merge_note(left: &Segment, right: &Segment) -> String {
    match (left, right) {
        (Segment::Clip(a), Segment::Clip(b)) => format!("merged clips #{}+#{}", a.id, b.id),
        _ => "merged gaps".to_string(),
    }
}

/// Build a Replace+Inserts batch that swaps the segment at `index` for a
/// list of pieces (splits and inserts). The first piece rides the
/// Replace, the rest are Inserted after it.
pub(super) fn replace_with_many(
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
pub(super) fn gap_piece(duration: Seconds) -> Option<Segment> {
    if duration > Seconds::from_integer(0) {
        Some(Segment::Gap(Gap { duration }))
    } else {
        None
    }
}

/// Describe an absorbed segment for the batch description.
pub(super) fn segment_note(seg: &Segment) -> String {
    match seg {
        Segment::Gap(_) => "gap".to_string(),
        Segment::Clip(c) => format!("clip #{}", c.id),
        Segment::Transition(t) => format!("transition #{}", t.id),
    }
}
