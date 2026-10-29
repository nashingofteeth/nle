//! Running an `Action` from the TUI and keeping the playhead honest across
//! it: the media anchor, the re-anchor after an edit, the undo/persist step,
//! and the delete/yank keys' shared plumbing.

use std::collections::HashSet;

use nle_core::action::Action;
use nle_core::time::frame_to_secs;
use nle_core::timeline::{Segment as TimelineSegment, Track};
#[cfg(test)]
use nle_core::{Batch, EditError, edit};
use nle_core::{Clip, History, Project, Seconds, Target, clip_span, save_register};

use crate::mpv::Mpv;

use super::croc::CrocAct;
use super::jobs::start_preview;
use super::{
    App, Mode, is_pool, load_pool_preview_at, persist_playhead, pool_pos, pool_sel, reload_project,
    secs, set_pool_sel,
};
/// The clip the playhead sits inside, if any.
pub(crate) fn clip_under(project: &Project, t: f64, ti: usize) -> Option<&Clip> {
    let track = project.timeline.track(ti)?;
    track.clip_at(grid_time(project, t))
}

/// The clip under `t` and the position of its head (start) or tail edge.
/// Shared by the `<` / `>` handling in both modes.
pub(crate) fn clip_edge_under(
    project: &Project,
    t: f64,
    head: bool,
    ti: usize,
) -> Option<(u32, f64)> {
    let c = clip_under(project, t, ti)?;
    let (s, e) = clip_span(project, c.id, ti)?;
    Some((c.id, if head { s } else { e }))
}

/// `<` / `>` (and `,` / `.`) in Normal mode: move the playhead to the
/// head or tail of the clip it is over.
pub(crate) fn seek_clip_end(app: &mut App, player: &dyn Mpv, head: bool) {
    let Some((id, edge)) = clip_edge_under(&app.project, app.playhead, head, app.active()) else {
        app.set_message("playhead is in a gap — clip start/end needs a clip");
        return;
    };
    app.seek_to(player, edge);
    app.set_message(&format!(
        "clip #{id} {}",
        if head { "start" } else { "end" }
    ));
}

/// The clip and source frame under the playhead — the media anchor an
/// edit should preserve. `None` over a gap (or past the end).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MediaAnchor {
    pub(crate) id: u32,
    pub(crate) source: Seconds,
    /// The clip's timeline start before the edit — where a ripple delete
    /// leaves the seam between its neighbours.
    pub(crate) start: f64,
    /// The playhead before the edit — the offset a plain delete keeps
    /// inside the gap it leaves.
    pub(crate) at: f64,
}

/// A position anchor for a gap or transition — neither is media (a gap has no
/// id, a transition is opaque), so the playhead can't be re-found by frame.
/// It is kept where it was while the segment survives; a *removal* ripples and
/// leaves the seam (`start`) between the neighbours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GapAnchor {
    /// The seam a removal leaves: a gap's start, a transition's end.
    pub(crate) start: f64,
    /// The playhead before the edit — kept while a gap or transition is still
    /// under it (an insert, a trim, an effect elsewhere), so those don't slide
    /// the playhead to the seam.
    pub(crate) at: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Anchor {
    Media(MediaAnchor),
    Gap(GapAnchor),
}

/// The gap or transition whose span *ends* exactly at `t`. `index_at` gives
/// the boundary to the follower, but for anchoring the segment the playhead
/// sits *on* is what matters: a blend's far edge is still the blend, and a
/// trim that moves that edge to the playhead must keep the playhead put.
pub(crate) fn gap_ending_at(track: &Track, t: Seconds) -> Option<usize> {
    let mut acc = Seconds::from_integer(0);
    for (i, s) in track.segments.iter().enumerate() {
        acc += s.duration();
        if acc == t && matches!(s, TimelineSegment::Gap(_) | TimelineSegment::Transition(_)) {
            return Some(i);
        }
    }
    None
}

/// Anchor the playhead's position before an edit: the clip + source frame
/// over a clip, or the gap's start over a gap.
pub(crate) fn edit_anchor(project: &Project, playhead: f64, ti: usize) -> Option<Anchor> {
    let track = project.timeline.track(ti)?;
    let t = grid_time(project, playhead);
    let idx = gap_ending_at(track, t).or_else(|| track.index_at(t))?;
    let at = secs(&t);
    match &track.segments[idx] {
        TimelineSegment::Clip(c) => Some(Anchor::Media(MediaAnchor {
            id: c.id,
            // Timeline → source scales by the clip's speed (a 2× clip's
            // timeline second is two source seconds).
            source: c.source_at(t - track.start_of(idx)),
            start: secs(&track.start_of(idx)),
            at,
        })),
        TimelineSegment::Gap(_) => Some(Anchor::Gap(GapAnchor {
            start: secs(&track.start_of(idx)),
            at,
        })),
        // A transition is removed by handing its trimmed spans back, so the
        // neighbours meet at its *end*; a non-removing edit keeps the playhead.
        TimelineSegment::Transition(tr) => Some(Anchor::Gap(GapAnchor {
            start: secs(&(track.start_of(idx) + tr.dur)),
            at,
        })),
    }
}

