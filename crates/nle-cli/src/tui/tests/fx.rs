use super::super::keys::fx::{form_for, form_for_effect};
use super::super::keys::handle_key;
use super::super::panels::fx::render_fx;
use super::*;
use nle_core::timeline::Effect;

// `j`/`k` move the row cursor; `d` drops that row (one undo step) and the
// cursor clamps onto the surviving list.
#[test]
fn effects_panel_drops_the_selected_row_and_undo_restores_it() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("fx-drop"));
    app.loaded = true;
    app.playhead = 1.0;
    let scale = nle_core::timeline::Effect::Scale { w: 2.0, h: 2.0 };
    let crop = nle_core::timeline::Effect::Crop {
        left: 10.0,
        right: 10.0,
        top: 0.0,
        bottom: 0.0,
    };
    set_effects(&mut app, 1, vec![scale.clone(), crop.clone()]);

    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('j'));
    assert_eq!(fx_state(&app).unwrap().sel, 1);
    handle_key(&mut app, &fake, key('d'));
    assert_eq!(fx_clip(&app).unwrap().effects, vec![scale.clone()]);
    assert_eq!(fx_state(&app).unwrap().sel, 0, "the cursor clamps");
    assert!(app.project.can_undo());

    handle_key(&mut app, &fake, key('u'));
    assert_eq!(fx_clip(&app).unwrap().effects, vec![scale, crop]);
    assert!(is_fx(&app), "undo keeps the panel open");
}

// `D` drops every effect in one batch; one undo brings them all back.
#[test]
fn effects_panel_clear_is_one_undo_step() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("fx-clear"));
    app.loaded = true;
    app.playhead = 1.0;
    let eff = nle_core::timeline::Effect::Eq {
        brightness: 0.5,
        contrast: 1.0,
        saturation: 1.0,
    };
    set_effects(&mut app, 1, vec![eff.clone(), eff.clone()]);
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('D'));
    assert!(fx_clip(&app).unwrap().effects.is_empty());
    handle_key(&mut app, &fake, key('u'));
    assert_eq!(fx_clip(&app).unwrap().effects, vec![eff.clone(), eff]);
}

// Digits open the row with that number directly; out-of-range says so.
#[test]
fn effects_panel_digits_open_a_row() {
    let fake = FakeMpv::new();
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-digits"));
    app.loaded = true;
    app.playhead = 1.0;
    set_effects(
        &mut app,
        1,
        vec![
            Effect::Crop {
                left: 10.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
            Effect::Eq {
                brightness: 0.0,
                contrast: 1.0,
                saturation: 1.0,
            },
            Effect::Scale { w: 50.0, h: 50.0 },
        ],
    );
    handle_key(&mut app, &fake, key('x'));
    let sel = |app: &App| fx_state(app).unwrap().sel;

    // An out-of-range digit leaves the list alone.
    handle_key(&mut app, &fake, key('9'));
    assert_eq!(sel(&app), 0);
    assert!(app.message.contains("no effect #9"), "{}", app.message);
    assert!(fx_state(&app).unwrap().form.is_none());

    // A valid digit opens that effect without an extra enter.
    handle_key(&mut app, &fake, key('2'));
    let form = fx_state(&app).unwrap().form.as_ref().unwrap();
    assert_eq!(form.editing, Some(1));
    assert_eq!(form.kind, "eq");
    assert_eq!(sel(&app), 1);

    // Esc returns to the list; another digit opens a different row.
    handle_key(&mut app, &fake, esc);
    handle_key(&mut app, &fake, key('3'));
    let form = fx_state(&app).unwrap().form.as_ref().unwrap();
    assert_eq!(form.editing, Some(2));
    assert_eq!(form.kind, "scale");
}

// Adding from the panel asks for the values in labelled fields, refuses a
// missing required one, and exits the panel once the effect lands.
#[test]
fn effects_panel_form_takes_values_and_exits() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-add"));
    app.loaded = true;
    app.playhead = 1.0;
    set_effects(&mut app, 1, vec![Effect::Scale { w: 2.0, h: 2.0 }]);

    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('c'));
    assert!(
        matches!(
            fx_state(&app).and_then(|s| s.form.as_ref()).map(|f| f.kind),
            Some("crop")
        ),
        "{:?}",
        app.mode
    );

    // An inset over 100 is refused at apply time and keeps the form open.
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    for c in "101".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, enter); // applies from the first field
    assert!(
        app.message.contains("left must be 0–100"),
        "{}",
        app.message
    );
    assert!(fx_state(&app).unwrap().form.is_some(), "still editing");

    // Cancel, reopen, and inset 10% from the left and right. Only tab
    // moves between fields; enter always commits.
    handle_key(&mut app, &fake, esc);
    handle_key(&mut app, &fake, key('c'));
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    for c in "10".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, tab);
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    for c in "10".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, enter); // apply from the right field

    assert!(
        matches!(app.mode, Mode::Normal),
        "applying exits the panel: {:?}",
        app.mode
    );
    assert!(
        app.message.starts_with("effect on clip #1"),
        "the action's notice stays: {}",
        app.message
    );
    assert_eq!(
        clip_of(&app.project, 1).effects.last(),
        Some(&Effect::Crop {
            left: 10.0,
            right: 10.0,
            top: 0.0,
            bottom: 0.0
        })
    );
}

