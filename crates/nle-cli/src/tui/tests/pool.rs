use super::super::jobs::PreviewMsg;
use super::super::keys::handle_key;
use super::super::keys::pool::{
    close_pool_at, insert_from_pool, load_pool_preview, pool_last_frame, pool_step_target,
    pool_strip_active,
};
use super::super::motion::secs;
use super::super::*;
use super::*;
use nle_core::edit;
use nle_core::time::Seconds;
use nle_core::time::frame_to_secs;
use nle_core::{Project, frame_of};

// Media pool: an asset used by a clip can't be removed; an unused one
// goes. Insert-from-pool is the shared ripple path (tested in core).
#[test]
fn media_pool_guarded_removal() {
    let mut p = asset_project();
    let (spare, _) = p.add_asset(
        "/media/spare.mp4".into(),
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
                duration: Some("5".into()),
                ..Default::default()
            },
        },
    );
    // Insert the spare as a clip (core path), then it's in use.
    let snapshot = TimelineSegment::Clip(Clip {
        id: 0,
        asset: spare,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(5),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let batch = edit::paste_ripple(&p, &snapshot, 0.0).unwrap();
    p.timeline = {
        let mut q = p.clone();
        edit::History::default().apply(&mut q, batch, 0).unwrap();
        q.timeline
    };
    assert_eq!(p.asset_usage(spare).len(), 1);
    assert!(p.remove_asset(spare).is_err(), "in use → refused");

    // An unused import removes cleanly.
    let (free, _) = p.add_asset(
        "/media/free.mp4".into(),
        p.asset(spare).unwrap().probe.clone(),
    );
    assert!(free != spare);
    assert!(p.remove_asset(free).is_ok());
}

// Inserting an asset from the pool is recorded in history and undoes.
#[test]
fn media_pool_insert_is_undoable() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 0.0;
    open_pool_mode(&mut app, 0); // first asset
    let end_before = app.project.timeline.end();

    // Same call insert_from_pool makes, minus the player hook.
    let snapshot = TimelineSegment::Clip(Clip {
        id: 0,
        asset: app.project.media[0].id,
        asset_in: Seconds::from_integer(0),
        asset_out: app.project.media[0].duration().unwrap(),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let batch = edit::paste_ripple(&app.project, &snapshot, app.playhead).unwrap();
    let frame = frame_of(app.project.fps.as_ref(), 0.0);
    app.history.apply(&mut app.project, batch, frame).unwrap();
    assert!(
        app.project.timeline.end() > end_before,
        "insert grew the timeline"
    );

    let outcome = app.history.undo(&mut app.project).unwrap();
    assert_eq!(
        app.project.timeline.end(),
        end_before,
        "undo removed the insert"
    );
    assert_eq!(
        frame_to_secs(app.project.fps.as_ref(), outcome.playhead),
        0.0
    );
}

// Inserting from the pool leaves it: the monitor goes back to the timeline
// instead of staying on the raw asset.
#[test]
fn pool_insert_exits_pool_mode() {
    let mut app = App::new(asset_project(), temp_project_path("pool-insert-exit"));
    app.playhead = 0.0;
    open_pool_mode(&mut app, 0);
    let fake = FakeMpv::new();
    let end_before = app.project.timeline.end();

    insert_from_pool(&mut app, &fake);

    assert!(!is_pool(&app), "inserting returns to the timeline");
    assert!(app.project.timeline.end() > end_before, "the clip landed");
    assert_eq!(app.message, "timeline");
}

// An explicit landing position wins over the pool's remembered resume.
#[test]
fn close_pool_at_keeps_the_landing_position() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 5.0;
    open_pool_mode(&mut app, 0);
    close_pool_at(&mut app, &FakeMpv::new(), 5.0);
    assert!(!is_pool(&app));
    assert!((app.playhead - 5.0).abs() < 1e-9);
}

// Leaving the pool is asynchronous: the mode flips to Normal immediately,
// but mpv is still reporting the pooled file until the EDL reload lands.
// That clock must not move the timeline playhead.
#[test]
fn sync_ignores_the_pooled_clock_after_leaving_the_pool() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    open_pool_mode(&mut app, 0);
    load_pool_preview(&mut app, &FakeMpv::new());
    assert!(app.pool_on_monitor);

    close_pool_at(&mut app, &FakeMpv::new(), 6.0);
    assert!(!is_pool(&app));
    assert!(
        app.pool_on_monitor,
        "still on the raw file until the EDL loads"
    );

    // `poll_preview` records the landing before it loads the EDL.
    app.pool_handoff = Some(6.0);

    // The old pooled clock still reports its end (and duration). Even
    // though 2.9667 would snap to the cut at 3.0, it must be ignored.
    let pooled = PlaybackState {
        time_pos: Some(2.9667),
        paused: false,
        file_loaded: true,
        duration: Some(3.0),
        ..Default::default()
    };
    app.sync(&pooled);
    assert_eq!(
        app.playhead, 6.0,
        "the pooled clock must not move the playhead"
    );

    // Once the EDL reports the landing/duration, its clock drives again.
    let edl = PlaybackState {
        time_pos: Some(6.0),
        paused: false,
        file_loaded: true,
        duration: Some(7.0),
        ..Default::default()
    };
    app.sync(&edl);
    assert_eq!(app.playhead, 6.0);
    assert!(!app.pool_on_monitor);
    assert!(app.pool_handoff.is_none());

    let raw = PlaybackState {
        time_pos: Some(2.5),
        paused: false,
        file_loaded: true,
        duration: Some(7.0),
        ..Default::default()
    };
    app.sync(&raw);
    assert!((app.playhead - 2.5).abs() < 1e-9);
}

