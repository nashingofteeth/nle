//! Extend semantics: arming a boundary and routing a commit to an op.
//!
//! These are the two pure decisions behind the TUI's extend mode — which
//! edge a position *arms* (`resolve_armed`), and which edit op a commit at
//! a target time *routes* to (`plan_commit` → `commit_batch`). They live
//! here, not in the TUI, so the headless CLI can grow/shrink/absorb edges
//! under exactly the same rules — and so both decisions are unit-testable
//! without mpv.

use crate::edit::{self, Batch, EditError};
use crate::project::Project;
use crate::time::secs_f64;
use crate::timeline::Segment;

/// The boundary an extend session works on: a clip's head/tail (by clip
/// id — stable while the mode is active), or a gap's left/right edge (by
/// segment index).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Armed {
    /// Head (`head == true`) or tail of the clip with this id.
    Clip { id: u32, head: bool },
    /// A gap armed at its tail (right edge) — extend it outward and
    /// followers shift/are overwritten. `index` is the gap's segment
    /// index, stable while the mode is active. Unbounded.
    GapTail { index: usize },
    /// A gap armed at its head (left edge) — extending it left inserts
    /// space (or overwrites the preceding segment); unbounded below.
    GapHead { index: usize },
}

/// What committing at the target position means. Maps 1:1 to edit ops in
/// `commit_batch`; kept separate so the routing table is unit testable
/// without mpv.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommitKind {
    TrimHead,
    TrimTail,
    GrowHeadIntoGap,
    GrowTailIntoGap,
    GrowTailPastEnd,
    AbsorbTail,
    AbsorbHead,
    /// Move a gap's tail to the target (grow or shrink).
    GapTail,
    /// Move a gap's head leftward (insert space / overwrite the left).
    GapHead,
    Delete,
    Noop,
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// The timeline span of a gap by segment index.
pub fn gap_span(project: &Project, index: usize, ti: usize) -> Option<(f64, f64)> {
    let track = project.timeline.track(ti)?;
    let g = match track.segments.get(index) {
        Some(Segment::Gap(g)) => g,
        _ => return None,
    };
    let s = secs_f64(&track.start_of(index));
    Some((s, s + secs_f64(&g.duration)))
}

/// The timeline span of a clip by id.
pub fn clip_span(project: &Project, id: u32, ti: usize) -> Option<(f64, f64)> {
    let track = project.timeline.track(ti)?;
    let mut acc = 0.0;
    for seg in &track.segments {
        let dur = secs_f64(&seg.duration());
        if let Segment::Clip(c) = seg
            && c.id == id
        {
            return Some((acc, acc + dur));
        }
        acc += dur;
    }
    None
}

/// Resolve the armed boundary at timeline position `t`: clip starts win
/// (head-of-right at shared cuts, consistent with `clip_under`), then
/// clip ends, then virtual timeline edges with gaps. Anything else is
/// not a boundary.
pub fn resolve_armed(project: &Project, t: f64, ti: usize) -> Result<(Armed, f64), String> {
    let track = project
        .timeline
        .track(ti)
        .ok_or_else(|| "empty timeline — add clips first".to_string())?;
    if track.segments.is_empty() {
        return Err("empty timeline — add clips first".to_string());
    }
    let eps = 1e-6;
    // Pass 1: the segment STARTING at t wins — a clip's head (shared cuts
    // arm the right head), or, when the right segment is a gap, the gap's
    // head. So `e` at a clip|gap boundary arms the gap, and at the very
    // start it arms the leading gap's head (extendable past time 0).
    let mut acc = 0.0;
    for (i, seg) in track.segments.iter().enumerate() {
        if (acc - t).abs() < eps {
            match seg {
                Segment::Clip(c) => {
                    return Ok((
                        Armed::Clip {
                            id: c.id,
                            head: true,
                        },
                        acc,
                    ));
                }
                Segment::Gap(_) => return Ok((Armed::GapHead { index: i }, acc)),
                // A transition isn't material to extend; phase 1 refuses it.
                Segment::Transition(_) => {
                    return Err("a transition can't be armed — remove it (dd) instead".to_string());
                }
            }
        }
        acc += secs_f64(&seg.duration());
    }
    // Pass 2: clip ends — only bites at the timeline end, where nothing
    // starts.
    let mut acc = 0.0;
    for seg in &track.segments {
        acc += secs_f64(&seg.duration());
        if let Segment::Clip(c) = seg
            && (acc - t).abs() < eps
        {
            return Ok((
                Armed::Clip {
                    id: c.id,
                    head: false,
                },
                acc,
            ));
        }
    }
    // Pass 3: gap ends (the trailing gap at the timeline end).
    let mut acc = 0.0;
    for (i, seg) in track.segments.iter().enumerate() {
        acc += secs_f64(&seg.duration());
        if matches!(seg, Segment::Gap(_)) && (acc - t).abs() < eps {
            return Ok((Armed::GapTail { index: i }, acc));
        }
    }
    Err("extend starts at a boundary — j/k to a cut".to_string())
}