// `e` opens the `eq` form with the neutral defaults; enter applies them
// from the first field (it never advances the focus).
#[test]
fn effects_panel_eq_form_defaults_are_neutral() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-eq"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('e'));
    handle_key(&mut app, &fake, enter);
    assert_eq!(
        clip_of(&app.project, 1).effects.last(),
        Some(&Effect::Eq {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0
        })
    );
    assert!(matches!(app.mode, Mode::Normal));
}

// Numeric field labels double as jump keys; tab/arrows move by one.
#[test]
fn effect_form_letters_jump_and_tab_moves() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
    let mut app = App::new(asset_project(), temp_project_path("fx-jump"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('c'));

    handle_key(&mut app, &fake, key('b'));
    let focus = |app: &App| fx_state(app).unwrap().form.as_ref().unwrap().focus;
    assert_eq!(focus(&app), 3, "b jumps to bottom");
    for c in "12".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, tab);
    assert_eq!(focus(&app), 3, "tab stops at the last field");
    handle_key(&mut app, &fake, back);
    assert_eq!(focus(&app), 2, "backtab moves back");

    handle_key(&mut app, &fake, enter);
    assert_eq!(
        clip_of(&app.project, 1).effects.last(),
        Some(&Effect::Crop {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 12.0
        })
    );
}

// The offset form takes a shift in percent of the canvas; x/y jump keys.
#[test]
fn offset_form_takes_x_and_y() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-offset"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('o'));
    for c in "10".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, key('y')); // jump to the y field
    for c in "-20".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, enter);
    assert_eq!(
        clip_of(&app.project, 1).effects.last(),
        Some(&Effect::Offset { x: 10.0, y: -20.0 })
    );
    assert!(matches!(app.mode, Mode::Normal));
}

// `a` locks the scale form's axes: typing one mirrors the other.
#[test]
fn scale_form_lock_keeps_the_aspect() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-lock"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('s'));
    assert!(
        fx_state(&app).unwrap().form.as_ref().unwrap().lock,
        "scale starts locked"
    );
    handle_key(&mut app, &fake, key('w'));
    for c in "50".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    let form = fx_state(&app).unwrap().form.as_ref().unwrap();
    assert!(form.lock);
    assert_eq!(form.fields[1].value, "50", "height mirrors width");
    handle_key(&mut app, &fake, enter);
    assert_eq!(
        clip_of(&app.project, 1).effects.last(),
        Some(&Effect::Scale { w: 50.0, h: 50.0 })
    );
}