#[cfg(test)]
pub(crate) fn clip_anchor(project: &Project, playhead: f64) -> Option<MediaAnchor> {
    match edit_anchor(project, playhead, 0) {
        Some(Anchor::Media(a)) => Some(a),
        _ => None,
    }
}

/// The timeline position of `source` in clip `id`, clamped to the clip's
/// current extent. `None` when the clip is gone.
pub(crate) fn resolve_anchor(
    project: &Project,
    id: u32,
    source: Seconds,
    ti: usize,
) -> Option<f64> {
    let track = project.timeline.track(ti)?;
    let mut acc = Seconds::from_integer(0);
    for seg in &track.segments {
        if let TimelineSegment::Clip(c) = seg
            && c.id == id
        {
            let pos = if source < c.asset_in {
                acc
            } else if source >= c.asset_out {
                acc + c.duration()
            } else {
                // Source → timeline divides by speed: the inverse of the
                // mapping `edit_anchor` used to record the media frame.
                acc + c.timeline_offset_of(source)
            };
            return Some(secs(&pos));
        }
        acc += seg.duration();
    }
    None
}

/// After an edit, put the playhead back on the media frame it was on
/// (`anchor`), so a trim that moves material under it keeps the same
/// frame rather than the same timecode.
pub(crate) fn reanchor_playhead(app: &mut App, anchor: Option<Anchor>) {
    let Some(anchor) = anchor else { return };
    let pos = match anchor {
        Anchor::Media(a) => resolve_anchor(&app.project, a.id, a.source, app.active())
            .unwrap_or_else(|| fallback_position(&app.project, &a, app.active())),
        // A gap or transition still under the playhead: an edit that doesn't
        // remove it keeps the playhead put. A removal leaves the seam between
        // the neighbours (the segment under `at` is then a clip).
        Anchor::Gap(g) => {
            let ti = app.active();
            let idx = app.project.timeline.track(ti).and_then(|track| {
                let t = grid_time(&app.project, g.at);
                gap_ending_at(track, t).or_else(|| track.index_at(t))
            });
            match idx {
                Some(i)
                    if !matches!(
                        app.project.timeline.tracks[ti].segments[i],
                        TimelineSegment::Clip(_)
                    ) =>
                {
                    g.at
                }
                _ => g.start,
            }
        }
    };
    let total = app.last_frame();
    app.playhead = pos.clamp(0.0, total);
}

/// Does this action move the active track's media? If not, the playhead stays
/// where it is — an anchor taken against the old active track would otherwise
/// resolve against the wrong one (`track rm` re-indexes).
pub(crate) fn reanchors(action: &Action) -> bool {
    !matches!(
        action,
        Action::TrackAdd { .. }
            | Action::TrackMute { .. }
            | Action::TrackRm { .. }
            | Action::AssetRm { .. }
    )
}

/// Where the playhead lands when the anchored clip is gone. A plain
/// delete replaces it with a gap of the same length, so keep the same
/// offset (`at`); a ripple delete removes it and closes the gap, so land
/// on the cut the neighbours now share (`start`).
pub(crate) fn fallback_position(project: &Project, a: &MediaAnchor, ti: usize) -> f64 {
    if let Some(track) = project.timeline.track(ti)
        && let Some(idx) = track.index_at(grid_time(project, a.at))
        && matches!(track.segments[idx], TimelineSegment::Gap(_))
    {
        return a.at;
    }
    a.start
}

/// The playhead's position on the project's frame grid, as Seconds.
pub(crate) fn grid_time(project: &Project, t: f64) -> Seconds {
    nle_core::grid_time(project.fps.as_ref(), t)
}

/// The trim keys' routing, as the key handlers call it (tests drive the
/// shared [`edit::trim_at_on`] through this shim).
#[cfg(test)]
pub(crate) fn trim_key(
    project: &Project,
    ph: f64,
    out: bool,
    ripple: bool,
    ti: usize,
) -> Result<Batch, EditError> {
    edit::trim_at_on(project, ti, ph, out, ripple)
}

