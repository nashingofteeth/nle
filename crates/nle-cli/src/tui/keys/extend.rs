//! Extend mode: arm one edge and move it past the neighbour. The planning
//! half (`plan_for_armed`) is pure; the apply half commits one `Batch`.

use crossterm::event::{KeyCode, KeyEvent};

use nle_core::Action;
use nle_core::timeline::{Segment as TimelineSegment, Track, TransitionSource};
use nle_core::{
    Armed, CommitKind, Edge, Project, Seconds, Target, clip_span, commit_batch, gap_span,
    plan_commit, resolve_armed,
};
#[cfg(test)]
use nle_core::{Batch, EditError};

use crate::mpv::Mpv;
use crate::preview;

use super::super::actions::run_action;
use super::super::jobs::start_preview;
use super::super::motion::{
    cut_points, half_frame, next_clip_start, next_cut, next_gap_start, prev_clip_start, prev_cut,
    prev_gap_start,
};
use super::super::{App, ExtendState, Mode, Pending, secs};
use super::count_digit;
/// Plan an extend-mode session: armed boundary, committable range,
/// material span for display, and preview parameters. Pure — the caller
/// realizes it with mpv IPC. Positions below are timeline seconds;
/// `hi == None` means unbounded (gap extension past the end).
pub(crate) struct ExtendPlan {
    pub(crate) track: usize,
    pub(crate) armed: Armed,
    pub(crate) edge_pos: f64,
    pub(crate) lo: f64,
    pub(crate) hi: Option<f64>,
    pub(crate) mat: Option<(f64, f64)>,
    pub(crate) preview: ExtendPreviewSpec,
}

pub(crate) enum ExtendPreviewSpec {
    /// Asset path + asset-relative start/length + timeline origin.
    Clip {
        path: std::path::PathBuf,
        src_start: Seconds,
        length: Seconds,
        origin: f64,
    },
    Black {
        origin: f64,
    },
}

pub(crate) fn plan_extend(
    project: &Project,
    playhead: f64,
    ripple: bool,
    ti: usize,
) -> Result<ExtendPlan, String> {
    let _ = ripple;
    let (armed, edge) = resolve_armed(project, playhead, ti)?;
    plan_for_armed(project, armed, edge, ti)
}