// Tab reaches the lock row like any other field.
#[test]
fn the_scale_lock_is_in_the_tab_order() {
    let fake = FakeMpv::new();
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-lock-tab"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('s'));
    let focus = |app: &App| fx_state(app).unwrap().form.as_ref().unwrap().focus;
    assert_eq!(focus(&app), 0);
    handle_key(&mut app, &fake, tab);
    assert_eq!(focus(&app), 1);
    handle_key(&mut app, &fake, tab);
    assert_eq!(focus(&app), 2, "tab reaches the lock");
    assert!(
        fx_state(&app).unwrap().form.as_ref().unwrap().lock,
        "the lock starts on"
    );
    handle_key(&mut app, &fake, space);
    assert!(!fx_state(&app).unwrap().form.as_ref().unwrap().lock);
    handle_key(&mut app, &fake, space);
    assert!(fx_state(&app).unwrap().form.as_ref().unwrap().lock);
}

// The lock is sticky: reopening the applied scale keeps it on (and
// turning it off sticks too).
#[test]
fn the_scale_lock_persists_across_forms() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-lock-sticky"));
    app.loaded = true;
    app.playhead = 1.0;

    // Apply scale 50×50 with the default (on) lock.
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('s'));
    assert!(fx_state(&app).unwrap().form.as_ref().unwrap().lock);
    handle_key(&mut app, &fake, key('w'));
    for c in "50".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, enter);
    assert!(app.fx_scale_lock);

    // Reopening the applied effect keeps it locked.
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, enter);
    let form = fx_state(&app).unwrap().form.as_ref().unwrap();
    assert!(form.lock, "the edit form reopens locked");
    assert_eq!(form.editing, Some(0));

    // Turning it off sticks for the next edit too.
    handle_key(&mut app, &fake, key('a'));
    handle_key(&mut app, &fake, space);
    assert!(!app.fx_scale_lock);
    handle_key(&mut app, &fake, esc);
    handle_key(&mut app, &fake, enter);
    assert!(!fx_state(&app).unwrap().form.as_ref().unwrap().lock);
}

// Editing a 2:1 scale with the lock keeps that ratio, not 1:1.
#[test]
fn scale_edit_lock_uses_the_applied_ratio() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-lock-edit"));
    app.loaded = true;
    app.playhead = 1.0;
    set_effects(&mut app, 1, vec![Effect::Scale { w: 50.0, h: 25.0 }]);
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, enter); // edit the scale
    assert!(
        fx_state(&app).unwrap().form.as_ref().unwrap().lock,
        "the edit form starts locked"
    );
    handle_key(&mut app, &fake, key('w'));
    for c in "40".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    let form = fx_state(&app).unwrap().form.as_ref().unwrap();
    assert_eq!(form.aspect, 2.0);
    assert_eq!(form.fields[1].value, "20", "40 / 2:1 = 20");
    handle_key(&mut app, &fake, enter);
    assert_eq!(
        clip_of(&app.project, 1).effects,
        vec![Effect::Scale { w: 40.0, h: 20.0 }]
    );
}

// Without the lock the axes stay independent.
#[test]
fn scale_form_without_lock_keeps_axes_independent() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-unlock"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('s'));
    handle_key(&mut app, &fake, key('a'));
    handle_key(&mut app, &fake, space); // unlock the default
    handle_key(&mut app, &fake, key('w'));
    for c in "50".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    assert!(!fx_state(&app).unwrap().form.as_ref().unwrap().lock);
    handle_key(&mut app, &fake, enter);
    assert_eq!(
        clip_of(&app.project, 1).effects.last(),
        Some(&Effect::Scale { w: 50.0, h: 100.0 })
    );
}