// A pool playthrough must not move the timeline cursor: the pool has its
// own clock, and EOF only ends the preview. The pre-pool playhead (and the
// pool's remembered resume) stay put.
#[test]
fn pool_eof_keeps_the_timeline_playhead() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json")); // 30 fps
    app.playhead = 1.5;
    open_pool_mode(&mut app, 0); // resume = the pre-pool position
    let before = app.playhead;
    let eof = PlaybackState {
        paused: true,
        file_loaded: true,
        eof: true,
        ..Default::default()
    };

    app.sync(&eof);
    assert_eq!(app.playhead, before, "the timeline cursor stays put");
    assert_eq!(pool_resume(&app), before, "and so does its resume");
    assert!(
        pool_strip_active(&app),
        "the asset stays in the timeline panel at EOF"
    );

    // A second EOF tick is still a no-op.
    app.sync(&eof);
    assert_eq!(app.playhead, before);
    assert_eq!(pool_resume(&app), before);
}

// Inserting a 6.006 s asset uses a whole-frame duration, so no
// sub-frame stump is produced.
#[test]
fn pool_insert_uses_whole_frames() {
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    p.add_asset(
        "/media/blue.mp4".into(),
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
                duration: Some("6.006".into()),
                ..Default::default()
            },
        },
    );
    let mut app = App::new(p, PathBuf::from("t.json"));
    // Mirror insert_from_pool's whole-frame math.
    let d = app.project.media[0].duration().unwrap();
    let fps = app.project.fps.unwrap();
    let frames = (secs(&d) * secs(&fps)).floor().max(1.0) as i64;
    let whole = Seconds::new(frames, 1) / fps;
    assert_eq!(
        whole,
        Seconds::new(180, 30),
        "6.006 floored to 180 frames (6.0 s)"
    );
    let snapshot = TimelineSegment::Clip(Clip {
        id: 0,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: whole,
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let batch = edit::paste_ripple(&app.project, &snapshot, 0.0).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    // No zero-length or sub-frame segment created.
    for s in &app.project.timeline.video_track().unwrap().segments {
        assert!(s.duration() > Seconds::from_integer(0));
    }
}

// Frame steps advance on the asset's grid and clamp to its last frame.
#[test]
fn pool_step_moves_one_frame_and_clamps() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    open_pool_mode(&mut app, 0);
    let fps = app.project.media[0].fps().unwrap();
    let f = secs(&fps);
    set_pool_pos(&mut app, 0.0);
    // Three forward steps land exactly 3 frames in (snapped, no drift).
    for _ in 0..3 {
        let (t, _) = pool_step_target(&app, 1);
        set_pool_pos(&mut app, t);
    }
    assert!((pool_pos(&app) - 3.0 / f).abs() < 1e-9);
    // Back two → one frame in.
    let (t, _) = pool_step_target(&app, -1);
    set_pool_pos(&mut app, t);
    let (t, _) = pool_step_target(&app, -1);
    assert!((t - 1.0 / f).abs() < 1e-9);
    // Clamps at 0 and at the last frame.
    let (t0, _) = pool_step_target(&app, -99);
    assert_eq!(t0, 0.0);
    let last = pool_last_frame(&app, 10.0);
    set_pool_pos(&mut app, last);
    let (tl, _) = pool_step_target(&app, 99);
    assert!((tl - last).abs() < 1e-9, "clamped to the last frame");
}