/// Run one catalog action — the single path every edit takes, keybind or
/// `:cmd`: apply through the undo history, reanchor the playhead, persist,
/// refresh the monitor. Returns whether it applied (the action set its own
/// notice either way).
/// What a delete can take away: clip/transition ids and the segment count.
/// `dd` on a clip leaves a gap (the count stays), so ids are needed; deleting
/// a gap keeps the ids, so the count is needed too.
pub(crate) struct TimelineStamp {
    pub(crate) ids: HashSet<u32>,
    pub(crate) segments: usize,
}

pub(crate) fn timeline_stamp(project: &Project) -> TimelineStamp {
    let mut stamp = TimelineStamp {
        ids: HashSet::new(),
        segments: 0,
    };
    for track in &project.timeline.tracks {
        for seg in &track.segments {
            stamp.segments += 1;
            match seg {
                TimelineSegment::Clip(c) => {
                    stamp.ids.insert(c.id);
                }
                TimelineSegment::Transition(t) => {
                    stamp.ids.insert(t.id);
                }
                TimelineSegment::Gap(_) => {}
            }
        }
    }
    stamp
}

/// Did an edit remove material? A clip/transition id vanished, or the segment
/// count fell. Trims that only shorten, splits, and inserts don't.
pub(crate) fn removed_material(before: &TimelineStamp, after: &TimelineStamp) -> bool {
    after.segments < before.segments || before.ids.iter().any(|id| !after.ids.contains(id))
}

/// Which crocodile reaction an accepted edit earns. `removed` is the
/// before/after stamp; `old_speed` is only needed to tell a speed-up
/// (`DashFast`) from a slow-down (`DashSlow`).
pub(crate) fn reaction_for(
    action: &Action,
    removed: bool,
    old_speed: Option<Seconds>,
) -> Option<CrocAct> {
    // Undo always gives the previous state back — a spit, even when what it
    // removes is a freshly-pasted clip.
    if matches!(action, Action::Undo) {
        return Some(CrocAct::Spit);
    }
    // Redo is the spit played backwards, regardless of what the change was.
    if matches!(action, Action::Redo) {
        return Some(CrocAct::Unspit);
    }
    if removed {
        return Some(CrocAct::Chomp);
    }
    match action {
        Action::TrimIn { .. } | Action::TrimOut { .. } => Some(CrocAct::Nibble),
        Action::Split { .. } => Some(CrocAct::Snap),
        Action::InsertGap { .. }
        | Action::Paste { .. }
        | Action::Extend { .. }
        | Action::TrackAdd { .. } => Some(CrocAct::Gulp),
        Action::SetSpeed { rate, .. } => {
            let old = old_speed?;
            let (new, old) = (secs(rate), secs(&old));
            if new > old {
                Some(CrocAct::DashFast)
            } else if new < old {
                Some(CrocAct::DashSlow)
            } else {
                None
            }
        }
        Action::Transition { .. } => Some(CrocAct::Slither),
        Action::AddEffect { .. } | Action::RemoveEffect { .. } | Action::ClearEffects { .. } => {
            Some(CrocAct::Wink)
        }
        Action::TrackMute { .. } => Some(CrocAct::Sleep),
        Action::AssetRm { .. } => Some(CrocAct::Chomp),
        Action::PurgeStumps { .. } => Some(CrocAct::Nibble),
        Action::Delete { .. } | Action::TrackRm { .. } | Action::Yank { .. } => None,
        Action::Undo | Action::Redo => unreachable!("handled above"),
    }
}

fn speed_of(project: &Project, id: u32) -> Option<Seconds> {
    project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.segments)
        .find_map(|seg| match seg {
            TimelineSegment::Clip(c) if c.id == id => Some(c.speed),
            _ => None,
        })
}