// Enter on a listed effect opens it prefilled; blank fields keep the old
// value, and the whole edit is one undo step.
#[test]
fn selecting_an_effect_opens_it_for_editing() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-edit"));
    app.loaded = true;
    app.playhead = 1.0;
    let old = Effect::Crop {
        left: 30.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    };
    set_effects(&mut app, 1, vec![old.clone()]);
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, enter);

    let form = fx_state(&app).unwrap().form.as_ref().unwrap();
    assert_eq!(form.editing, Some(0));
    assert_eq!(form.kind, "crop");
    assert_eq!(form.fields[0].value, "", "the input starts blank");
    assert_eq!(form.fields[0].previous.as_deref(), Some("30"));

    // Change only the right inset; left keeps its previous 30.
    handle_key(&mut app, &fake, key('r'));
    for c in "10".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, enter);
    assert_eq!(
        clip_of(&app.project, 1).effects,
        vec![Effect::Crop {
            left: 30.0,
            right: 10.0,
            top: 0.0,
            bottom: 0.0
        }]
    );
    assert!(app.message.starts_with("edit crop"), "{}", app.message);

    handle_key(&mut app, &fake, key('u'));
    assert_eq!(clip_of(&app.project, 1).effects, vec![old]);
}

// Applying an edit form without typing anything is not a history entry.
#[test]
fn a_blank_edit_form_keeps_the_effect() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-noop"));
    app.loaded = true;
    app.playhead = 1.0;
    let old = Effect::Eq {
        brightness: 0.2,
        contrast: 1.0,
        saturation: 1.0,
    };
    set_effects(&mut app, 1, vec![old.clone()]);
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, enter);
    handle_key(&mut app, &fake, enter);

    assert_eq!(clip_of(&app.project, 1).effects, vec![old]);
    assert!(app.message.contains("unchanged"), "{}", app.message);
    assert!(!app.project.can_undo(), "a no-op is not an undo step");
    assert!(matches!(app.mode, Mode::Normal));
}

// LUTs are CLI-only for now: `l` adds nothing and a listed LUT can't be
// opened in the panel's form (removal still works).
#[test]
fn lut_is_cli_only_in_the_panel_for_now() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("fx-lut"));
    app.loaded = true;
    app.playhead = 1.0;
    set_effects(
        &mut app,
        1,
        vec![Effect::Lut {
            path: "/luts/a.cube".into(),
        }],
    );
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('l'));
    assert!(
        fx_state(&app).unwrap().form.is_none(),
        "l no longer opens a LUT form"
    );
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(
        fx_state(&app).unwrap().form.is_none(),
        "Enter leaves the LUT row alone"
    );
    assert!(app.message.contains("CLI-only"), "{}", app.message);
}

// The edit form shows the applied value beside the blank input.
#[test]
fn the_edit_form_shows_the_old_value_beside_a_blank_input() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = App::new(asset_project(), temp_project_path("t"));
    let effect = Effect::Crop {
        left: 30.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    };
    app.mode = Mode::Fx(FxState {
        clip: 1,
        sel: 0,
        form: Some(form_for_effect(&effect, 0)),
    });
    let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
    term.draw(|f| render_fx(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..12u16)
        .map(|y| (0..80u16).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("edit crop → clip #1"), "{text}");
    assert!(text.contains("was 30"), "{text}");
    assert!(text.contains("enter applies"), "{text}");
}

// Re-opening a kind that is already applied edits it: no second row.
#[test]
fn adding_an_existing_kind_opens_it_for_editing() {
    let fake = FakeMpv::new();
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut app = App::new(asset_project(), temp_project_path("fx-nostack"));
    app.loaded = true;
    app.playhead = 1.0;
    set_effects(
        &mut app,
        1,
        vec![Effect::Crop {
            left: 30.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        }],
    );
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('c'));

    let form = fx_state(&app).unwrap().form.as_ref().unwrap();
    assert_eq!(form.editing, Some(0), "c edits the applied crop");
    assert_eq!(form.fields[0].previous.as_deref(), Some("30"));

    handle_key(&mut app, &fake, key('r'));
    for c in "10".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(&mut app, &fake, enter);
    assert_eq!(
        clip_of(&app.project, 1).effects,
        vec![Effect::Crop {
            left: 30.0,
            right: 10.0,
            top: 0.0,
            bottom: 0.0
        }],
        "still one crop row"
    );
}

