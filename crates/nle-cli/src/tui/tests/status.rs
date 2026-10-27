use super::super::actions::{TimelineStamp, removed_material};
use super::super::keys::handle_key;
use super::super::*;
use super::*;

// The status row carries the preview job's progress: finished/total, the
// percentage (including the render in flight), and "queued" when a newer
// edit is waiting.
#[test]
fn the_status_row_shows_preview_progress() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
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
    assert!(app.chomp_until.is_some(), "a delete opens the mouth");
    let open = status_row(&app);
    assert!(open.contains(r#">";::;>--"#), "{open}");

    // Deleting a gap has no id to lose, but the segment count falls.
    let mut app = App::new(asset_project(), temp_project_path("chomp-gap"));
    app.loaded = true;
    app.playhead = 4.0; // the gap
    handle_key(&mut app, &fake, key('d'));
    handle_key(&mut app, &fake, key('d'));
    assert!(app.chomp_until.is_some(), "a gap delete chomps too");

    // After the window it is closed again.
    app.chomp_until = Some(Instant::now() - Duration::from_millis(1));
    let row = status_row(&app);
    assert!(row.contains(r#"=";::;>--"#), "{row}");
    assert!(!row.contains(r#">";::;>--"#), "{row}");
}

// Pause/play use the same number of cells, so `· MODE` doesn't shift when
// the transport toggles (the emoji ⏸ was wide, ▶ narrow).
#[test]
fn the_transport_icons_line_up() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
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

// Shortening trims and splits don't remove material, so no chomp.
#[test]
fn a_trim_or_split_does_not_chomp() {
    let fake = FakeMpv::new();
    let mut app = App::new(asset_project(), temp_project_path("chomp-trim"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('o'));
    assert!(app.chomp_until.is_none(), "{}", app.message);

    let mut app = App::new(asset_project(), temp_project_path("chomp-split"));
    app.loaded = true;
    app.playhead = 1.0;
    handle_key(&mut app, &fake, key('s'));
    assert!(app.chomp_until.is_none(), "{}", app.message);
}

#[test]
fn the_removal_stamp_sees_gaps_and_clips() {
    let stamp = |ids: &[u32], segments| TimelineStamp {
        ids: ids.iter().copied().collect(),
        segments,
    };
    assert!(
        removed_material(&stamp(&[1], 2), &stamp(&[1], 1)),
        "a gap delete lowers the count"
    );
    assert!(
        removed_material(&stamp(&[1], 2), &stamp(&[], 2)),
        "a clip→gap keeps the count but loses the id"
    );
    assert!(
        !removed_material(&stamp(&[1], 2), &stamp(&[1, 2], 3)),
        "a split adds"
    );
    assert!(
        !removed_material(&stamp(&[1], 2), &stamp(&[1], 2)),
        "nothing changed"
    );
}