/// Build the plan for a known armed boundary at `edge`. Split from
/// `plan_extend` so extend mode can re-arm to the other side of a shared
/// cut without re-resolving from a playhead.
pub(crate) fn plan_for_armed(
    project: &Project,
    armed: Armed,
    edge: f64,
    ti: usize,
) -> Result<ExtendPlan, String> {
    let track = project
        .timeline
        .track(ti)
        .ok_or_else(|| "empty timeline".to_string())?;

    match armed {
        Armed::Clip { id, head } => {
            let (s, e) = clip_span(project, id, ti).ok_or_else(|| "clip vanished".to_string())?;
            let asset_id = track
                .segments
                .iter()
                .filter_map(|seg| match seg {
                    TimelineSegment::Clip(c) if c.id == id => Some(c),
                    _ => None,
                })
                .next()
                .ok_or_else(|| "clip vanished".to_string())?;
            let asset = project
                .asset(asset_id.asset)
                .ok_or_else(|| format!("asset #{} is missing", asset_id.asset))?;
            let ain = secs(&asset_id.asset_in);
            let aout = secs(&asset_id.asset_out);
            let idx = track
                .clip_index(id)
                .ok_or_else(|| "clip vanished".to_string())?;
            let dur_opt = asset.duration().map(|d| secs(&d));
            // Unknown asset duration: no extension room, trim/delete only,
            // and no material strip to show.
            let (lo, hi, mat) = if head {
                // Head material reaches back to source zero. A segment at
                // the timeline start (s == 0) can extend PAST it: the
                // material is prepended (the segment keeps its start and
                // grows, followers shift right), so its lower bound goes
                // negative by the head room. Anywhere further right is
                // capped at 0 — nothing exists left of the origin.
                let lo = if s <= 0.0 { -ain } else { (s - ain).max(0.0) };
                (lo, Some(e), dur_opt.map(|d| (s - ain, s + (d - ain))))
            } else {
                // Tail material reaches forward to the asset end.
                (
                    s,
                    Some(dur_opt.map(|d| e + (d - aout)).unwrap_or(e)),
                    dur_opt.map(|d| (e - aout, e + (d - aout))),
                )
            };
            // At a blend's far end the edge's material is the transition
            // side's outer source, not the clip's visible in/out: red's tail
            // shows `a.src_out`, blue's head `b.src_in`. Without this the
            // monitor sat one blend-duration off the armed edge.
            let src_at_edge =
                blend_edge_source(track, idx, edge, head).unwrap_or(if head { ain } else { aout });
            // Preview: the whole asset file, so every reachable position
            // shows real frames. mpv 0 maps to the timeline position of
            // source zero (edge minus the edge's source point). Without a
            // known duration, map just the clip's current span.
            let preview = match dur_opt {
                Some(d) => {
                    // Trim the preview to the reachable side so mpv stops
                    // at the segment's edge instead of playing the whole
                    // asset: a head reaches source [0, aout], a tail
                    // [ain, d].
                    let length = if head { aout } else { d };
                    ExtendPreviewSpec::Clip {
                        path: asset.path.clone(),
                        src_start: Seconds::from_integer(0),
                        length: Seconds::new((length * 1e6).round() as i64, 1_000_000),
                        origin: edge - src_at_edge,
                    }
                }
                None => ExtendPreviewSpec::Clip {
                    path: asset.path.clone(),
                    src_start: Seconds::new((ain * 1e6).round() as i64, 1_000_000),
                    length: Seconds::new(((aout - ain) * 1e6).round() as i64, 1_000_000),
                    origin: s,
                },
            };
            Ok(ExtendPlan {
                track: ti,
                armed,
                edge_pos: edge,
                lo,
                hi,
                mat,
                preview,
            })
        }
        Armed::GapTail { index } => {
            let g = match track.segments.get(index) {
                Some(TimelineSegment::Gap(g)) => g,
                _ => return Err("gap vanished".to_string()),
            };
            let gap_start = secs(&track.start_of(index));
            Ok(ExtendPlan {
                track: ti,
                armed,
                edge_pos: gap_start + secs(&g.duration),
                lo: gap_start,
                hi: None,
                mat: None,
                preview: ExtendPreviewSpec::Black { origin: gap_start },
            })
        }
        Armed::GapHead { index } => {
            let g = match track.segments.get(index) {
                Some(TimelineSegment::Gap(g)) => g,
                _ => return Err("gap vanished".to_string()),
            };
            let gap_start = secs(&track.start_of(index));
            // Gaps have no material, so the head reaches back without
            // bound: extending it left inserts empty space (growing the
            // gap), even past time 0. The gap's own end bounds shrinking.
            Ok(ExtendPlan {
                track: ti,
                armed,
                edge_pos: gap_start,
                lo: f64::NEG_INFINITY,
                hi: Some(gap_start + secs(&g.duration)),
                mat: None,
                preview: ExtendPreviewSpec::Black { origin: 0.0 },
            })
        }
    }
}

pub(crate) fn extend_state(app: &App) -> Option<ExtendState> {
    match &app.mode {
        Mode::Extend(st) => Some(st.clone()),
        _ => None,
    }
}

pub(crate) fn clamp_extend(st: &ExtendState, t: f64) -> f64 {
    t.clamp(st.lo, st.hi.unwrap_or(f64::INFINITY))
}

/// Clamp `t` into the extend range AND land it on the project's frame
/// grid. The material-end bound is off-grid (it derives from the asset's
/// duration), but the commit ops snap to frames internally — leaving the
/// playhead unsnapped would park it a sub-frame away from the cut the
/// commit actually creates (red indicator, "no clip ends here"). Snapping
/// can step just outside the range, so pull back to the nearest in-range
/// frame.
pub(crate) fn snap_extend_target(fps: Option<&Seconds>, st: &ExtendState, t: f64) -> f64 {
    // No floor at 0: a timeline-start segment can extend left of it
    // (material prepended), so `st.lo` may be negative.
    let clamped = clamp_extend(st, t);
    let Some(fps) = fps else { return clamped };
    let f = secs(fps);
    let mut target = (clamped * f).round() / f;
    if let Some(hi) = st.hi
        && target > hi
    {
        target = (hi * f).floor() / f;
    }
    if target < st.lo {
        target = (st.lo * f).ceil() / f;
    }
    target
}

