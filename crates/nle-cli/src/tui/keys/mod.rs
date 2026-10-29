//! Key dispatch: the Normal-mode table and the mode switch that routes a
//! key to pool, effects, extend or the command line.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use nle_core::action::{Action, GapLen, Target};
use nle_core::track_label;

use crate::mpv::Mpv;

use self::command::command_key;
use self::extend::{Side, enter_extend, enter_extend_side, extend_key};
use self::fx::{
    add_transition, fx_key, open_fx, open_transition_form, transition_form_key, transition_target,
};
use self::pool::{open_pool, pool_key};
use super::actions::{edit_delete, edit_yank, run_action, seek_clip_end};
use super::croc::CrocAct;
use super::motion::{
    cut_points, half_frame, next_clip_start, next_cut, next_gap_start, prev_clip_start, prev_cut,
    prev_gap_start,
};
use super::{App, Mode, Pending, is_fx, is_pool};

pub(crate) mod command;
pub(crate) mod extend;
pub(crate) mod fx;
pub(crate) mod pool;

pub fn handle_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    // Ctrl-C quits even in raw mode (no SIGINT is generated). One guard for
    // every mode: the panels that grew later (command, pool, transition, fx)
    // each missed their own copy and the key went dead there.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        return;
    }
    if matches!(app.mode, Mode::Command(_)) {
        let buf = match std::mem::replace(&mut app.mode, Mode::Normal) {
            Mode::Command(buf) => buf,
            _ => unreachable!("checked above"),
        };
        command_key(app, player, buf, key);
    } else if matches!(app.mode, Mode::Extend(_)) {
        extend_key(app, player, key);
    } else if is_pool(app) {
        pool_key(app, player, key);
    } else if matches!(app.mode, Mode::Transition(_)) {
        transition_form_key(app, player, key);
    } else if is_fx(app) {
        fx_key(app, player, key);
    } else {
        normal_key(app, player, key);
    }
}

/// Whether `code` completes the pending `gg` motion. Pure so the pool and
/// normal modes share one rule — and it is testable without a live mpv.
pub(crate) fn completes_gg(pending: Pending, code: KeyCode) -> bool {
    matches!((pending, code), (Pending::G, KeyCode::Char('g')))
}

/// Undo the last edit, restoring the playhead to where it was made.
pub(crate) fn do_undo(app: &mut App, player: &dyn Mpv) {
    run_action(app, player, &Action::Undo);
}

/// Redo the last undone edit.
pub(crate) fn do_redo(app: &mut App, player: &dyn Mpv) {
    run_action(app, player, &Action::Redo);
}

/// Add a digit to the pending vim count. Saturating is deliberate: an
/// overflow panicked in debug and wrapped to a different count in release
/// (`999999999999f` used to crash the TUI).
pub(crate) fn count_digit(app: &mut App, d: u32) {
    app.count = Some(app.count.unwrap_or(0).saturating_mul(10).saturating_add(d));
}