// The pool clock's last frame is one frame before the asset duration.
#[test]
fn pool_last_frame_is_one_before_duration() {
    let app = App::new(asset_project(), PathBuf::from("t.json"));
    // 30 fps, 10 s asset → last frame at 10 - 1/30.
    assert!((pool_last_frame(&app, 10.0) - (10.0 - 1.0 / 30.0)).abs() < 1e-9);
}

// The pool's `gg`: a lone `g` only arms the sequence; the second seeks.
#[test]
fn pool_gg_waits_for_the_second_key() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.loaded = true;
    open_pool_mode(&mut app, 0);
    let fake = FakeMpv::new();

    handle_key(&mut app, &fake, key('g'));
    assert!(fake.calls().is_empty(), "a lone g must not seek");
    handle_key(&mut app, &fake, key('g'));
    assert_eq!(fake.calls(), vec![Call::SeekAbs(0.0)]);
}

// Pool space: restart at EOF, otherwise toggle.
#[test]
fn pool_space_restarts_at_eof_else_toggles() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.loaded = true;
    open_pool_mode(&mut app, 0);

    let fake = FakeMpv::new();
    app.eof = false;
    handle_key(&mut app, &fake, key(' '));
    assert_eq!(fake.calls(), vec![Call::TogglePause]);

    let fake = FakeMpv::new();
    app.eof = true;
    handle_key(&mut app, &fake, key(' '));
    assert_eq!(
        fake.calls(),
        vec![Call::SeekAbs(0.0), Call::SetPause(false)]
    );
    assert_eq!(pool_pos(&app), 0.0);
}

// Undo while the pool is open must keep the pool preview (not kick the
// monitor to the timeline EDL).
#[test]
fn pool_undo_keeps_the_pool_preview() {
    let mut app = App::new(asset_project(), temp_project_path("pool-undo"));
    app.playhead = 1.0;
    let batch = edit::trim_in_ripple(&app.project, app.playhead).unwrap();
    let frame = frame_of(app.project.fps.as_ref(), app.playhead);
    app.history.apply(&mut app.project, batch, frame).unwrap();
    app.loaded = true;
    open_pool_mode(&mut app, 0);

    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('u'));
    assert!(!app.history.can_undo(), "the trim was undone");
    assert!(is_pool(&app), "still in the pool");
    match fake.calls().last() {
        Some(Call::Load(url, _)) => assert!(url.ends_with("red.mp4"), "{url}"),
        other => panic!("expected a pool reload, got {other:?}"),
    }
}

// Removing a pooled asset is recorded and undoable, and the preview
// follows the new selection.
#[test]
fn pool_remove_is_undoable() {
    let mut p = asset_project();
    let (spare, _) = p.add_asset("/media/spare.mp4".into(), p.asset(1).unwrap().probe.clone());
    let mut app = App::new(p, temp_project_path("pool-remove"));
    app.loaded = true;
    open_pool_mode(&mut app, 1); // the spare row
    let fake = FakeMpv::new();

    handle_key(&mut app, &fake, key('d'));
    assert_eq!(app.project.media.len(), 1, "spare removed");
    assert!(app.history.can_undo());
    assert!(is_pool(&app), "still in the pool");
    assert!(
        matches!(fake.calls().last(), Some(Call::Load(url, _)) if url.ends_with("red.mp4")),
        "preview moved to the remaining asset: {:?}",
        fake.calls()
    );

    handle_key(&mut app, &fake, key('u'));
    assert_eq!(app.project.media.len(), 2, "undo restored the spare");
    assert!(app.project.asset(spare).is_some());
}

// A pool always hands the timeline back paused at the pre-pool cursor:
// mpv reports pause=false at EOF, so the EDL must not load playing.
#[test]
fn a_pool_close_pauses_at_the_landing() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.paused = false; // even a playing timeline is paused on hand-back
    open_pool_mode(&mut app, 0);
    close_pool_at(&mut app, &fake, 1.0);
    assert_eq!(app.pending_pause, Some(true));
    assert!(
        fake.calls()
            .iter()
            .any(|c| matches!(c, Call::SetPause(true))),
        "pause before the EDL loads: {:?}",
        fake.calls()
    );

    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    tx.send(PreviewMsg::Done(Ok("edl://timeline".into())))
        .unwrap();
    poll_preview(&mut app, &fake);
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|c| matches!(c, Call::SetPause(true)))
            .count(),
        2,
        "and again on the new file: {:?}",
        fake.calls()
    );
    assert_eq!(app.pending_pause, None);
}