/// The timeline position of an armed boundary. Computed from the live
/// layout, so it stays honest after any edit the mode does not make.
pub fn armed_edge(project: &Project, armed: &Armed, ti: usize) -> Result<f64, String> {
    match armed {
        Armed::Clip { id, head } => {
            let (s, e) = clip_span(project, *id, ti).ok_or_else(|| "clip vanished".to_string())?;
            Ok(if *head { s } else { e })
        }
        Armed::GapTail { index } => Ok(gap_span(project, *index, ti)
            .ok_or_else(|| "gap vanished".to_string())?
            .1),
        Armed::GapHead { index } => Ok(gap_span(project, *index, ti)
            .ok_or_else(|| "gap vanished".to_string())?
            .0),
    }
}

/// Route a commit from the armed edge and the target position. Positions
/// use the live track layout (unchanged while an extend session is open).
pub fn plan_commit(
    project: &Project,
    armed: &Armed,
    edge_pos: f64,
    ph: f64,
    ti: usize,
) -> Result<CommitKind, String> {
    if approx(ph, edge_pos) {
        return Ok(CommitKind::Noop);
    }
    match armed {
        Armed::Clip { id, head } => {
            let (s, e) = clip_span(project, *id, ti).ok_or_else(|| "clip vanished".to_string())?;
            let track = project
                .timeline
                .track(ti)
                .ok_or_else(|| "clip vanished".to_string())?;
            let si = track
                .segments
                .iter()
                .position(|seg| matches!(seg, Segment::Clip(c) if c.id == *id))
                .ok_or_else(|| "clip vanished".to_string())?;
            if *head {
                if ph > edge_pos && ph < e - 1e-9 {
                    Ok(CommitKind::TrimHead)
                } else if ph < edge_pos {
                    // Strictly inside the adjacent gap: plain grow. At or
                    // past its far edge there's no gap left to donate, so
                    // absorb (which removes the covered span).
                    let in_gap = si > 0
                        && matches!(track.segments[si - 1], Segment::Gap(_))
                        && ph > s - secs_f64(&track.segments[si - 1].duration()) + 1e-9;
                    if in_gap {
                        Ok(CommitKind::GrowHeadIntoGap)
                    } else {
                        Ok(CommitKind::AbsorbHead)
                    }
                } else {
                    Ok(CommitKind::Delete)
                }
            } else if ph < edge_pos && ph > s + 1e-9 {
                Ok(CommitKind::TrimTail)
            } else if ph > edge_pos {
                // Strictly inside the adjacent gap: plain grow. At or past
                // its far edge the gap is fully spanned, so absorb (which
                // removes it and trims/removes any clip the span reaches).
                let in_gap = si + 1 < track.segments.len()
                    && matches!(track.segments[si + 1], Segment::Gap(_))
                    && ph < e + secs_f64(&track.segments[si + 1].duration()) - 1e-9;
                if in_gap {
                    Ok(CommitKind::GrowTailIntoGap)
                } else if si + 1 >= track.segments.len() {
                    // Nothing beyond: past the timeline end.
                    Ok(CommitKind::GrowTailPastEnd)
                } else {
                    Ok(CommitKind::AbsorbTail)
                }
            } else {
                Ok(CommitKind::Delete)
            }
        }
        Armed::GapTail { index } => {
            if let Some((s, _)) = gap_span(project, *index, ti)
                && ph <= s + 1e-9
            {
                // At the gap's far end (its start): fully consumed — delete
                // it, rippling shut.
                return Ok(CommitKind::Delete);
            }
            Ok(CommitKind::GapTail)
        }
        Armed::GapHead { index } => {
            if let Some((_, e)) = gap_span(project, *index, ti)
                && ph >= e - 1e-9
            {
                return Ok(CommitKind::Delete);
            }
            Ok(CommitKind::GapHead)
        }
    }
}

