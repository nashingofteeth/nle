use super::super::jobs::PreviewMsg;
use super::super::motion::{fmt_timecode, secs};
use super::super::*;
use super::*;

// The transport flush: a move is detected in frames (the two float
// paths can differ in the last ulp, which must not retrigger it), and
// flushing writes the cursor `nle seek` reads.
#[test]
fn transport_moves_flush_the_stored_playhead() {
    let path = temp_project_path("transport-flush");
    asset_project().save(&path).unwrap();
    let mut app = App::new(asset_project(), path.clone());
    app.loaded = true;
    assert!(
        !playhead_moved(&app),
        "the session starts on the stored frame"
    );

    // A transport key moves the playhead; the diff notices it.
    handle_key(&mut app, &FakeMpv::new(), key('l'));
    assert_eq!(app.playhead, 1.0);
    assert!(playhead_moved(&app));
    persist_playhead(&mut app).unwrap();
    assert_eq!(
        Project::load(&path).unwrap().playhead,
        Seconds::from_integer(1)
    );
    assert!(!playhead_moved(&app), "flushing clears the diff");

    // Sub-frame noise must not look like a move.
    app.playhead = secs(&app.project.playhead) + 1e-9;
    assert!(!playhead_moved(&app), "frame-granular, not float-granular");
    assert!(
        !app.external_change(),
        "our own save is not a foreign change"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

// A shell write (`nle seek`, `nle yank`, an edit) is adopted within a
// tick instead of being clobbered by our next save.
#[test]
fn a_foreign_write_is_seen_and_adopted() {
    let path = temp_project_path("watch-adopt");
    watchable_project().save(&path).unwrap();
    let mut app = App::new(watchable_project(), path.clone());
    assert!(!app.external_change(), "we start in sync with the file");

    // The shell parks the cursor and yanks a gap into the register.
    let mut p = Project::load(&path).unwrap();
    p.set_playhead(3.0);
    p.save(&path).unwrap();
    let reg = nle_core::Register {
        segment: TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(2),
        }),
        path: None,
    };
    nle_core::save_register(&path, Some(&reg)).unwrap();

    assert!(app.external_change());
    assert!(watch_foreign(&mut app), "Normal mode adopts it right away");
    reload_project(&mut app, &FakeMpv::new(), true).unwrap();
    assert_eq!(app.playhead, 3.0, "the shell's cursor");
    assert_eq!(app.register, Some(reg), "and its yank");
    assert!(!app.external_change(), "the watch re-baselines");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

// `R` is a manual reload: it must keep the live cursor, while a foreign
// write still adopts the shell's saved playhead.
#[test]
fn a_user_reload_keeps_the_live_playhead() {
    let path = temp_project_path("reload-keep");
    watchable_project().save(&path).unwrap();
    let mut app = App::new(watchable_project(), path.clone());
    app.playhead = 2.0;

    reload_project(&mut app, &FakeMpv::new(), false).unwrap();
    assert_eq!(app.playhead, 2.0, "the manual reload stays put");

    // A shell moved the stored playhead; adopting it (foreign reload)
    // still follows the file.
    let mut p = Project::load(&path).unwrap();
    p.set_playhead(1.0);
    p.save(&path).unwrap();
    reload_project(&mut app, &FakeMpv::new(), true).unwrap();
    assert_eq!(app.playhead, 1.0, "a foreign reload adopts the cursor");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

// A manual reload clamps the live cursor into a shorter timeline.
#[test]
fn a_user_reload_clamps_a_stale_playhead() {
    let path = temp_project_path("reload-clamp");
    watchable_project().save(&path).unwrap();
    let mut app = App::new(watchable_project(), path.clone());
    app.playhead = 100.0; // past the 7 s fixture
    reload_project(&mut app, &FakeMpv::new(), false).unwrap();
    assert!(app.playhead <= app.last_frame());
    assert!(app.playhead >= 0.0);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

// The guard: never write over a change we haven't seen.
#[test]
fn a_foreign_write_is_not_clobbered() {
    let path = temp_project_path("watch-guard");
    watchable_project().save(&path).unwrap();
    let mut app = App::new(watchable_project(), path.clone());
    let mut p = Project::load(&path).unwrap();
    p.set_playhead(3.0);
    p.save(&path).unwrap();

    app.playhead = 5.0;
    assert!(!persist_playhead(&mut app).unwrap(), "the guard refuses");
    assert_eq!(
        Project::load(&path).unwrap().playhead,
        Seconds::from_integer(3),
        "the shell's cursor survived"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

// A mode holds indices into the current state, so a foreign write waits
// for Normal instead of yanking the ground out from under it.
#[test]
fn watch_foreign_waits_for_a_mode_to_end() {
    let path = temp_project_path("watch-mode");
    watchable_project().save(&path).unwrap();
    let mut app = App::new(watchable_project(), path.clone());
    app.mode = Mode::Extend(ExtendState {
        track: 0,
        armed: Armed::Clip { id: 1, head: true },
        edge_pos: 0.0,
        ripple: false,
        lo: 0.0,
        hi: None,
        mat: None,
        preview_off: 0.0,
    });
    let mut p = Project::load(&path).unwrap();
    p.set_playhead(2.0);
    p.save(&path).unwrap();

    assert!(
        !watch_foreign(&mut app),
        "no reload while a mode holds state"
    );
    assert!(app.reload_pending);
    app.mode = Mode::Normal;
    assert!(watch_foreign(&mut app), "applied once Normal");
    assert!(!app.reload_pending);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

// `:seek` moves the playhead and the stored cursor in one step, so a
// shell and the TUI share one position.
#[test]
fn seek_moves_the_playhead_and_persists_it() {
    let path = temp_project_path("seek");
    asset_project().save(&path).unwrap();
    let mut app = App::new(asset_project(), path.clone());
    app.loaded = true;
    assert_eq!(cmd(&mut app, "seek 2"), "playhead → 00:02:00");
    assert_eq!(app.playhead, 2.0);
    assert_eq!(
        Project::load(&path).unwrap().playhead,
        Seconds::from_integer(2)
    );
    // Past the end clamps onto the last frame, like every other seek.
    cmd(&mut app, "seek 999");
    assert_eq!(app.playhead, app.last_frame(), "not the end boundary");
    assert!(app.playhead < app.total());
    // An edit re-saves the cursor with the session position.
    cmd(&mut app, "seek 2");
    cmd(&mut app, "split");
    assert_eq!(
        Project::load(&path).unwrap().playhead,
        Seconds::from_integer(2)
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    let _ = std::fs::remove_file(nle_core::register_path(&path));
}

#[test]
fn sync_applies_grid_and_cut_snapping_while_playing() {
    let mut p = project();
    p.fps = Some(Seconds::new(30, 1));
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.sync(&PlaybackState {
        time_pos: Some(5.0 + 1.0 / 30.0),
        paused: false,
        file_loaded: true,
        ..Default::default()
    });
    assert_eq!(app.playhead, 5.0);
    app.sync(&PlaybackState {
        time_pos: Some(4.011),
        paused: false,
        file_loaded: true,
        ..Default::default()
    });
    assert_eq!(app.playhead, 4.0);
}

#[test]
fn sync_ignores_mpv_positions_while_paused() {
    let mut p = project();
    p.fps = Some(Seconds::new(30, 1));
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = 2.0;
    app.sync(&PlaybackState {
        time_pos: Some(5.0 + 1.0 / 30.0),
        paused: true,
        file_loaded: true,
        ..Default::default()
    });
    assert_eq!(app.playhead, 2.0);
    app.sync(&PlaybackState {
        time_pos: Some(5.0 + 1.0 / 30.0),
        paused: false,
        file_loaded: true,
        ..Default::default()
    });
    assert_eq!(app.playhead, 5.0);
}

// While the extend preview plays, the playhead must stay within the
// armed range (a head can't run past the segment's end).
#[test]
fn sync_clamps_to_the_extend_range_while_playing() {
    let mut p = project();
    p.fps = Some(Seconds::new(30, 1));
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.mode = Mode::Extend(ExtendState {
        track: 0,
        armed: Armed::Clip { id: 1, head: true },
        edge_pos: 0.0,
        ripple: false,
        lo: -1.0,
        hi: Some(3.0),
        mat: None,
        preview_off: 0.0,
    });
    app.sync(&PlaybackState {
        time_pos: Some(9.0),
        paused: false,
        file_loaded: true,
        ..Default::default()
    });
    assert_eq!(app.playhead, 3.0, "clamped at the segment end");
    app.sync(&PlaybackState {
        time_pos: Some(-5.0),
        paused: false,
        file_loaded: true,
        ..Default::default()
    });
    assert_eq!(app.playhead, -1.0, "clamped at the range start");
}

#[test]
fn frame_steps_move_one_frame_and_clamp() {
    let mut p = project();
    p.fps = Some(Seconds::new(30, 1));
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.step_playhead(1);
    assert_eq!(app.playhead, 1.0 / 30.0);
    app.step_playhead(-1);
    assert_eq!(app.playhead, 0.0);
    app.step_playhead(-1);
    assert_eq!(app.playhead, 0.0, "clamped at the start");
    app.playhead = 1000.0;
    app.step_playhead(1);
    assert_eq!(app.playhead, 209.0 / 30.0, "clamped at the last frame");
}

#[test]
fn switching_track_keeps_the_composite_monitor() {
    let mut p = asset_project();
    p.timeline.add_track(TrackKind::Audio);
    let mut app = App::new(p, temp_project_path("track-switch"));
    let fake = FakeMpv::new();
    set_active_track(&mut app, 1);
    assert_eq!(app.active(), 1);
    assert!(app.message.contains("a1"), "{}", app.message);
    // The monitor plays the composite of all video tracks, so switching
    // the edit target does not disturb it.
    assert!(
        fake.calls().is_empty(),
        "no monitor reload: {:?}",
        fake.calls()
    );
}

// POOL's main clock is the asset clock, matching the strip marker; the
// project cursor is restored when the pool closes.
#[test]
fn the_pool_status_shows_the_asset_clock() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 1.5;
    open_pool_mode(&mut app, 0);
    set_pool_pos(&mut app, 4.0);
    let text = status_row(&app);
    let fps = Seconds::new(30, 1);
    assert!(text.contains("POOL"), "{text}");
    assert!(
        text.contains(&fmt_timecode(4.0, Some(&fps))),
        "asset clock: {text}"
    );
    assert!(!text.contains("pool "), "no split label: {text}");
}

#[test]
fn transport_records_monitor_calls() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.loaded = true;
    let fake = FakeMpv::new();

    handle_key(&mut app, &fake, key('l'));
    assert_eq!(app.playhead, 1.0);
    assert_eq!(fake.calls(), vec![Call::SeekRel(1.0)]);

    // A count multiplies the seek.
    handle_key(&mut app, &fake, key('3'));
    handle_key(&mut app, &fake, key('l'));
    assert_eq!(app.playhead, 4.0);
    assert_eq!(fake.calls().last(), Some(&Call::SeekRel(3.0)));

    handle_key(&mut app, &fake, key('f'));
    let want = 4.0 + 1.0 / 30.0;
    assert!((app.playhead - want).abs() < 1e-9);
    // f/F move the monitor to the model frame on the project grid (an
    // absolute seek), never mpv's source-rate `frame_step`.
    match fake.calls().last() {
        Some(Call::SeekAbs(t)) => assert!((t - (want + 0.004)).abs() < 1e-9),
        other => panic!("f must seek the monitor to the model frame, got {other:?}"),
    }
}

// The worker's progress and completion: the EDL loads at the playhead in
// force when it lands, and the indicator clears.
#[test]
fn poll_preview_loads_the_built_edl() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    app.preview_progress = Some(PreviewProgress::default());

    tx.send(PreviewMsg::Progress {
        done: 1,
        total: 2,
        frac: 0.25,
    })
    .unwrap();
    poll_preview(&mut app, &fake);
    assert_eq!(
        app.preview_progress,
        Some(PreviewProgress {
            done: 1,
            total: 2,
            frac: 0.25
        })
    );
    assert!(app.preview.is_some());

    app.playhead = 1.5;
    tx.send(PreviewMsg::Done(Ok("edl://built".into()))).unwrap();
    poll_preview(&mut app, &fake);
    assert_eq!(
        fake.calls(),
        vec![Call::Load("edl://built".into(), Some(1.5))]
    );
    assert!(app.preview.is_none() && app.preview_progress.is_none());
}

// A build that finishes after a newer edit is discarded — its EDL is
// stale — and the queued build starts instead.
#[test]
fn an_obsolete_preview_is_not_loaded() {
    let fake = FakeMpv::new();
    let mut p = asset_project();
    p.timeline.tracks[0].segments.remove(1); // C1 | C2: no gap, no proxy
    let mut app = App::new(p, temp_project_path("preview-stale"));
    app.preview_enabled = true;
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    app.preview_dirty = true;
    tx.send(PreviewMsg::Done(Ok("edl://old".into()))).unwrap();
    poll_preview(&mut app, &fake);
    assert!(fake.calls().is_empty(), "the stale EDL is not loaded");
    assert!(app.preview.is_some(), "the queued build starts");
    assert!(!app.preview_dirty);
}

// A monitor failure must not erase what happened to the timeline: the
// notice is kept and the reason appended.
#[test]
fn a_preview_failure_keeps_the_edit_notice() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.set_message("effect on clip #1");
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    tx.send(PreviewMsg::Done(Err("boom".into()))).unwrap();
    poll_preview(&mut app, &fake);
    assert_eq!(app.message, "effect on clip #1 — preview failed: boom");
    assert!(app.preview.is_none());
}

// Extend mode owns the monitor the same way (over the material preview,
// whose clock is translated through `preview_off`): a timeline build
// that finishes mid-session must not replace that picture — and its
// landing through `sync`'s `t + preview_off` would move the playhead
// off by the preview origin while playing.
#[test]
fn a_preview_finishing_in_extend_mode_does_not_steal_the_monitor() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.mode = Mode::Extend(ExtendState {
        track: 0,
        armed: Armed::Clip { id: 1, head: false },
        edge_pos: 3.0,
        ripple: false,
        lo: 0.0,
        hi: Some(9.0),
        mat: None,
        preview_off: -1.0,
    });
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    tx.send(PreviewMsg::Done(Ok("edl://timeline".into())))
        .unwrap();
    poll_preview(&mut app, &fake);
    assert!(fake.calls().is_empty(), "the material preview stays");
    assert!(app.preview.is_none(), "the build is not queued to restart");
    assert!(!app.preview_dirty);
}

// Seeking while the monitor is still showing the pooled file / material
// preview (mode Normal, EDL not landed) moves the landing: mpv applies
// the seek to the file being loaded, and the wait must end at the
// position the user chose, not the one they left.
#[test]
fn a_seek_during_a_handoff_moves_the_landing() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.loaded = true; // a file is on the monitor (the pooled one)
    app.pool_on_monitor = true;
    app.pool_handoff = Some(1.0);
    app.seek_to(&fake, 2.5);
    assert_eq!(app.pool_handoff, Some(2.5));
    assert_eq!(app.playhead, 2.5);
    assert!(app.pool_on_monitor, "still waiting for the EDL to land");

    // Relative seeks are seeks too: the first post-seek report is the
    // moved position, so the wait must end there (otherwise a small nudge
    // snaps back within tolerance, a large one never lands at all).
    app.pool_handoff = Some(2.5);
    app.seek_rel(&fake, 1.0);
    assert_eq!(app.playhead, 3.5);
    assert_eq!(app.pool_handoff, Some(3.5));
    app.seek_rel(&fake, -0.5);
    assert_eq!(app.pool_handoff, Some(3.0));
}

// A failed rebuild during a monitor hand-off must give the hand-off up:
// the timeline EDL will never land, and leaving `pool_on_monitor` set
// would have the playhead ignore mpv for the rest of the session.
#[test]
fn a_failed_handoff_gives_the_hand_off_up() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.set_message("timeline");
    app.pool_on_monitor = true;
    app.pool_handoff = Some(2.0);
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    tx.send(PreviewMsg::Done(Err("ffmpeg: no such file".into())))
        .unwrap();
    poll_preview(&mut app, &fake);
    assert!(!app.pool_on_monitor, "the playhead must follow again");
    assert_eq!(app.pool_handoff, None);
    assert_eq!(
        app.message,
        "timeline — preview failed: ffmpeg: no such file"
    );
}
