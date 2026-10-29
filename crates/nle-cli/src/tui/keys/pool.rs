//! Media-pool mode: the asset list's keys, the raw-file preview, and the
//! hand-off back to the timeline. `Mode::Pool` keeps "open" and its selection
//! together; the accessors live in the parent module.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use nle_core::Action;
use nle_core::time::Seconds;
use nle_core::timeline::{Clip, Segment as TimelineSegment};

use crate::mpv::Mpv;

use super::super::actions::run_action;
use super::super::croc::CrocAct;
use super::super::jobs::start_preview;
use super::super::keys::completes_gg;
use super::super::keys::count_digit;
use super::super::keys::do_redo;
use super::super::keys::do_undo;
use super::super::keys::select_active_track;
use super::super::{
    App, Mode, Pending, PoolState, is_pool, pool_pos, pool_resume, pool_sel, remember_pool, secs,
    set_pool_pos, set_pool_sel,
};
/// Media-pool panel keys: move the list (loading each asset for preview),
/// insert/remove, and seek/play the preview.
pub(crate) fn pool_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    // Counts build for the seek keys (bare `0` = start of the file).
    if let KeyCode::Char(c @ '0'..='9') = key.code {
        if c == '0' && app.count.is_none() {
            pool_seek_abs(app, player, 0.0);
            return;
        }
        let d = c.to_digit(10).unwrap_or(0);
        count_digit(app, d);
        return;
    }
    let had_count = app.count.is_some();
    let mult = app.count.take().unwrap_or(1);

    // Second key of the `gg` sequence (mirrors normal mode). This must run
    // BEFORE the match below sets the pending state, or a lone `g` would
    // complete itself.
    if completes_gg(app.pending, key.code) {
        app.pending = Pending::None;
        pool_seek_abs(app, player, 0.0);
        return;
    }

    let n = app.project.media.len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('m') => close_pool(app, player),
        // List motion (j/k), list ends ({ }).
        KeyCode::Char('j') | KeyCode::Down => pool_move(app, player, 1, n),
        KeyCode::Char('k') | KeyCode::Up => pool_move(app, player, -1, n),
        KeyCode::Char('{') => pool_select(app, player, 0),
        KeyCode::Char('}') => pool_select(app, player, n.saturating_sub(1)),
        // Preview ends (file, not list): gg / 0 = start, G / $ = end.
        KeyCode::Char('g') => app.pending = Pending::G,
        KeyCode::Char('G') | KeyCode::Char('$') => pool_seek_end(app, player),
        KeyCode::Enter | KeyCode::Char('P') => insert_from_pool(app, player),
        KeyCode::Char('d') => remove_from_pool(app, player),
        // Import a file into the pool: opens the command line pre-filled.
        KeyCode::Char('i') => {
            remember_pool(app);
            app.command_from_pool = true;
            app.mode = Mode::Command("import ".to_string());
        }
        KeyCode::Char('u') => do_undo(app, player),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => do_redo(app, player),
        // Preview transport (asset seconds).
        KeyCode::Char(' ') => pool_toggle_play(app, player),
        KeyCode::Char('h') => pool_seek_rel(app, player, -(mult as f64)),
        KeyCode::Char('l') => pool_seek_rel(app, player, mult as f64),
        KeyCode::Char('H') => pool_seek_rel(app, player, -5.0 * mult as f64),
        KeyCode::Char('L') => pool_seek_rel(app, player, 5.0 * mult as f64),
        KeyCode::Char('f') => pool_step(app, player, 1),
        KeyCode::Char('F') => pool_step(app, player, -1),
        // Track change is available in the pool so `enter` inserts where you
        // mean (the monitor keeps the asset preview; see `pool_set_track`).
        KeyCode::Char('t') => {
            let n = app.project.timeline.tracks.len();
            let idx = if had_count {
                (mult as usize).saturating_sub(1)
            } else {
                (app.active() + 1) % n.max(1)
            };
            pool_set_track(app, idx);
        }
        KeyCode::Char('T') => {
            let n = app.project.timeline.tracks.len();
            let idx = if had_count {
                (mult as usize).saturating_sub(1)
            } else {
                (app.active() + n.max(1) - 1) % n.max(1)
            };
            pool_set_track(app, idx);
        }
        _ => {}
    }

    // `gg` completes to the start of the file (mirrors normal mode).
    // Only `g` carries a pending sequence; any other key clears it. (Digits
    // return above, so they never reach here.)
    if !matches!(key.code, KeyCode::Char('g')) {
        app.pending = Pending::None;
    }
}

/// Seek the pool preview to the selected asset's last frame.
pub(crate) fn pool_seek_end(app: &mut App, player: &dyn Mpv) {
    let d = app
        .project
        .media
        .get(pool_sel(app).unwrap_or(0))
        .and_then(|a| a.duration())
        .map(|d| secs(&d))
        .unwrap_or(0.0);
    pool_seek_abs(app, player, pool_last_frame(app, d));
}

