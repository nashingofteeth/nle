use super::super::croc::CrocAct;
use super::super::keys::fx::form_for_effect;
use super::super::keys::{completes_gg, handle_key};
use super::*;
use nle_core::Armed;
use nle_core::timeline::Effect;

// Ctrl-C quits from whichever panel owns the keys — raw mode means no
// SIGINT, and the panels added after normal mode each dropped the guard.
#[test]
fn ctrl_c_quits_from_every_panel() {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let modes: Vec<(&str, App)> = vec![
        ("normal", App::new(asset_project(), temp_project_path("t"))),
        ("command", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            a.mode = Mode::Command("import ".into());
            a
        }),
        ("extend", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            a.mode = Mode::Extend(ExtendState {
                track: 0,
                armed: Armed::Clip { id: 1, head: false },
                edge_pos: 3.0,
                ripple: false,
                lo: 0.0,
                hi: Some(9.0),
                mat: None,
                preview_off: -1.0,
            });
            a
        }),
        ("pool", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            open_pool_mode(&mut a, 0);
            a
        }),
        ("transition", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            a.mode = Mode::Transition(TransitionForm {
                id: 1,
                start: 2.0,
                kind: TransitionKind::Dissolve,
                frames: String::new(),
                current: 30,
            });
            a
        }),
        ("fx list", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            a.mode = Mode::Fx(FxState {
                clip: 1,
                sel: 0,
                form: None,
            });
            a
        }),
        ("fx form", {
            let mut a = App::new(asset_project(), temp_project_path("t"));
            let effect = Effect::Crop {
                left: 0.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            };
            a.mode = Mode::Fx(FxState {
                clip: 1,
                sel: 0,
                form: Some(form_for_effect(&effect, 0)),
            });
            a
        }),
    ];
    for (name, mut app) in modes {
        let fake = FakeMpv::new();
        handle_key(&mut app, &fake, ctrl_c);
        assert!(app.quit, "[{name}] ctrl-c must quit");
    }
}

// A pasted count must not overflow the multiplier (debug panic) or step the
// playhead once per frame (a hang). It saturates and the move is one clamp.
#[test]
fn a_huge_count_saturates_and_lands_on_the_last_frame() {
    let mut app = App::new(asset_project(), temp_project_path("huge-count"));
    app.loaded = true;
    let fake = FakeMpv::new();
    for c in "999999999999999999999999".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    assert_eq!(app.count, Some(u32::MAX));
    handle_key(&mut app, &fake, key('f'));
    assert_eq!(app.count, None);
    assert_eq!(
        app.playhead,
        app.last_frame(),
        "huge count seeks to the end"
    );
}

// The same digit rule is shared by the pool and extend modes.
#[test]
fn a_huge_count_in_the_pool_does_not_panic() {
    let mut app = App::new(asset_project(), temp_project_path("huge-count-pool"));
    app.loaded = true;
    open_pool_mode(&mut app, 0);
    let fake = FakeMpv::new();
    for c in "999999999999999999999999".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, key('l'));
    assert_eq!(app.count, None);
}

#[test]
fn play_clears_to_the_ambient_swim_and_pause_sleeps() {
    let mut app = App::new(asset_project(), temp_project_path("transport-croc"));
    app.loaded = true;
    let fake = FakeMpv::new();

    app.paused = true;
    handle_key(&mut app, &fake, key(' '));
    assert_eq!(app.croc.act(), CrocAct::Idle, "play returns to ambient art");

    app.paused = false;
    handle_key(&mut app, &fake, key(' '));
    assert_eq!(app.croc.act(), CrocAct::Sleep, "pause only closes the eyes");
}

// The pool's `gg` needs both keys: a lone `g` only arms the sequence.
#[test]
fn completes_gg_needs_both_keys() {
    assert!(completes_gg(Pending::G, KeyCode::Char('g')));
    assert!(!completes_gg(Pending::None, KeyCode::Char('g')));
    assert!(!completes_gg(Pending::G, KeyCode::Char('G')));
}

#[test]
fn speed_key_opens_the_rate_prompt() {
    let mut app = App::new(asset_project(), temp_project_path("speed-key"));
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('r'));
    assert!(matches!(app.mode, Mode::Command(ref s) if s == "speed "));
}

// The pair was flipped: `r` speeds, `R` reloads.
#[test]
fn reload_is_uppercase_r() {
    let mut app = App::new(asset_project(), temp_project_path("reload-key"));
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('R'));
    assert!(app.reload_requested, "R asks the loop for a reload");
    assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
    handle_key(&mut app, &fake, key('r'));
    assert!(matches!(app.mode, Mode::Command(ref s) if s == "speed "));
}