pub(crate) fn armed_desc(armed: &Armed) -> String {
    match armed {
        Armed::Clip { id, head } => {
            format!("{} of clip #{id}", if *head { "head" } else { "tail" })
        }
        Armed::GapTail { .. } => "gap end".to_string(),
        Armed::GapHead { .. } => "gap start".to_string(),
    }
}

/// Enter extend mode: resolve the boundary, build the material preview,
/// and hand mpv over to it. Stays in Normal on any failure.
pub(crate) fn enter_extend(app: &mut App, player: &dyn Mpv, ripple: bool) {
    let plan = match plan_extend(&app.project, app.playhead, ripple, app.active()) {
        Err(msg) => {
            app.set_message(&msg);
            return;
        }
        Ok(p) => p,
    };
    // `resolve_armed` hops past a blend beside the cut; say so, or the playhead
    // moving to the blend's far end reads as a jump.
    let hopped = (plan.edge_pos - app.playhead).abs() > 1e-6
        && at_blend_edge(&app.project, app.active(), app.playhead);
    // Land on the armed edge: a gap on the right presents its tail, which
    // sits away from the cut `e` was pressed at.
    app.playhead = plan.edge_pos;
    apply_extend_plan(app, player, plan, ripple);
    if hopped {
        app.message
            .push_str(" — the blend is part of the clip — armed its far end");
    }
}

/// Hand the monitor over to an extend plan's material preview and enter
/// the mode. Shared by entry and by swapping sides at a shared cut.
pub(crate) fn apply_extend_plan(app: &mut App, player: &dyn Mpv, plan: ExtendPlan, ripple: bool) {
    let (url, origin) = match &plan.preview {
        ExtendPreviewSpec::Clip {
            path,
            src_start,
            length,
            origin,
        } => {
            let pv = preview::build_clip_preview(path, *src_start, *length, *origin);
            (pv.url, pv.origin)
        }
        ExtendPreviewSpec::Black { origin } => {
            // Bounded ranges (leading gap) cover just the gap; unbounded
            // ones (trailing gap) reach an hour out via repeated chunks.
            let length = match plan.hi {
                Some(hi) => Seconds::new(((hi - origin) * 1e6).round() as i64, 1_000_000),
                None => Seconds::from_integer(preview::EXTEND_PREVIEW_SECS),
            };
            match preview::build_black_preview(&app.project, *origin, length) {
                Err(e) => {
                    app.set_message(&format!("preview failed: {e}"));
                    return;
                }
                Ok(pv) => (pv.url, pv.origin),
            }
        }
    };
    player.load(&url, Some((app.playhead - origin).max(0.0)));
    // The material preview is not the pooled file; its clock maps back
    // through `preview_off` (see `sync`).
    app.pool_on_monitor = false;
    app.pool_handoff = None;
    let flag = if ripple { " · RIPPLE" } else { "" };
    app.set_message(&format!(
        "extend {}{flag} — navigate, e to commit",
        armed_desc(&plan.armed)
    ));
    app.mode = Mode::Extend(ExtendState {
        track: plan.track,
        armed: plan.armed,
        edge_pos: plan.edge_pos,
        ripple,
        lo: plan.lo,
        hi: plan.hi,
        mat: plan.mat,
        preview_off: origin,
    });
}

/// Which clip boundary of a cut to work on: the clip that ENDS there
/// (its tail) or the clip that STARTS there (its head). At a clip|clip
/// cut both exist; against a gap only one side has a clip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Side {
    Left,
    Right,
}