// Leaving the pool records the landing; the old pooled clock stays
// ignored until the EDL is actually the monitor's file.
#[test]
fn poll_preview_records_the_pool_landing() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 2.0;
    app.pool_on_monitor = true;
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    tx.send(PreviewMsg::Done(Ok("edl://timeline".into())))
        .unwrap();
    poll_preview(&mut app, &fake);
    assert!(app.pool_on_monitor, "still ignores the pooled clock");
    assert_eq!(app.pool_handoff, Some(2.0));
    assert_eq!(
        fake.calls(),
        vec![Call::Load("edl://timeline".into(), Some(2.0))]
    );
}

// While the pool owns the monitor, a finished timeline build must not
// replace the asset preview.
#[test]
fn a_preview_finishing_in_the_pool_does_not_steal_the_monitor() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    open_pool_mode(&mut app, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    tx.send(PreviewMsg::Done(Ok("edl://timeline".into())))
        .unwrap();
    poll_preview(&mut app, &fake);
    assert!(fake.calls().is_empty(), "the pool asset stays");
    assert!(app.preview.is_none());
}

// In the pool the pooled asset legitimately owns the monitor, so a
// failure there leaves the hand-off flags alone (the asset keeps the
// preview and the pool playhead keeps following it).
#[test]
fn a_failed_handoff_in_the_pool_keeps_the_flags() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    open_pool_mode(&mut app, 0);
    app.set_message("media");
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    tx.send(PreviewMsg::Done(Err("worker boom".into())))
        .unwrap();
    poll_preview(&mut app, &fake);
    assert!(app.pool_on_monitor, "the pool owns the monitor");
    assert_eq!(app.message, "media", "the pool drops build failures");
}

// `t`/`T` change the insert target while the pool is open, without
// disturbing the asset preview on the monitor.
#[test]
fn pool_t_switches_the_insert_track() {
    let mut p = asset_project();
    p.timeline.add_track(TrackKind::Audio); // track 1
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.loaded = true;
    open_pool_mode(&mut app, 0);
    let fake = FakeMpv::new();

    handle_key(&mut app, &fake, key('t'));
    assert_eq!(app.active(), 1, "t cycles to the next track");
    assert!(app.message.contains("a1"), "{}", app.message);
    assert!(
        fake.calls().is_empty(),
        "the asset preview is untouched: {:?}",
        fake.calls()
    );

    handle_key(&mut app, &fake, key('T'));
    assert_eq!(app.active(), 0, "T cycles back");

    // `<2>t` jumps to track 2 (1-based).
    handle_key(&mut app, &fake, key('2'));
    handle_key(&mut app, &fake, key('t'));
    assert_eq!(app.active(), 1);
    assert!(is_pool(&app), "still in the pool");
}

// Reopening the pool returns to the row last selected.
#[test]
fn pool_reopen_returns_to_the_last_row() {
    let mut p = asset_project();
    p.add_asset("/media/blue.mp4".into(), p.asset(1).unwrap().probe.clone());
    // Drop the gap so close_pool's composite EDL needs no black file (ffmpeg).
    p.timeline.video_track_mut().unwrap().segments.remove(1);
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.loaded = true;
    let fake = FakeMpv::new();

    handle_key(&mut app, &fake, key('m')); // open
    handle_key(&mut app, &fake, key('j')); // second row
    assert_eq!(pool_sel(&app), Some(1));
    set_pool_pos(&mut app, 3.0);

    handle_key(&mut app, &fake, key('m')); // close (captures row/clock)
    assert!(!is_pool(&app));
    handle_key(&mut app, &fake, key('m')); // reopen
    assert_eq!(pool_sel(&app), Some(1), "reopened on the last row");
    assert_eq!(pool_pos(&app), 3.0, "and at the last preview clock");
}

// The `i` command closes the panel to use the command line; the selected
// asset and clock survive that detour too.
#[test]
fn pool_command_preserves_the_pool_state() {
    let mut p = asset_project();
    p.add_asset("/media/blue.mp4".into(), p.asset(1).unwrap().probe.clone());
    p.timeline.video_track_mut().unwrap().segments.remove(1); // no black file
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.loaded = true;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('m'));
    handle_key(&mut app, &fake, key('j'));
    set_pool_pos(&mut app, 4.0);
    handle_key(&mut app, &fake, key('i'));
    assert!(matches!(app.mode, Mode::Command(_)));
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
    assert!(!is_pool(&app));

    handle_key(&mut app, &fake, key('m'));
    assert_eq!(pool_sel(&app), Some(1), "same asset selected");
    assert_eq!(pool_pos(&app), 4.0, "same preview clock");
}

// `i` in the pool opens the import path prompt (command line pre-filled).
#[test]
fn pool_i_opens_the_import_prompt() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.loaded = true;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('m')); // open the pool
    handle_key(&mut app, &fake, key('i'));
    assert_eq!(app.mode, Mode::Command("import ".to_string()));
}