// A legacy duplicate (the demo's `crop, crop`) is not stacked further:
// the add key edits the row under the cursor.
#[test]
fn an_existing_duplicate_edits_the_selected_row() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("fx-dup"));
    app.loaded = true;
    app.playhead = 1.0;
    set_effects(
        &mut app,
        1,
        vec![
            Effect::Crop {
                left: 10.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
            Effect::Crop {
                left: 20.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
        ],
    );
    app.mode = Mode::Fx(FxState {
        clip: 1,
        sel: 1,
        form: None,
    });
    handle_key(&mut app, &fake, key('c'));
    let form = fx_state(&app).unwrap().form.as_ref().unwrap();
    assert_eq!(form.editing, Some(1), "the selected crop is edited");
    assert_eq!(form.fields[0].previous.as_deref(), Some("20"));
}

// The panel renders the kind + parameters, and says how to add when the
// clip has none.
#[test]
fn the_effects_panel_shows_rows_or_an_empty_hint() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = App::new(asset_project(), temp_project_path("t"));
    set_effects(
        &mut app,
        1,
        vec![
            nle_core::timeline::Effect::Crop {
                left: 10.0,
                right: 10.0,
                top: 10.0,
                bottom: 10.0,
            },
            nle_core::timeline::Effect::Eq {
                brightness: 0.1,
                contrast: 1.0,
                saturation: 1.0,
            },
        ],
    );
    app.mode = Mode::Fx(FxState {
        clip: 1,
        sel: 1,
        form: None,
    });
    let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
    term.draw(|f| render_fx(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..8u16)
        .map(|y| (0..60u16).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("effects → clip #1"), "{text}");
    assert!(text.contains("l10 r10 t10 b10"), "{text}");
    assert!(text.contains("b 0.1 · c 1 · s 1"), "{text}");

    app.mode = Mode::Fx(FxState {
        clip: 2,
        sel: 0,
        form: None,
    });
    let mut term = Terminal::new(TestBackend::new(60, 6)).unwrap();
    term.draw(|f| render_fx(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..6u16)
        .map(|y| (0..60u16).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("no effects"), "{text}");
}

// Each add form explains what its fields expect: units, ranges, and the
// neutral/default values.
#[test]
fn effect_forms_hint_their_expected_input() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let render = |kind: &'static str| -> String {
        let mut app = App::new(asset_project(), temp_project_path("t"));
        app.mode = Mode::Fx(FxState {
            clip: 1,
            sel: 0,
            form: Some(form_for(kind)),
        });
        let mut term = Terminal::new(TestBackend::new(100, 16)).unwrap();
        term.draw(|f| render_fx(f, f.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..16u16)
            .map(|y| {
                (0..100u16)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    for (kind, hint) in [
        ("crop", "opposite edge"),
        ("scale", "100 keeps the size"),
        ("offset", "0 keeps the position"),
        ("eq", "contrast 1"),
    ] {
        let text = render(kind);
        assert!(text.contains(hint), "{kind}: {hint:?}\n{text}");
        // Every input starts blank; numeric forms show their neutral value
        // beside it, even when adding (not only when editing).
        assert!(text.contains("[▏]"), "{kind}: the input is blank\n{text}");
        assert!(text.contains("was "), "{kind}: no previous value\n{text}");
        if kind == "scale" {
            assert!(text.contains("aspect lock"), "{kind}: no lock row\n{text}");
            assert!(text.contains("[x]"), "{kind}: starts unlocked\n{text}");
            assert!(
                text.contains("space toggles"),
                "{kind}: no lock hint\n{text}"
            );
        }
    }
}