/// The clip boundary on `side` of the cut at `t`, with its exact edge
/// position. `None` when no clip sits on that side (a gap, or nothing).
pub(crate) fn armed_on_side(
    project: &Project,
    t: f64,
    side: Side,
    ti: usize,
) -> Option<(Armed, f64)> {
    let track = project.timeline.track(ti)?;
    let eps = 1e-6;
    let mut acc = 0.0;
    for (i, seg) in track.segments.iter().enumerate() {
        let start = acc;
        acc += secs(&seg.duration());
        let end = acc;
        match seg {
            // A blend's far edge is the neighbour's real edge: the clip
            // before it ends there (`]`), the clip after it starts there
            // (`[`). At its near edge the same bracket arms the same clip
            // and `blend_far_end` hops across afterwards.
            TimelineSegment::Transition(_) => match side {
                Side::Left if (end - t).abs() < eps => {
                    if let Some(TimelineSegment::Clip(c)) =
                        i.checked_sub(1).and_then(|p| track.segments.get(p))
                    {
                        return Some((
                            Armed::Clip {
                                id: c.id,
                                head: false,
                            },
                            t,
                        ));
                    }
                }
                Side::Right if (start - t).abs() < eps => {
                    if let Some(TimelineSegment::Clip(c)) = track.segments.get(i + 1) {
                        return Some((
                            Armed::Clip {
                                id: c.id,
                                head: true,
                            },
                            t,
                        ));
                    }
                }
                _ => {}
            },
            TimelineSegment::Clip(c) => match side {
                Side::Left if (end - t).abs() < eps => {
                    return Some((
                        Armed::Clip {
                            id: c.id,
                            head: false,
                        },
                        end,
                    ));
                }
                Side::Right if (start - t).abs() < eps => {
                    return Some((
                        Armed::Clip {
                            id: c.id,
                            head: true,
                        },
                        start,
                    ));
                }
                _ => {}
            },
            // A gap presents the edge at the cut: its tail where it ends
            // (left side) or its head where it starts (right side).
            TimelineSegment::Gap(_) => match side {
                Side::Left if (end - t).abs() < eps => {
                    return Some((Armed::GapTail { index: i }, end));
                }
                Side::Right if (start - t).abs() < eps => {
                    return Some((Armed::GapHead { index: i }, start));
                }
                _ => {}
            },
        }
    }
    None
}

/// Is `t` a blend's edge — the cut between a clip and a transition?
pub(crate) fn at_blend_edge(project: &Project, ti: usize, t: f64) -> bool {
    let Some(track) = project.timeline.track(ti) else {
        return false;
    };
    let mut acc = 0.0;
    for seg in &track.segments {
        if let TimelineSegment::Transition(_) = seg
            && (acc - t).abs() < 1e-6
        {
            return true;
        }
        acc += secs(&seg.duration());
        if let TimelineSegment::Transition(_) = seg
            && (acc - t).abs() < 1e-6
        {
            return true;
        }
    }
    false
}

/// The far end of the blend beside a clip's armed edge, if one is there. A
/// transition is part of both neighbours, so entering extend mode at a
/// clip|transition cut hops past it to the clip's real edge.
pub(crate) fn blend_far_end(
    project: &Project,
    ti: usize,
    armed: &Armed,
    edge: f64,
) -> Option<(f64, &'static str)> {
    let track = project.timeline.track(ti)?;
    let Armed::Clip { id, head } = armed else {
        return None;
    };
    let idx = track.clip_index(*id)?;
    // The blend sits on the side the armed edge opens toward: a head reaches
    // left, a tail right.
    let ni = if *head { idx.checked_sub(1)? } else { idx + 1 };
    let TimelineSegment::Transition(tr) = track.segments.get(ni)? else {
        return None;
    };
    let (near, far) = (
        secs(&track.start_of(ni)),
        secs(&(track.start_of(ni) + tr.dur)),
    );
    // The edge the clip's edge touches, and the blend's far end past it.
    let (want, far_end) = if *head { (far, near) } else { (near, far) };
    ((edge - want).abs() < 1e-6)
        .then_some((far_end, "the blend is part of the clip — armed its far end"))
}