pub(crate) fn normal_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    let code = key.code;

    // Vim counts: digits build a multiplier; bare `0` jumps to the start.
    if let KeyCode::Char(c @ '0'..='9') = code {
        if c == '0' && app.count.is_none() {
            app.seek_to(player, 0.0);
            return;
        }
        let d = c.to_digit(10).unwrap_or(0);
        count_digit(app, d);
        return;
    }
    let had_count = app.count.is_some();
    let mult = app.count.take().unwrap_or(1);
    let eps = half_frame(&app.project);

    // Second key of a two-key sequence.
    match (&app.pending, code) {
        (Pending::G, KeyCode::Char('g')) => {
            app.pending = Pending::None;
            app.seek_to(player, 0.0);
            return;
        }
        (Pending::D, KeyCode::Char('d')) => {
            app.pending = Pending::None;
            edit_delete(app, player);
            return;
        }
        (Pending::Y, KeyCode::Char('y')) => {
            app.pending = Pending::None;
            edit_yank(app, player);
            return;
        }
        _ => {}
    }

    match code {
        KeyCode::Char(' ') => {
            if app.guard_loaded() {
                if app.eof {
                    // At the end: play restarts from the beginning.
                    player.seek_absolute(0.0);
                    player.set_pause(false);
                    app.croc.play(CrocAct::Swim);
                } else {
                    let was_paused = app.paused;
                    player.toggle_pause();
                    app.croc.play(if was_paused {
                        CrocAct::Swim
                    } else {
                        CrocAct::Yawn
                    });
                }
            }
        }
        KeyCode::Char('h') => app.seek_rel(player, -(mult as f64)),
        KeyCode::Char('l') => app.seek_rel(player, mult as f64),
        KeyCode::Char('H') => app.seek_rel(player, -5.0 * mult as f64),
        KeyCode::Char('L') => app.seek_rel(player, 5.0 * mult as f64),
        KeyCode::Char('f') => {
            if app.guard_loaded() {
                app.step_playhead_by(1, mult);
                // Model-authoritative: move the monitor to the frame we
                // display, on the PROJECT grid. mpv's own `frame_step` walks
                // the source's grid instead, so a 24/59.94 clip would drift
                // the marker and the monitor apart (and jump the playhead on
                // the next play).
                app.seek_to(player, app.playhead);
            }
        }
        KeyCode::Char('F') => {
            if app.guard_loaded() {
                app.step_playhead_by(-1, mult);
                app.seek_to(player, app.playhead);
            }
        }
        KeyCode::Char('j') => {
            match next_cut(&cut_points(&app.project, app.active()), app.playhead, eps) {
                Some(t) => app.seek_to(player, t),
                None => app.set_message("no cut after the playhead"),
            }
        }
        KeyCode::Char('k') => {
            match prev_cut(&cut_points(&app.project, app.active()), app.playhead, eps) {
                Some(t) => app.seek_to(player, t),
                None => app.set_message("no cut before the playhead"),
            }
        }
        KeyCode::Char('w') => {
            match next_clip_start(&app.project, app.playhead, eps, app.active()) {
                Some(t) => app.seek_to(player, t),
                None => app.set_message("no clip after the playhead"),
            }
        }
        KeyCode::Char('b') => {
            match prev_clip_start(&app.project, app.playhead, eps, app.active()) {
                Some(t) => app.seek_to(player, t),
                None => app.set_message("no clip before the playhead"),
            }
        }
        KeyCode::Char('n') => match next_gap_start(&app.project, app.playhead, eps, app.active()) {
            Some(t) => app.seek_to(player, t),
            None => app.set_message("no gap after the playhead"),
        },
        KeyCode::Char('N') => match prev_gap_start(&app.project, app.playhead, eps, app.active()) {
            Some(t) => app.seek_to(player, t),
            None => app.set_message("no gap before the playhead"),
        },
        KeyCode::Char('g') => app.pending = Pending::G,
        KeyCode::Char('d') => app.pending = Pending::D,
        KeyCode::Char('y') => app.pending = Pending::Y,
        // ---- editing: uniform playhead-based ops (counts don't multiply) ----
        KeyCode::Char('i') => {
            run_action(
                app,
                player,
                &Action::TrimIn {
                    track: app.active(),
                    at: app.playhead,
                    ripple: false,
                },
            );
        }
        KeyCode::Char('I') => {
            run_action(
                app,
                player,
                &Action::TrimIn {
                    track: app.active(),
                    at: app.playhead,
                    ripple: true,
                },
            );
        }
        KeyCode::Char('o') => {
            run_action(
                app,
                player,
                &Action::TrimOut {
                    track: app.active(),
                    at: app.playhead,
                    ripple: false,
                },
            );
        }
        KeyCode::Char('O') => {
            run_action(
                app,
                player,
                &Action::TrimOut {
                    track: app.active(),
                    at: app.playhead,
                    ripple: true,
                },
            );
        }
        KeyCode::Char('s') => {
            run_action(
                app,
                player,
                &Action::Split {
                    track: app.active(),
                    at: app.playhead,
                },
            );
        }
        KeyCode::Char('_') => {
            run_action(
                app,
                player,
                &Action::InsertGap {
                    track: app.active(),
                    at: app.playhead,
                    len: GapLen::Frames(mult as i64),
                },
            );
        }
        KeyCode::Char('D') => {
            edit_yank(app, player); // D yanks too
            run_action(
                app,
                player,
                &Action::Delete {
                    track: app.active(),
                    target: Target::At(app.playhead),
                    ripple: true,
                },
            );
        }
        KeyCode::Char('p') => {
            run_action(
                app,
                player,
                &Action::Paste {
                    track: app.active(),
                    at: app.playhead,
                    ripple: false,
                    segment: None,
                },
            );
        }
        KeyCode::Char('P') => {
            run_action(
                app,
                player,
                &Action::Paste {
                    track: app.active(),
                    at: app.playhead,
                    ripple: true,
                    segment: None,
                },
            );
        }
        KeyCode::Char('u') => do_undo(app, player),
        KeyCode::Char('m') => open_pool(app, player),
        // `x` — the effects panel for the clip under the playhead, or the
        // length form when the playhead is inside a transition (a blend owns
        // no clip's effects).
        KeyCode::Char('x') => {
            if transition_target(app).is_some() {
                open_transition_form(app, None);
            } else {
                open_fx(app);
            }
        }
        // `X` — cross-dissolve at the boundary; a count is the duration in
        // frames (default 1 s). On an existing blend the same key opens its
        // length form, with the count pre-filled.
        KeyCode::Char('X') => {
            if transition_target(app).is_some() {
                open_transition_form(app, had_count.then_some(mult as i64));
            } else {
                add_transition(app, player, had_count.then_some(mult as i64));
            }
        }
        // `t`/`T` cycle the active track; `<N>t` jumps to track N (1-based).
        KeyCode::Char('t') => {
            let n = app.project.timeline.tracks.len();
            let idx = if had_count {
                (mult as usize).saturating_sub(1)
            } else {
                (app.active() + 1) % n.max(1)
            };
            set_active_track(app, idx);
        }
        KeyCode::Char('T') => {
            let n = app.project.timeline.tracks.len();
            let idx = if had_count {
                (mult as usize).saturating_sub(1)
            } else {
                (app.active() + n.max(1) - 1) % n.max(1)
            };
            set_active_track(app, idx);
        }
        KeyCode::Char('e') => enter_extend(app, player, false),
        KeyCode::Char('E') => enter_extend(app, player, true),
        KeyCode::Char('[') => enter_extend_side(app, player, Side::Right),
        KeyCode::Char(']') => enter_extend_side(app, player, Side::Left),
        KeyCode::Char('<') | KeyCode::Char(',') => seek_clip_end(app, player, true),
        KeyCode::Char('>') | KeyCode::Char('.') => seek_clip_end(app, player, false),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => do_redo(app, player),
        // ---- navigation/transport (unchanged) ----
        KeyCode::Char('G') | KeyCode::Char('$') => app.seek_to(player, app.total()),
        KeyCode::Char('R') => app.reload_requested = true,
        KeyCode::Char('q') => app.quit = true,
        // `r` opens the speed prompt for the clip under the playhead; the
        // command line is pre-filled so it reads as `:speed ` (or the
        // transition rate form when the playhead is inside a blend).
        KeyCode::Char('r') => {
            app.mode = Mode::Command(if transition_target(app).is_some() {
                "transition --percent ".to_string()
            } else {
                "speed ".to_string()
            });
        }
        KeyCode::Char(':') => app.mode = Mode::Command(String::new()),
        _ => {}
    }

    // Any key other than a pending-sequence starter (or count digits)
    // cancels a pending two-key sequence.
    if !matches!(
        code,
        KeyCode::Char('g') | KeyCode::Char('d') | KeyCode::Char('y') | KeyCode::Char('0'..='9')
    ) {
        app.pending = Pending::None;
    }
}

/// Make track `idx` active (clamped) and return its label. Leaves the monitor
/// alone — the pool keeps its asset preview; normal mode reloads on top.
pub(crate) fn select_active_track(app: &mut App, idx: usize) -> Option<String> {
    let n = app.project.timeline.tracks.len();
    if n == 0 {
        return None;
    }
    app.active_track = idx.min(n - 1);
    Some(track_label(&app.project, app.active_track))
}

/// Make track `idx` active and announce it. The monitor plays the composite
/// of all video tracks, so switching tracks does not reload it.
pub(crate) fn set_active_track(app: &mut App, idx: usize) {
    if let Some(label) = select_active_track(app, idx) {
        app.set_message(&format!("track {label}"));
    }
}