/// Map a commit kind to its edit op. The target position does the routing:
/// each op reads positionally from `ph`.
pub fn commit_batch(
    project: &Project,
    ph: f64,
    armed: &Armed,
    kind: CommitKind,
    ripple: bool,
    ti: usize,
) -> Result<Batch, EditError> {
    let (id, head) = match armed {
        Armed::Clip { id, head } => (*id, *head),
        Armed::GapTail { .. } | Armed::GapHead { .. } => (0, false),
    };
    match kind {
        CommitKind::TrimHead => {
            if ripple {
                edit::trim_in_ripple_on(project, ti, ph)
            } else {
                edit::trim_in_on(project, ti, ph)
            }
        }
        CommitKind::TrimTail => {
            if ripple {
                edit::trim_out_ripple_on(project, ti, ph)
            } else {
                edit::trim_out_on(project, ti, ph)
            }
        }
        CommitKind::GrowHeadIntoGap => {
            // Preserve: the existing gap op extends the head while the
            // neighboring gap shrinks (length unchanged). Ripple: extend
            // without touching the gap (followers shift).
            if ripple {
                edit::extend_edge_on(project, ti, id, true, ph)
            } else {
                edit::trim_out_on(project, ti, ph)
            }
        }
        CommitKind::GrowTailIntoGap => {
            if ripple {
                edit::extend_edge_on(project, ti, id, false, ph)
            } else {
                edit::trim_in_on(project, ti, ph)
            }
        }
        CommitKind::GrowTailPastEnd => edit::extend_edge_on(project, ti, id, head, ph),
        // Absorb: e replaces everything covered (gaps removed, clips
        // trimmed/removed); E pushes everything aside via plain extend.
        CommitKind::AbsorbTail | CommitKind::AbsorbHead => {
            if ripple {
                edit::extend_edge_on(project, ti, id, head, ph)
            } else {
                edit::absorb_outward_on(project, ti, id, head, ph)
            }
        }
        CommitKind::GapTail => {
            let Armed::GapTail { index } = armed else {
                return Err(EditError::NoGap);
            };
            edit::extend_gap_tail_on(project, ti, *index, ph, ripple)
        }
        CommitKind::GapHead => {
            let Armed::GapHead { index } = armed else {
                return Err(EditError::NoGap);
            };
            edit::extend_gap_head_on(project, ti, *index, ph, ripple)
        }
        CommitKind::Delete => {
            // The target reached the armed segment's opposite end, so it
            // is fully consumed: delete IT (rippling), not whatever segment
            // the boundary resolves to. Gaps have no clip id — delete by
            // index.
            match armed {
                Armed::GapTail { index } | Armed::GapHead { index } => {
                    edit::delete_segment_on(project, ti, *index, true)
                }
                _ if ripple => edit::delete_clip_ripple_by_id_on(project, ti, id),
                _ => edit::delete_clip_by_id_on(project, ti, id),
            }
        }
        CommitKind::Noop => Err(EditError::NothingToTrim(format!("edge already at {}", ph))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RawFormat, RawProbe, RawStream};
    use crate::time::Seconds;
    use crate::timeline::{Clip, Gap, Segment};

    fn probe_report(duration: &str) -> RawProbe {
        RawProbe {
            streams: vec![RawStream {
                index: 0,
                codec_type: Some("video".into()),
                codec_name: Some("h264".into()),
                width: Some(320),
                height: Some(240),
                r_frame_rate: Some("30/1".into()),
                ..Default::default()
            }],
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    fn clip(id: u32, asset: u32, in_s: i64, out_s: i64) -> Segment {
        Segment::Clip(Clip {
            id,
            asset,
            asset_in: Seconds::from_integer(in_s),
            asset_out: Seconds::from_integer(out_s),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    }

    fn gap(secs: i64) -> Segment {
        Segment::Gap(Gap {
            duration: Seconds::from_integer(secs),
        })
    }

    /// [Gap 1 @0..1] [Clip #1 1..4] [Gap 2 @4..6] [Clip #2 6..8]
    fn fixture() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments =
            vec![gap(1), clip(1, a, 0, 3), gap(2), clip(2, a, 0, 2)];
        p
    }

    #[test]
    fn armed_edge_reads_each_boundary_kind() {
        let p = fixture();
        assert_eq!(
            armed_edge(&p, &Armed::Clip { id: 1, head: true }, 0),
            Ok(1.0)
        );
        assert_eq!(
            armed_edge(&p, &Armed::Clip { id: 1, head: false }, 0),
            Ok(4.0)
        );
        assert_eq!(armed_edge(&p, &Armed::GapHead { index: 2 }, 0), Ok(4.0));
        assert_eq!(armed_edge(&p, &Armed::GapTail { index: 2 }, 0), Ok(6.0));
        assert_eq!(
            armed_edge(&p, &Armed::Clip { id: 9, head: true }, 0),
            Err("clip vanished".to_string())
        );
    }

    #[test]
    fn resolve_armed_prefers_the_segment_starting_at_t() {
        let p = fixture();
        // A clip|gap boundary arms the gap's head; the timeline start arms
        // the leading gap's head (extendable past time 0).
        assert_eq!(
            resolve_armed(&p, 4.0, 0),
            Ok((Armed::GapHead { index: 2 }, 4.0))
        );
        assert_eq!(
            resolve_armed(&p, 0.0, 0),
            Ok((Armed::GapHead { index: 0 }, 0.0))
        );
        // A clip head at a shared cut, and the tail where nothing starts.
        assert_eq!(
            resolve_armed(&p, 1.0, 0),
            Ok((Armed::Clip { id: 1, head: true }, 1.0))
        );
        assert_eq!(
            resolve_armed(&p, 8.0, 0),
            Ok((Armed::Clip { id: 2, head: false }, 8.0))
        );
        // Mid-segment is not a boundary at all.
        assert!(resolve_armed(&p, 2.5, 0).is_err());
    }

    #[test]
    fn plan_commit_routes_the_extend_table() {
        let p = fixture();
        let tail = Armed::Clip { id: 1, head: false };
        let head = Armed::Clip { id: 1, head: true };
        assert_eq!(
            plan_commit(&p, &tail, 4.0, 3.0, 0),
            Ok(CommitKind::TrimTail)
        );
        assert_eq!(
            plan_commit(&p, &tail, 4.0, 5.0, 0),
            Ok(CommitKind::GrowTailIntoGap)
        );
        assert_eq!(
            plan_commit(&p, &tail, 4.0, 6.5, 0),
            Ok(CommitKind::AbsorbTail)
        );
        assert_eq!(plan_commit(&p, &tail, 4.0, 1.0, 0), Ok(CommitKind::Delete));
        assert_eq!(plan_commit(&p, &tail, 4.0, 4.0, 0), Ok(CommitKind::Noop));
        assert_eq!(
            plan_commit(&p, &head, 1.0, 2.0, 0),
            Ok(CommitKind::TrimHead)
        );
        assert_eq!(
            plan_commit(&p, &head, 1.0, 0.5, 0),
            Ok(CommitKind::GrowHeadIntoGap)
        );
    }

    #[test]
    fn commit_batch_deletes_the_armed_clip_at_its_opposite_end() {
        let p = fixture();
        let armed = Armed::Clip { id: 1, head: false };
        let kind = plan_commit(&p, &armed, 4.0, 1.0, 0).unwrap();
        let batch = commit_batch(&p, 1.0, &armed, kind, false, 0).unwrap();
        let mut probe = p.clone();
        for op in &batch.ops {
            op.apply(&mut probe).unwrap();
        }
        let segs = &probe.timeline.video_track().unwrap().segments;
        assert!(
            !segs
                .iter()
                .any(|s| matches!(s, Segment::Clip(c) if c.id == 1)),
            "clip #1 should be gone: {segs:?}"
        );
    }
}