/// The material at a blend's far edge: the preceding clip's tail ends on
/// `a.src_out` (the blend's end), the following clip's head starts on
/// `b.src_in` (its start). `None` when the edge isn't a blend's far end or
/// that side is black.
pub(crate) fn blend_edge_source(
    track: &Track,
    clip_idx: usize,
    edge: f64,
    head: bool,
) -> Option<f64> {
    let ni = if head {
        clip_idx.checked_sub(1)?
    } else {
        clip_idx + 1
    };
    let TimelineSegment::Transition(tr) = track.segments.get(ni)? else {
        return None;
    };
    let near = secs(&track.start_of(ni));
    let far = near + secs(&tr.dur);
    // A tail's edge is the blend's end; a head's is its start.
    let (want, side) = if head { (near, &tr.b) } else { (far, &tr.a) };
    if (edge - want).abs() > 1e-6 {
        return None;
    }
    match side {
        TransitionSource::Clip {
            src_in, src_out, ..
        } => Some(secs(if head { src_in } else { src_out })),
        TransitionSource::Black => None,
    }
}

pub(crate) fn side_hint(side: Side) -> &'static str {
    // Name the *other* bracket: the hint fires on the key that just failed.
    match side {
        Side::Left => "no clip ends here — [ works the clip on the right",
        Side::Right => "no clip starts here — ] works the clip on the left",
    }
}

/// `[` / `]` at a cut: enter extend mode already armed on the clip on
/// that side — `[` opens right, so it works the clip starting (right)
/// there; `]` opens left, so the clip ending (left). `e`/`E` keep the
/// default (right at a shared cut).
pub(crate) fn enter_extend_side(app: &mut App, player: &dyn Mpv, side: Side) {
    let Some((armed, mut edge)) = armed_on_side(&app.project, app.playhead, side, app.active())
    else {
        app.set_message(side_hint(side));
        return;
    };
    // The blend beside the cut is part of the clip: arm its far end.
    let note = match blend_far_end(&app.project, app.active(), &armed, edge) {
        Some((far, note)) => {
            edge = far;
            Some(note)
        }
        None => None,
    };
    match plan_for_armed(&app.project, armed, edge, app.active()) {
        Ok(plan) => {
            // Land on the armed edge (a gap presents its tail, which can
            // be away from the cut we armed it from).
            app.playhead = edge;
            apply_extend_plan(app, player, plan, false);
            if let Some(note) = note {
                app.message.push_str(&format!(" — {note}"));
            }
        }
        Err(m) => app.set_message(&m),
    }
}

/// `[` / `]` inside extend mode: move to the clip on the other side of
/// the same cut, keeping the ripple flag and playhead.
pub(crate) fn set_extend_side(app: &mut App, player: &dyn Mpv, side: Side) {
    let Some(st) = extend_state(app) else { return };
    let Some((armed, mut edge)) = armed_on_side(&app.project, st.edge_pos, side, st.track) else {
        app.set_message(side_hint(side));
        return;
    };
    // Same hop as entering the mode: a blend beside the cut is part of the
    // clip, so its edge is the blend's far end.
    if let Some((far, _)) = blend_far_end(&app.project, st.track, &armed, edge) {
        edge = far;
    }
    if armed == st.armed {
        return;
    }
    match plan_for_armed(&app.project, armed, edge, st.track) {
        Ok(plan) => {
            app.playhead = edge;
            apply_extend_plan(app, player, plan, st.ripple);
        }
        Err(m) => app.set_message(&m),
    }
}

/// The armed edge on the requested end of the same segment (clip or gap),
/// and its position. `None` when the segment is gone.
pub(crate) fn flip_target(
    project: &Project,
    armed: Armed,
    head: bool,
    ti: usize,
) -> Option<(Armed, f64)> {
    match armed {
        Armed::Clip { id, .. } => clip_span(project, id, ti)
            .map(|(s, e)| (Armed::Clip { id, head }, if head { s } else { e })),
        Armed::GapHead { index } | Armed::GapTail { index } => {
            gap_span(project, index, ti).map(|(s, e)| {
                if head {
                    (Armed::GapHead { index }, s)
                } else {
                    (Armed::GapTail { index }, e)
                }
            })
        }
    }
}

