use super::super::*;
use super::*;
use nle_core::time::Seconds;

// `x` adds a cross-dissolve at the boundary under the playhead.
#[test]
fn x_key_adds_a_transition() {
    let mut app = App::new(asset_project(), temp_project_path("x-key"));
    app.loaded = true;
    app.playhead = 3.0; // asset_project's clip|gap cut
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('X'));
    let track = app.project.timeline.video_track().unwrap();
    assert!(
        track
            .segments
            .iter()
            .any(|s| matches!(s, TimelineSegment::Transition(_))),
        "a transition was added: {:?}",
        track.segments
    );
}

// Inside a blend `x` opens the transition length form, not the clip's
// effects panel; entering frames retimes in one batch.
#[test]
fn x_on_a_transition_opens_the_length_form() {
    let mut app = App::new(asset_project(), temp_project_path("trans-form"));
    app.loaded = true;
    app.playhead = 3.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('X')); // a 1 s dissolve at the clip|gap cut
    assert_eq!(app.playhead, 2.5, "inside the new blend");

    handle_key(&mut app, &fake, key('x'));
    let Mode::Transition(form) = &app.mode else {
        panic!("expected the transition form, got {:?}", app.mode)
    };
    assert_eq!((form.current, form.start), (30, 2.0));
    assert!(matches!(form.kind, TransitionKind::Dissolve));

    for c in "15".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
    let track = app.project.timeline.video_track().unwrap();
    let blend = track
        .segments
        .iter()
        .find_map(|s| match s {
            TimelineSegment::Transition(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(blend.dur, Seconds::new(1, 2));
    assert!(app.message.contains("retime transition"), "{}", app.message);
}

// `X` on a blend opens the same form; a count pre-fills the frames.
#[test]
fn x_and_a_count_edit_the_transition_frames() {
    let mut app = App::new(asset_project(), temp_project_path("trans-x"));
    app.loaded = true;
    app.playhead = 3.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('X'));

    handle_key(&mut app, &fake, key('1'));
    handle_key(&mut app, &fake, key('5'));
    handle_key(&mut app, &fake, key('X'));
    let Mode::Transition(form) = &app.mode else {
        panic!("expected the transition form, got {:?}", app.mode)
    };
    assert_eq!(form.frames, "15", "the count pre-fills the field");
    assert_eq!(form.current, 30);
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    // Without a count the input starts blank and shows the current value.
    handle_key(&mut app, &fake, key('X'));
    let Mode::Transition(form) = &app.mode else {
        panic!("expected the transition form, got {:?}", app.mode)
    };
    assert!(form.frames.is_empty());
    assert_eq!(form.current, 15);
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
}

// A blank form keeps the length; a refusal keeps the form open.
#[test]
fn a_transition_form_keeps_the_length_on_blank_or_refusal() {
    let mut app = App::new(asset_project(), temp_project_path("trans-blank"));
    app.loaded = true;
    app.playhead = 3.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('X'));
    handle_key(&mut app, &fake, key('x'));

    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.message.contains("unchanged"), "{}", app.message);

    // A count larger than the clip's handle is refused in place.
    handle_key(&mut app, &fake, key('X'));
    for c in "10000".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(
        matches!(app.mode, Mode::Transition(_)),
        "the form stays open: {:?}",
        app.mode
    );
    assert!(app.message.contains("no more tail"), "{}", app.message);
}

// `R` on a blend pre-fills the transition rate command; 50% doubles it.
#[test]
fn r_on_a_transition_prefills_the_rate_command() {
    let mut app = App::new(asset_project(), temp_project_path("trans-r"));
    app.loaded = true;
    app.playhead = 3.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('X'));

    handle_key(&mut app, &fake, key('r'));
    assert!(
        matches!(app.mode, Mode::Command(ref s) if s == "transition --percent "),
        "{:?}",
        app.mode
    );
    for c in "50".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    let track = app.project.timeline.video_track().unwrap();
    let blend = track
        .segments
        .iter()
        .find_map(|s| match s {
            TimelineSegment::Transition(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(blend.dur, Seconds::from_integer(2), "50% is twice as long");
}

// `X` lands in the middle of the new blend — the cut the transition was
// added to — not on either edge.
#[test]
fn x_lands_on_the_transition_midpoint() {
    let mut app = App::new(asset_project(), temp_project_path("x-cut"));
    app.loaded = true;
    app.playhead = 3.0; // asset_project's clip|gap cut
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('X'));
    assert_eq!(app.playhead, 2.5, "the middle of the 1 s blend");
    let track = app.project.timeline.video_track().unwrap();
    let blend = track
        .segments
        .iter()
        .position(|s| matches!(s, TimelineSegment::Transition(_)))
        .expect("a transition was added");
    assert_eq!(
        track.start_of(blend) + track.segments[blend].duration(),
        Seconds::from_integer(3),
        "the blend ends on the cut"
    );
}

// `xx` adds the transition and leaves the effects panel; a refused add
// keeps it open so it can be retried.
#[test]
fn xx_adds_a_transition_and_exits_effects_mode() {
    let mut app = App::new(asset_project(), temp_project_path("xx-exit"));
    app.loaded = true;
    app.playhead = 3.0; // clip|gap cut
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('x'));
    assert!(is_fx(&app), "the first x opens the panel");
    handle_key(&mut app, &fake, key('x'));
    assert!(
        matches!(app.mode, Mode::Normal),
        "the second x exits the panel: {:?}",
        app.mode
    );
    assert!(app.message.starts_with("transition"), "{}", app.message);
    assert!(
        app.project
            .timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .any(|s| matches!(s, TimelineSegment::Transition(_))),
        "the transition landed"
    );
    assert_eq!(app.playhead, 2.5, "the new blend's midpoint");

    // Not on a cut: the add fails, so the panel stays open.
    let mut app = App::new(asset_project(), temp_project_path("xx-fail"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('x'));
    handle_key(&mut app, &fake, key('x'));
    assert!(is_fx(&app), "a refused transition keeps the panel open");
}