/// Play/pause the pool preview; at the end of the file, restart from 0
/// (mpv `--keep-open` holds at EOF, where a plain unpause would stick).
pub(crate) fn pool_toggle_play(app: &mut App, player: &dyn Mpv) {
    if !app.loaded {
        return;
    }
    if app.eof {
        set_pool_pos(app, 0.0);
        player.seek_absolute(0.0);
        player.set_pause(false);
        app.croc.clear();
    } else {
        let was_paused = app.paused;
        player.toggle_pause();
        if was_paused {
            app.croc.clear();
        } else {
            app.croc.play(CrocAct::Sleep);
        }
    }
}

/// Move the list selection by `delta` and load the new asset.
pub(crate) fn pool_move(app: &mut App, player: &dyn Mpv, delta: i64, n: usize) {
    if n == 0 {
        return;
    }
    let cur = pool_sel(app).unwrap_or(0).min(n - 1) as i64;
    pool_select(app, player, (cur + delta).clamp(0, n as i64 - 1) as usize);
}

/// Select a list row and swap the monitor to that asset.
pub(crate) fn pool_select(app: &mut App, player: &dyn Mpv, sel: usize) {
    if pool_sel(app) == Some(sel) && app.loaded {
        return;
    }
    set_pool_sel(app, sel);
    load_pool_preview(app, player);
}

/// Load the selected asset into the monitor as a raw file from its start.
pub(crate) fn load_pool_preview(app: &mut App, player: &dyn Mpv) {
    load_pool_preview_at(app, player, 0.0);
}

/// Load the selected asset at `start` (clamped to its frames). Used by the
/// reopen snapshot as well as a fresh selection.
pub(crate) fn load_pool_preview_at(app: &mut App, player: &dyn Mpv, start: f64) {
    let Some(sel) = pool_sel(app) else { return };
    let Some(asset) = app.project.media.get(sel) else {
        return;
    };
    let path = asset.path.clone();
    let dur = asset.duration().map(|d| secs(&d)).unwrap_or(0.0);
    let last = pool_last_frame(app, dur);
    let start = if start.is_finite() {
        start.clamp(0.0, last)
    } else {
        0.0
    };
    set_pool_pos(app, start);
    app.pool_on_monitor = true;
    app.pool_handoff = None;
    app.pending_pause = None;
    app.eof = false; // a freshly loaded file is not at EOF
    player.load(&path.display().to_string(), Some(start));
}

/// Open the pool: remember the timeline playhead, preview the last row.
pub(crate) fn open_pool(app: &mut App, player: &dyn Mpv) {
    let len = app.project.media.len();
    // Prefer the remembered asset id: an import/removal may have moved rows.
    let sel = app
        .last_pool_asset
        .and_then(|id| app.project.media.iter().position(|a| a.id == id))
        .unwrap_or_else(|| app.last_pool_sel.min(len.saturating_sub(1)));
    let same_asset = app.project.media.get(sel).map(|a| a.id) == app.last_pool_asset;
    let start = if same_asset { app.last_pool_pos } else { 0.0 };
    app.mode = Mode::Pool(PoolState {
        sel,
        pos: start,
        resume: app.playhead,
    });
    load_pool_preview_at(app, player, start);
    app.set_message("media");
}

/// Close the pool: reload the project timeline at the remembered playhead.
/// A `:` line opened from the pool (`i`) leaves the monitor on the pooled
/// file: put the timeline back on it, as `close_pool` does.
pub(crate) fn leave_pool_command(app: &mut App, player: &dyn Mpv) {
    if !app.command_from_pool {
        return;
    }
    app.command_from_pool = false;
    app.pending_pause = Some(true);
    player.set_pause(true);
    app.reload_requested = true;
}

pub(crate) fn close_pool(app: &mut App, player: &dyn Mpv) {
    close_pool_at(app, player, pool_resume(app));
}

/// Close the pool and put the playhead at `at`: the pre-pool position on a
/// manual close, or the frame an insert landed on. Always hands the timeline
/// back **paused**: mpv is often playing at EOF, and an auto-playing EDL
/// would let the cut magnet move the landing.
pub(crate) fn close_pool_at(app: &mut App, player: &dyn Mpv, at: f64) {
    remember_pool(app);
    app.mode = Mode::Normal;
    app.playhead = at.clamp(0.0, app.last_frame());
    app.pending_pause = Some(true);
    // Set it now so the subsequent async `loadfile` starts paused (mpv keeps
    // the pause property; the pending value re-applies it after the load).
    player.set_pause(true);
    // The monitor was on the pooled asset; rebuild the timeline EDL (from the
    // cache unless something changed) and reload when it is ready.
    start_preview(app);
    app.set_message("timeline");
}