/// `<` / `>` (and `,` / `.`) inside extend mode: re-anchor the armed edge
/// to the head or tail of the segment being edited (clip or gap), keeping
/// the ripple flag.
pub(crate) fn flip_extend_edge(app: &mut App, player: &dyn Mpv, head: bool) {
    let Some(st) = extend_state(app) else { return };
    let Some((armed, edge)) = flip_target(&app.project, st.armed, head, st.track) else {
        app.set_message("segment is gone");
        return;
    };
    if st.armed == armed {
        return; // already on that end
    }
    match plan_for_armed(&app.project, armed, edge, st.track) {
        Ok(plan) => {
            // Anchor the playhead before handing the monitor over so the
            // material preview opens on the armed source point.
            app.playhead = edge;
            apply_extend_plan(app, player, plan, st.ripple);
        }
        Err(m) => app.set_message(&m),
    }
}

/// Leave extend mode: project timeline back on the monitor, playhead at
/// `pos`. The rebuild rides the preview worker like every other monitor
/// hand-over — a cold proxy is seconds of ffmpeg on the key thread, which
/// froze the whole TUI — so the exit parked below lets `poll_preview` land
/// the EDL. Pause first: an auto-playing EDL streams past the landing and
/// the position check in `sync` would never clear (the same reason
/// `close_pool_at` pauses).
pub(crate) fn exit_extend(app: &mut App, player: &dyn Mpv, pos: f64, msg: &str) {
    app.playhead = pos;
    app.mode = Mode::Normal;
    // The material preview is still (or just now) the monitor's file; its
    // clock must not drive the playhead until the timeline EDL reports its
    // landing. The poll raises `pool_handoff` when it loads.
    app.pool_on_monitor = true;
    app.pool_handoff = None;
    app.pending_pause = Some(true);
    player.set_pause(true);
    start_preview(app);
    app.set_message(msg);
}

/// Map a commit kind to its edit op for the playhead under `app` — the
/// extend commit's routing, as a test-facing shim over [`commit_batch`].
#[cfg(test)]
pub(crate) fn run_commit_action(
    app: &App,
    armed: &Armed,
    kind: CommitKind,
    ripple: bool,
) -> Result<Batch, EditError> {
    commit_batch(
        &app.project,
        app.playhead,
        armed,
        kind,
        ripple,
        app.active(),
    )
}

/// The timeline length if the current extend position were committed.
/// Runs the real op on a scratch copy so every route (trim, absorb,
/// ripple, gap) projects honestly; falls back to the current length when
/// the commit would refuse or leave an invalid timeline.
pub(crate) fn projected_total(app: &App, st: &ExtendState) -> f64 {
    let fallback = app.total();
    let Ok(kind) = plan_commit(&app.project, &st.armed, st.edge_pos, app.playhead, st.track) else {
        return fallback;
    };
    if kind == CommitKind::Noop {
        return fallback;
    }
    let Ok(batch) = commit_batch(
        &app.project,
        app.playhead,
        &st.armed,
        kind,
        st.ripple,
        st.track,
    ) else {
        return fallback;
    };
    let mut probe = app.project.clone();
    for op in &batch.ops {
        if op.apply(&mut probe).is_err() || probe.validate().is_err() {
            return fallback;
        }
    }
    secs(&probe.timeline.end())
}

/// Map an armed boundary to the catalog's addressing — extend mode
/// resolved it interactively, and the action resolves it back the same
/// way (gap edges by segment index, clip edges by id).
pub(crate) fn target_of_armed(armed: &Armed) -> (Target, Edge) {
    match armed {
        Armed::Clip { id, head } => (Target::Clip(*id), if *head { Edge::In } else { Edge::Out }),
        Armed::GapHead { index } => (Target::Segment(*index), Edge::In),
        Armed::GapTail { index } => (Target::Segment(*index), Edge::Out),
    }
}

