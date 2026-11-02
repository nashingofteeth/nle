use super::super::actions::{reaction_for, run_action};
use super::super::croc::CrocAct;
use super::super::keys::handle_key;
use super::*;
use nle_core::Seconds;
use nle_core::Target;
use nle_core::action::{Action, GapLen, TransitionLen};
use nle_core::timeline::{Effect, TrackKind, TransitionKind};

// The status row carries the preview job's progress: finished/total, the
// percentage (including the render in flight), and "queued" when a newer
// edit is waiting.
#[test]
fn the_status_row_shows_preview_progress() {
    let mut app = App::new(asset_project(), temp_project_path("t"));
    let before = status_row(&app);
    assert!(!before.contains("proxies"), "{before}");

    // A build younger than the notice delay is not announced: a warm cache
    // finishes in milliseconds and must not flash.
    let (_tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    app.preview_progress = Some(PreviewProgress {
        done: 1,
        total: 3,
        frac: 0.5,
    });
    let young = status_row(&app);
    assert!(!young.contains("proxies"), "no flash: {young}");

    // Past the delay it shows up.
    if let Some(job) = app.preview.as_mut() {
        job.started = Instant::now() - PREVIEW_NOTICE_DELAY - Duration::from_millis(1);
    }
    let mid = status_row(&app);
    assert!(mid.contains("proxies 1/3 · 50%"), "{mid}");

    app.preview_dirty = true;
    let queued = status_row(&app);
    assert!(queued.contains("50% · queued"), "{queued}");

    // Before the plan counts its renders, it says it is building.
    app.preview_progress = Some(PreviewProgress::default());
    let planning = status_row(&app);
    assert!(planning.contains("building preview"), "{planning}");
}

// The status mark is a crocodile; deleting material opens its mouth for a
// moment, then it closes (time-based, so there is no extra mode/state).
#[test]
fn the_crocodile_chomps_when_material_is_removed() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("chomp-clip"));
    app.loaded = true;
    let closed = status_row(&app);
    assert!(closed.contains(r#"=";::;>--"#), "{closed}");

    // `dd` on clip #1 leaves a gap: the clip id vanishes, the count does
    // not — the stamp looks at both.
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('d'));
    handle_key(&mut app, &fake, key('d'));
    assert_eq!(app.croc.act(), CrocAct::Chomp, "a delete opens the mouth");
    app.croc.hold_first_frame();
    let open = status_row(&app);
    assert!(open.contains(r#">";::;>--"#), "{open}");

    // Deleting a gap has no id to lose, but the segment count falls.
    let mut app = App::new(asset_project(), temp_project_path("chomp-gap"));
    app.loaded = true;
    app.playhead = 4.0; // the gap
    handle_key(&mut app, &fake, key('d'));
    handle_key(&mut app, &fake, key('d'));
    assert_eq!(app.croc.act(), CrocAct::Chomp, "a gap delete chomps too");

    // After the window it is closed again.
    app.croc.set_elapsed(Duration::from_millis(500));
    let row = status_row(&app);
    assert!(row.contains(r#"=";::;>--"#), "{row}");
    assert!(!row.contains(r#">";::;>--"#), "{row}");
}

#[test]
fn the_crocodile_blinks_when_idle() {
    let mut app = App::new(asset_project(), temp_project_path("blink"));
    assert_eq!(app.croc.act(), CrocAct::Idle);
    app.croc.set_idle_elapsed(Duration::from_millis(6_850));
    let blinking = status_row(&app);
    assert!(blinking.contains(r#"=-;::;>--"#), "{blinking}");
    app.croc.set_idle_elapsed(Duration::ZERO);
    let resting = status_row(&app);
    assert!(resting.contains(r#"=";::;>--"#), "{resting}");
    assert!(!resting.contains(r#"=-;::;>--"#), "{resting}");
}

// Pause/play use the same number of cells, so `· MODE` doesn't shift when
// the transport toggles (the emoji ⏸ was wide, ▶ narrow).
#[test]
fn the_transport_icons_line_up() {
    let mut app = App::new(asset_project(), temp_project_path("t"));
    app.loaded = true;
    app.paused = true;
    let paused = status_row(&app);
    app.paused = false;
    let playing = status_row(&app);
    let col = |row: &str| row.find("· NORMAL").unwrap_or(usize::MAX);
    assert!(paused.contains("|| · NORMAL"), "{paused}");
    assert!(playing.contains("|> · NORMAL"), "{playing}");
    assert_eq!(
        col(&paused),
        col(&playing),
        "the mode text must not move when the transport toggles:\n{paused}\n{playing}"
    );
}

// Shortening trims and splits don't remove material, so they get their own
// quieter reactions — never the removal chomp.
#[test]
fn a_trim_or_split_does_not_chomp() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("chomp-trim"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('o'));
    assert_eq!(app.croc.act(), CrocAct::Nibble, "{}", app.message);

    let mut app = App::new(asset_project(), temp_project_path("chomp-split"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('s'));
    assert_eq!(app.croc.act(), CrocAct::Snap, "{}", app.message);
}

#[test]
fn undo_spits_the_material_back() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("spit"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('d'));
    handle_key(&mut app, &fake, key('d'));
    assert_eq!(app.croc.act(), CrocAct::Chomp);
    handle_key(&mut app, &fake, key('u'));
    assert_eq!(app.croc.act(), CrocAct::Spit, "{}", app.message);
    run_action(&mut app, &fake, &Action::Redo);
    assert_eq!(app.croc.act(), CrocAct::Unspit, "redo reverses the spit");
}

#[test]
fn a_refused_edit_hisses() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("refuse"));
    app.loaded = true;
    let ok = run_action(
        &mut app,
        &fake,
        &Action::Delete {
            track: 0,
            target: Target::Clip(999),
            ripple: false,
        },
    );
    assert!(!ok);
    assert_eq!(app.croc.act(), CrocAct::Hiss);
}

#[test]
fn playing_never_idles_in_the_status_row() {
    use super::super::croc::{SWIM1, SWIM2};
    let mut app = App::new(asset_project(), temp_project_path("swim"));
    app.loaded = true;
    app.paused = false;
    for _ in 0..8 {
        let row = status_row(&app);
        assert!(
            row.contains(SWIM1) || row.contains(SWIM2),
            "playing must swim, not idle: {row}"
        );
    }
}

#[test]
fn speed_direction_picks_the_tail() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("speed-croc"));
    app.loaded = true;
    run_action(
        &mut app,
        &fake,
        &Action::SetSpeed {
            track: 0,
            clip: 1,
            rate: Seconds::new(2, 1),
        },
    );
    assert_eq!(app.croc.act(), CrocAct::DashFast, "{}", app.message);
    run_action(
        &mut app,
        &fake,
        &Action::SetSpeed {
            track: 0,
            clip: 1,
            rate: Seconds::new(1, 2),
        },
    );
    assert_eq!(app.croc.act(), CrocAct::DashSlow, "{}", app.message);
}

#[test]
fn the_action_vocabulary_maps_to_reactions() {
    use CrocAct::*;
    let undo = reaction_for(&Action::Undo, false, None);
    assert_eq!(
        undo,
        Some(Spit),
        "undo spits even when it removes an insert"
    );
    assert_eq!(reaction_for(&Action::Undo, true, None), Some(Spit));
    assert_eq!(
        reaction_for(
            &Action::Delete {
                track: 0,
                target: Target::Clip(1),
                ripple: false
            },
            true,
            None
        ),
        Some(Chomp)
    );
    assert_eq!(
        reaction_for(&Action::Split { track: 0, at: 1.0 }, false, None),
        Some(Snap)
    );
    assert_eq!(
        reaction_for(
            &Action::TrimIn {
                track: 0,
                at: 1.0,
                ripple: false
            },
            false,
            None
        ),
        Some(Nibble)
    );
    assert_eq!(
        reaction_for(
            &Action::InsertGap {
                track: 0,
                at: 1.0,
                len: GapLen::Frames(1)
            },
            false,
            None
        ),
        Some(Gulp)
    );
    assert_eq!(
        reaction_for(
            &Action::TrackAdd {
                kind: TrackKind::Video
            },
            false,
            None
        ),
        Some(Gulp)
    );
    assert_eq!(
        reaction_for(
            &Action::Transition {
                track: 0,
                at: 1.0,
                kind: TransitionKind::Dissolve,
                len: TransitionLen::Frames(10)
            },
            false,
            None
        ),
        Some(Slither)
    );
    assert_eq!(
        reaction_for(
            &Action::AddEffect {
                track: 0,
                clip: 1,
                effect: Effect::Eq {
                    brightness: 0.0,
                    contrast: 1.0,
                    saturation: 1.0
                },
                index: None
            },
            false,
            None
        ),
        Some(Wink)
    );
    assert_eq!(
        reaction_for(&Action::TrackMute { track: 0 }, false, None),
        Some(Sleep)
    );
    assert_eq!(
        reaction_for(&Action::AssetRm { id: 1 }, false, None),
        Some(Chomp)
    );
    assert_eq!(reaction_for(&Action::Redo, false, None), Some(Unspit));
    assert_eq!(reaction_for(&Action::Redo, true, None), Some(Unspit));
    assert_eq!(
        reaction_for(
            &Action::Yank {
                track: 0,
                target: Target::At(1.0)
            },
            false,
            None
        ),
        None
    );

    let speed = |rate| {
        reaction_for(
            &Action::SetSpeed {
                track: 0,
                clip: 1,
                rate,
            },
            false,
            Some(Seconds::from_integer(1)),
        )
    };
    assert_eq!(speed(Seconds::new(2, 1)), Some(DashFast));
    assert_eq!(speed(Seconds::new(1, 2)), Some(DashSlow));
    assert_eq!(speed(Seconds::from_integer(1)), None);
}