pub(crate) fn run_action(app: &mut App, player: &dyn Mpv, action: &Action) -> bool {
    // What the timeline holds before the edit — a removal opens the crocodile's
    // mouth in the status row (the chomp is transient, not a message).
    let stamp = timeline_stamp(&app.project);
    let speed_before = match action {
        Action::SetSpeed { clip, .. } => speed_of(&app.project, *clip),
        _ => None,
    };
    // Keep the playhead on the frame it was showing, not the same
    // timecode (a trim can move material under it). Undo/redo carry their
    // own restore position; an extend commit is made from the armed edge,
    // and lands on the edge it moved to.
    let restore = matches!(action, Action::Undo | Action::Redo);
    let in_extend = matches!(app.mode, Mode::Extend(_));
    let anchor = (!in_extend && !restore)
        .then(|| edit_anchor(&app.project, app.playhead, app.active()))
        .flatten();
    let from = match &app.mode {
        Mode::Extend(st) => st.edge_pos,
        _ => app.playhead,
    };
    match app.project.edit(action, from, &mut app.register) {
        Ok(outcome) => {
            match outcome.playhead {
                Some(frame) => {
                    // Clamp: a redo can restore a position into a shorter
                    // timeline.
                    app.playhead =
                        frame_to_secs(app.project.fps.as_ref(), frame).clamp(0.0, app.last_frame());
                }
                // A no-op (a yank) must not move the playhead: the anchor
                // maps a transition to its *end*, so reanchoring a yank out
                // of a blend would slide the playhead — and `dd` yanks first.
                None if outcome.changed && reanchors(action) => reanchor_playhead(app, anchor),
                // A track or pool change doesn't move the active track's
                // media, so leave the playhead where it is (just clamped): a
                // `track rm` re-indexes, and an anchor taken against the old
                // active would resolve against the wrong track.
                None if outcome.changed => {
                    app.playhead = app.playhead.clamp(0.0, app.last_frame());
                }
                None => {}
            }
            if outcome.changed {
                let removed = removed_material(&stamp, &timeline_stamp(&app.project));
                if let Some(act) = reaction_for(action, removed, speed_before) {
                    app.croc.play(act);
                }
            }
            app.set_message(&outcome.desc);
            // The register is shared with the CLI (`nle yank`/`nle paste`).
            if save_register(&app.path, app.register.as_ref()).is_ok() {
                app.note_register();
            }
            if outcome.changed {
                persist_and_refresh(app, player);
            }
            true
        }
        Err(e) => {
            app.fail(&crate::edit_message(&e));
            false
        }
    }
}

/// Atomic save + view refresh. Called after every successful edit (and
/// undo/redo) — no manual save state to get wrong. The undo history sidecar
/// is written alongside the project, then the monitor is reloaded for
/// whichever view is showing: the pool's raw file when the pool is open, or
/// the timeline EDL (with the playhead baked into the load) otherwise.
pub(crate) fn persist_and_refresh(app: &mut App, player: &dyn Mpv) {
    // The stored playhead follows the session — it is what `nle seek`
    // reads and where the next TUI session opens.
    match persist_playhead(app) {
        Ok(true) => {}
        Ok(false) => {
            // A shell wrote the project first: adopt theirs rather than
            // clobber it, and say so — this edit was not saved.
            if let Err(e) = reload_project(app, player, true) {
                app.fail(&format!("reload failed: {e}"));
                return;
            }
            let prev = app.message.clone();
            app.fail(&format!(
                "{prev} — not saved: nle.json changed on disk (reloaded)"
            ));
            return;
        }
        Err(e) => {
            app.fail(&format!("save failed: {e}"));
            return;
        }
    }
    if let Err(e) = app.project.save_history(&History::sidecar_path(&app.path)) {
        app.fail(&format!("history save failed: {e}"));
    } else {
        app.note_history();
    }
    if is_pool(app) {
        // A remove/undo changed the list; keep the selection in range and
        // reload the (possibly different) asset. Doing this here is why an
        // undo while the pool is open no longer kicks the monitor to the
        // timeline.
        let sel = pool_sel(app)
            .unwrap_or(0)
            .min(app.project.media.len().saturating_sub(1));
        set_pool_sel(app, sel);
        crate::log::line(&format!("load pool preview #{sel}"));
        // An undo/edit shouldn't rewind the preview; clamp the old clock to
        // the (possibly different) selected asset.
        let pos = pool_pos(app);
        load_pool_preview_at(app, player, pos);
    } else {
        // Proxy renders are seconds of ffmpeg: build the EDL off-thread and
        // let the status row show progress. A newer edit supersedes this one.
        start_preview(app);
    }
}

pub(crate) fn edit_delete(app: &mut App, player: &dyn Mpv) {
    edit_yank(app, player); // dd yanks first (vim: dd fills the register)
    run_action(
        app,
        player,
        &Action::Delete {
            track: app.active(),
            target: Target::At(app.playhead),
            ripple: false,
        },
    );
}

pub(crate) fn edit_yank(app: &mut App, player: &dyn Mpv) {
    run_action(
        app,
        player,
        &Action::Yank {
            track: app.active(),
            target: Target::At(app.playhead),
        },
    );
}