/// Commit: run the extend as one undo step and leave the mode. Failures
/// keep the mode (and its preview) so the position isn't lost.
pub(crate) fn commit_extend(app: &mut App, player: &dyn Mpv) {
    let Some(st) = extend_state(app) else { return };
    let kind = match plan_commit(&app.project, &st.armed, st.edge_pos, app.playhead, st.track) {
        Err(msg) => {
            app.set_message(&msg);
            return;
        }
        Ok(k) => k,
    };
    if kind == CommitKind::Noop {
        exit_extend(app, player, st.edge_pos, "edge already there");
        return;
    }
    let (target, edge) = target_of_armed(&st.armed);
    if run_action(
        app,
        player,
        &Action::Extend {
            track: st.track,
            target,
            edge,
            to: app.playhead,
            ripple: st.ripple,
        },
    ) {
        app.mode = Mode::Normal;
        // The monitor still shows the material preview while the post-edit
        // rebuild renders, and translating its clock back needs extend
        // mode's `preview_off`. Raise the hand-off so `sync` ignores mpv
        // until the timeline EDL lands, and pause like `close_pool_at`:
        // an auto-playing EDL reports past the landing and the check would
        // never clear. Resume is one press of `space`.
        app.pool_on_monitor = true;
        app.pool_handoff = None;
        app.pending_pause = Some(true);
        player.set_pause(true);
    }
}

/// Cancel: snap the playhead back to the armed edge, project timeline
/// back on the monitor.
pub(crate) fn cancel_extend(app: &mut App, player: &dyn Mpv) {
    let Some(st) = extend_state(app) else { return };
    exit_extend(app, player, st.edge_pos, "extend cancelled");
}

/// Mode-aware absolute seek: clamped to the committable range, translated
/// through the preview offset for mpv.
pub(crate) fn seek_to_mode(app: &mut App, player: &dyn Mpv, st: &ExtendState, t: f64) {
    if !app.guard_loaded() {
        return;
    }
    let target = snap_extend_target(app.project.fps.as_ref(), st, t);
    app.playhead = target;
    // mpv can't seek before its file start; the gap head can push the
    // mapped position negative, so clamp (the preview is black anyway).
    player.seek_absolute((target - st.preview_off + 0.004).max(0.0));
}

pub(crate) fn seek_rel_mode(app: &mut App, player: &dyn Mpv, st: &ExtendState, delta: f64) {
    let target = app.playhead + delta;
    seek_to_mode(app, player, st, target);
}

/// Frame index a frame-step from `playhead` lands on, clamped to the armed
/// range. `lo` may be negative (head material prepended past time 0) — do
/// not re-floor it here.
pub(crate) fn extend_step_index(
    lo: f64,
    hi: Option<f64>,
    fps: Seconds,
    playhead: f64,
    dir: i64,
    mult: u32,
) -> i64 {
    let f = secs(&fps);
    let lo_i = (lo * f).round() as i64;
    let hi_i = hi.map(|h| (h * f).round() as i64).unwrap_or(i64::MAX);
    ((playhead * f).round() as i64 + dir * mult as i64).clamp(lo_i, hi_i)
}

/// Mode-aware frame step: the frame index is clamped to the range, and
/// mpv only steps when the model actually moved (no divergence).
pub(crate) fn step_mode(app: &mut App, player: &dyn Mpv, st: &ExtendState, dir: i64, mult: u32) {
    if !app.guard_loaded() {
        return;
    }
    let Some(fps) = app.project.fps.as_ref() else {
        return;
    };
    let idx = extend_step_index(st.lo, st.hi, *fps, app.playhead, dir, mult);
    let next = idx as f64 / secs(fps);
    if next != app.playhead {
        app.playhead = next;
        // Seek mpv to the model frame on the project grid — see the f/F note.
        app.seek_to(player, next);
    }
}

pub(crate) fn extend_inert(app: &mut App) {
    app.set_message("exit extend mode first (Esc)");
}