/// Seek the pool preview to an absolute asset time (clamped to duration).
pub(crate) fn pool_seek_abs(app: &mut App, player: &dyn Mpv, t: f64) {
    if !app.loaded {
        return;
    }
    let d = app
        .project
        .media
        .get(pool_sel(app).unwrap_or(0))
        .and_then(|a| a.duration())
        .map(|d| secs(&d))
        .unwrap_or(f64::INFINITY);
    // The last *frame* sits one frame before the duration; seeking to the
    // exact end lands past the final frame (mpv holds there and reports a
    // couple frames short). Clamp to the last frame's timestamp.
    let last = pool_last_frame(app, d);
    let target = t.clamp(0.0, last);
    set_pool_pos(app, target);
    player.seek_absolute(target);
}

/// Timestamp of the selected asset's last frame (`duration − 1 frame`).
pub(crate) fn pool_last_frame(app: &App, dur: f64) -> f64 {
    let fps = app
        .project
        .media
        .get(pool_sel(app).unwrap_or(0))
        .and_then(|a| a.fps())
        .map(|f| secs(&f))
        .unwrap_or(30.0);
    (dur - 1.0 / fps).max(0.0)
}

pub(crate) fn pool_seek_rel(app: &mut App, player: &dyn Mpv, delta: f64) {
    pool_seek_abs(app, player, pool_pos(app) + delta);
}

/// Frame-step the pool preview: move on the asset's frame grid and seek
/// frame-exactly (same model-authoritative approach as normal mode).
pub(crate) fn pool_step(app: &mut App, player: &dyn Mpv, dir: i64) {
    if !app.loaded {
        return;
    }
    let (target, _fps) = pool_step_target(app, dir);
    set_pool_pos(app, target);
    // Nudge a hair into the frame so a frame-exact seek lands ON it, not
    // the previous frame (same trick normal mode uses).
    player.seek_absolute(target + 0.004);
}

/// The frame-grid target of a pool step (`dir` frames) and the asset fps.
/// Pure: snaps to the grid so repeated steps don't drift; clamps to
/// `[0, last frame]`.
pub(crate) fn pool_step_target(app: &App, dir: i64) -> (f64, f64) {
    let sel = pool_sel(app).unwrap_or(0);
    let fps = app
        .project
        .media
        .get(sel)
        .and_then(|a| a.fps())
        .map(|f| secs(&f))
        .unwrap_or(30.0);
    let dur = app
        .project
        .media
        .get(sel)
        .and_then(|a| a.duration())
        .map(|d| secs(&d))
        .unwrap_or(f64::INFINITY);
    let last = pool_last_frame(app, dur);
    let cur = (pool_pos(app) * fps).round() as i64;
    let idx = (cur + dir).clamp(0, (last * fps).round() as i64);
    (idx as f64 / fps, fps)
}

/// Insert a clip from the selected pool asset at the playhead (ripple).
pub(crate) fn insert_from_pool(app: &mut App, player: &dyn Mpv) {
    let Some(asset) = app.project.media.get(pool_sel(app).unwrap_or(0)) else {
        app.set_message("no asset selected");
        return;
    };
    let dur = asset.duration();
    // Use a whole-frame duration: an asset like blue.mp4 (6.006 s) trimmed
    // to its exact duration would leave a sub-frame stump if a later edit
    // (a split, a paste) lands near the end — floor to the frame grid so
    // inserted clips are clean.
    let whole = match (dur, app.project.fps.as_ref()) {
        (Some(d), Some(fps)) => {
            let frames = (secs(&d) * secs(fps)).floor().max(1.0);
            Seconds::new(frames as i64, 1) / *fps
        }
        (Some(d), None) => d,
        (None, _) => {
            app.set_message("asset has no known duration");
            return;
        }
    };
    let snapshot = TimelineSegment::Clip(Clip {
        id: 0,
        asset: asset.id,
        asset_in: Seconds::from_integer(0),
        asset_out: whole,
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let inserted = run_action(
        app,
        player,
        &Action::Paste {
            track: app.active(),
            at: app.playhead,
            ripple: true,
            segment: Some(snapshot),
        },
    );
    if inserted {
        // Inserting is the pool's job: leave it so the monitor returns to the
        // timeline, keeping the frame the insert landed on.
        let landed = app.playhead;
        close_pool_at(app, player, landed);
    }
}

/// Remove the selected asset from the pool, undoably (the guard refuses if
/// it's in use), then refresh the pool preview.
pub(crate) fn remove_from_pool(app: &mut App, player: &dyn Mpv) {
    let Some(asset) = app.project.media.get(pool_sel(app).unwrap_or(0)) else {
        app.set_message("no asset selected");
        return;
    };
    run_action(app, player, &Action::AssetRm { id: asset.id });
}

/// Switch the active track from the pool: retarget where `enter` inserts and
/// announce it, but leave the monitor on the asset preview.
pub(crate) fn pool_set_track(app: &mut App, idx: usize) {
    if let Some(label) = select_active_track(app, idx) {
        app.set_message(&format!("insert track {label}"));
    }
}

/// While the pool is open the timeline panel and the monitor both show the
/// selected asset — playing, paused or ended. The project returns (at its
/// insert cursor) only when the pool is closed.
pub(crate) fn pool_strip_active(app: &App) -> bool {
    is_pool(app)
}