pub(crate) fn extend_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    let code = key.code;

    // Counts still build for nav keys; bare `0` seeks (clamped below).
    if let KeyCode::Char(c @ '0'..='9') = code {
        if c == '0' && app.count.is_none() {
            if let Some(st) = extend_state(app) {
                seek_to_mode(app, player, &st, 0.0);
            }
            return;
        }
        let d = c.to_digit(10).unwrap_or(0);
        count_digit(app, d);
        return;
    }
    let mult = app.count.take().unwrap_or(1);
    let eps = half_frame(&app.project);

    // `gg` still works (lands on the range start via clamp).
    if let (Pending::G, KeyCode::Char('g')) = (&app.pending, code) {
        app.pending = Pending::None;
        if let Some(st) = extend_state(app) {
            seek_to_mode(app, player, &st, 0.0);
        }
        return;
    }

    let Some(st) = extend_state(app) else { return };
    match code {
        KeyCode::Char(' ') => {
            if app.guard_loaded() {
                player.toggle_pause();
            }
        }
        KeyCode::Char('h') => seek_rel_mode(app, player, &st, -(mult as f64)),
        KeyCode::Char('l') => seek_rel_mode(app, player, &st, mult as f64),
        KeyCode::Char('H') => seek_rel_mode(app, player, &st, -5.0 * mult as f64),
        KeyCode::Char('L') => seek_rel_mode(app, player, &st, 5.0 * mult as f64),
        KeyCode::Char('f') => step_mode(app, player, &st, 1, mult),
        KeyCode::Char('F') => step_mode(app, player, &st, -1, mult),
        KeyCode::Char('j') => {
            match next_cut(&cut_points(&app.project, app.active()), app.playhead, eps) {
                Some(t) => seek_to_mode(app, player, &st, t),
                None => seek_to_mode(app, player, &st, app.total()),
            }
        }
        KeyCode::Char('k') => {
            match prev_cut(&cut_points(&app.project, app.active()), app.playhead, eps) {
                Some(t) => seek_to_mode(app, player, &st, t),
                None => seek_to_mode(app, player, &st, 0.0),
            }
        }
        KeyCode::Char('w') => {
            match next_clip_start(&app.project, app.playhead, eps, app.active()) {
                Some(t) => seek_to_mode(app, player, &st, t),
                None => seek_to_mode(app, player, &st, app.total()),
            }
        }
        KeyCode::Char('b') => {
            match prev_clip_start(&app.project, app.playhead, eps, app.active()) {
                Some(t) => seek_to_mode(app, player, &st, t),
                None => seek_to_mode(app, player, &st, 0.0),
            }
        }
        KeyCode::Char('n') => match next_gap_start(&app.project, app.playhead, eps, app.active()) {
            Some(t) => seek_to_mode(app, player, &st, t),
            None => seek_to_mode(app, player, &st, app.total()),
        },
        KeyCode::Char('N') => match prev_gap_start(&app.project, app.playhead, eps, app.active()) {
            Some(t) => seek_to_mode(app, player, &st, t),
            None => seek_to_mode(app, player, &st, 0.0),
        },
        KeyCode::Char('g') => app.pending = Pending::G,
        KeyCode::Char('G') | KeyCode::Char('$') => seek_to_mode(app, player, &st, app.total()),
        KeyCode::Char('e') => commit_extend(app, player),
        KeyCode::Char('[') => set_extend_side(app, player, Side::Right),
        KeyCode::Char(']') => set_extend_side(app, player, Side::Left),
        KeyCode::Char('<') | KeyCode::Char(',') => flip_extend_edge(app, player, true),
        KeyCode::Char('>') | KeyCode::Char('.') => flip_extend_edge(app, player, false),
        KeyCode::Char('E') => {
            let ripple = match &mut app.mode {
                Mode::Extend(inner) => {
                    inner.ripple = !inner.ripple;
                    inner.ripple
                }
                _ => return,
            };
            app.set_message(if ripple {
                "extend: ripple ON"
            } else {
                "extend: preserve"
            });
        }
        KeyCode::Esc => cancel_extend(app, player),
        KeyCode::Char('q') => cancel_extend(app, player),
        KeyCode::Char(':')
        | KeyCode::Char('r')
        | KeyCode::Char('u')
        | KeyCode::Char('i')
        | KeyCode::Char('I')
        | KeyCode::Char('o')
        | KeyCode::Char('O')
        | KeyCode::Char('s')
        | KeyCode::Char('p')
        | KeyCode::Char('P')
        | KeyCode::Char('d')
        | KeyCode::Char('y') => extend_inert(app),
        _ => {}
    }

    if !matches!(code, KeyCode::Char('g') | KeyCode::Char('0'..='9')) {
        app.pending = Pending::None;
    }
}
