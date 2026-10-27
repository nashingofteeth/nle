use super::super::actions::{
    Anchor, GapAnchor, MediaAnchor, clip_anchor, edit_anchor, edit_yank, reanchor_playhead,
    trim_key,
};
use super::super::keys::handle_key;
use super::super::*;
use super::*;
use nle_core::edit;
use nle_core::frame_of;
use nle_core::time::Seconds;
use nle_core::time::frame_to_secs;

// Post-action playhead placement, one row per settled rule. Each case
// starts fresh, seeks with `l`, presses the keys, and asserts where the
// playhead lands.
#[test]
fn playhead_placement_after_each_action() {
    let cases: &[(&str, &str, f64, &str)] = &[
        ("ll", "dd", 2.0, "plain delete keeps the offset in the gap"),
        ("ll", "D", 0.0, "ripple delete lands on the seam"),
        ("ll", "s", 2.0, "split leaves it"),
        ("ll", "_", 2.0, "insert gap leaves it"),
        ("ll", "i", 2.0, "a gap-left head trim leaves it"),
        (
            "ll",
            "I",
            0.0,
            "a ripple head trim moves it back by the trim",
        ),
        ("ll", "o", 2.0, "a tail trim leaves it"),
        ("ll", "O", 2.0, "a ripple tail trim leaves it"),
        ("ll", "yy", 2.0, "a yank is a read"),
        ("ll", "yyp", 2.0, "paste lands on the pasted clip's start"),
        (
            "ll",
            "yyP",
            2.0,
            "a ripple paste lands on the pasted clip's start",
        ),
        (
            "lll",
            "X",
            2.5,
            "a transition lands in the middle of the blend",
        ),
        ("lll", "dd", 3.0, "deleting the gap lands on the seam"),
        ("ll", "u", 2.0, "undo restores the pre-edit position"),
    ];
    for (seek, keys, want, why) in cases {
        let mut app = App::new(asset_project(), temp_project_path("place"));
        app.loaded = true;
        let fake = FakeMpv::new();
        for k in seek.chars() {
            handle_key(&mut app, &fake, key(k));
        }
        for k in keys.chars() {
            handle_key(&mut app, &fake, key(k));
        }
        assert!(
            (app.playhead - want).abs() < 1e-9,
            "{keys:?} from {seek} → {} (want {want}) — {why}",
            app.playhead
        );
    }
}

// The playhead never lands on the `end` boundary — a timeline of `dur`
// spans `[0, dur)` — so a trim that leaves its clip last clamps onto the
// clip's final frame.
#[test]
fn the_playhead_clamps_to_the_last_frame_after_a_trim() {
    let mut p = asset_project();
    let a = p.media[0].id;
    p.timeline.tracks[0].segments = vec![TimelineSegment::Clip(nle_core::Clip {
        id: 1,
        asset: a,
        asset_in: Seconds::from_integer(1),
        asset_out: Seconds::from_integer(5),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let mut app = App::new(p, temp_project_path("clamp"));
    app.loaded = true;
    app.playhead = 2.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('O')); // ripple trim out to 2.0
    assert_eq!(app.total(), 2.0, "the timeline shrank to the clip");
    assert_eq!(app.playhead, app.last_frame(), "on the final frame");
    assert!(app.playhead < app.total());
}

// A track change doesn't move the active track's media: the playhead stays
// put (`track rm` re-indexes, so an anchor against the old active would
// resolve against the wrong track).
#[test]
fn a_track_removal_leaves_the_playhead_put() {
    let mut p = asset_project();
    p.timeline.add_track(TrackKind::Video);
    let a = p.media[0].id;
    p.timeline.tracks[1].segments = vec![TimelineSegment::Clip(nle_core::Clip {
        id: 9,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(4),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let mut app = App::new(p, temp_project_path("track-rm"));
    app.loaded = true;
    app.active_track = 1;
    app.playhead = 2.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key(':'));
    for c in "track rm 2".chars() {
        handle_key(&mut app, &fake, key(c));
    }
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert_eq!(app.playhead, 2.0, "the playhead stays put");
}

// A blend's far edge is still the blend: `index_at` gives the boundary to
// the follower, but anchoring must see the blend — or a trim that moves the
// edge to the playhead slides it onto the follower's frame, and removing
// the blend follows that frame into the clip instead of landing on the cut.
#[test]
fn trimming_at_a_blends_edge_keeps_the_playhead() {
    let fake = FakeMpv::new();
    // Inside the blend: `o` moves the far edge to the playhead.
    let mut app = App::new(blend_pair(), temp_project_path("blend-edge"));
    app.loaded = true;
    app.playhead = 2.5;
    handle_key(&mut app, &fake, key('o'));
    assert_eq!(app.playhead, 2.5, "the far edge moved to the playhead");

    // On the blend's far edge: `i` removes the blend (the segment ending
    // there) and lands on the cut the restore leaves.
    let mut app = App::new(blend_pair(), temp_project_path("blend-edge2"));
    app.loaded = true;
    app.playhead = 3.0;
    handle_key(&mut app, &fake, key('i'));
    assert_eq!(app.playhead, 3.0, "the cut the restore leaves");
}

// A blend's far edge is not the neighbour's edge: `o` at the blend's end
// and `i` at its start must not collapse the neighbour into a gap — they
// are no-ops, in both directions and with or without ripple.
#[test]
fn trimming_at_a_blends_far_edge_spares_the_neighbour() {
    let fake = FakeMpv::new();
    // blend_pair(): C1 0..2 · blend [2,3) · C2 3..5.
    for (k, pos) in [('o', 3.0), ('O', 3.0), ('i', 2.0), ('I', 2.0)] {
        let mut app = App::new(blend_pair(), temp_project_path("blend-far"));
        app.loaded = true;
        app.playhead = pos;
        let before = app.project.timeline.tracks[0].segments.clone();
        handle_key(&mut app, &fake, key(k));
        assert_eq!(
            app.project.timeline.tracks[0].segments, before,
            "{k} at {pos} must not change the track: {}",
            app.message
        );
        assert!(app.message.contains("nothing to trim"), "{}", app.message);
    }
}

// i/I/o/O on a gap all ripple; on a clip the case distinction stays.
#[test]
fn trim_key_ripples_on_gaps() {
    let p = asset_project(); // [C1 0..3][gap 3..5][C2 5..7]
    let a = trim_key(&p, 4.0, false, false, 0).unwrap();
    let b = trim_key(&p, 4.0, false, true, 0).unwrap();
    assert_eq!(a.ops, b.ops, "i and I match on a gap");
    assert!(a.desc.contains("ripple trim gap in"), "{}", a.desc);
    let c = trim_key(&p, 4.0, true, false, 0).unwrap();
    assert_eq!(c.ops, trim_key(&p, 4.0, true, true, 0).unwrap().ops);
    assert!(c.desc.contains("ripple trim gap out"), "{}", c.desc);
    // On a clip, i (gap left behind) and I (ripple) differ.
    assert_ne!(
        trim_key(&p, 1.0, false, false, 0).unwrap().ops,
        trim_key(&p, 1.0, false, true, 0).unwrap().ops
    );
}

// yy grabs whichever segment is under the playhead — gap included.
#[test]
fn yank_grabs_gaps_too() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 4.0; // inside the [3..5) gap
    edit_yank(&mut app, &FakeMpv::new());
    assert!(
        matches!(app.register.as_ref().map(|r| &r.segment), Some(TimelineSegment::Gap(g)) if g.duration == Seconds::from_integer(2))
    );
    app.playhead = 1.0; // inside clip #1
    edit_yank(&mut app, &FakeMpv::new());
    assert!(
        matches!(app.register.as_ref().map(|r| &r.segment), Some(TimelineSegment::Clip(c)) if c.id == 1)
    );
}

// yy/dd/D follow the active track, not the first video track.
#[test]
fn yank_follows_the_active_track() {
    let mut p = asset_project(); // track 0: C1 [0..3), gap, C2 [5..7)
    let a = p.media[0].id;
    let ti = p.timeline.add_track(TrackKind::Video);
    p.timeline.tracks[ti].segments = vec![TimelineSegment::Clip(nle_core::Clip {
        id: 42,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.active_track = ti;
    app.playhead = 1.0; // over clip #42 on track 1 and clip #1 on track 0
    edit_yank(&mut app, &FakeMpv::new());
    assert!(
        matches!(app.register.as_ref().map(|r| &r.segment), Some(TimelineSegment::Clip(c)) if c.id == 42),
        "yanked the active track's clip, not v1's: {:?}",
        app.register
    );
}

// At a segment edge, i/o delete it: non-ripple leaves a gap, capital
// ripples, and a gap always ripples.
#[test]
fn trim_key_deletes_at_segment_edges() {
    let p = asset_project(); // [C1 0..3][gap 3..5][C2 5..7]
    let b = trim_key(&p, 0.0, true, false, 0).unwrap(); // o at C1's start
    assert!(b.desc.contains("delete clip #1"), "{}", b.desc);
    let b = trim_key(&p, 0.0, true, true, 0).unwrap(); // O at C1's start
    assert!(b.desc.contains("ripple delete"), "{}", b.desc);
    let b = trim_key(&p, 3.0, false, false, 0).unwrap(); // i at C1's end
    assert!(b.desc.contains("delete clip #1"), "{}", b.desc);
    let b = trim_key(&p, 3.0, true, false, 0).unwrap(); // o at the gap's start
    assert!(b.desc.contains("delete gap (ripple)"), "{}", b.desc);
    let b = trim_key(&p, 5.0, true, false, 0).unwrap(); // o at C2's start
    assert!(b.desc.contains("delete clip #2"), "{}", b.desc);
}

// After an edit the playhead stays on the media frame it was showing,
// not the same timecode.
#[test]
fn playhead_reanchors_to_the_media_frame() {
    // Ripple head trim: the frame under the playhead moves back, so
    // the playhead follows it.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 1.0; // C1 [0..3) src 1..4 → source 2
    let anchor = edit_anchor(&app.project, app.playhead, 0);
    assert_eq!(
        anchor,
        Some(Anchor::Media(MediaAnchor {
            id: 1,
            source: Seconds::from_integer(2),
            start: 0.0,
            at: 1.0
        }))
    );
    let batch = edit::trim_in_ripple(&app.project, app.playhead).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 0.0, "moved back by the trimmed second");

    // Gap-left head trim: the clip's head moves to the playhead and a
    // gap takes the trimmed material — the frame is unchanged.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 1.0;
    let anchor = edit_anchor(&app.project, app.playhead, 0);
    let batch = edit::trim_in(&app.project, app.playhead).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 1.0, "frame unchanged");

    // Tail trim: material after the playhead is removed — unchanged.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 1.0;
    let anchor = edit_anchor(&app.project, app.playhead, 0);
    let batch = edit::trim_out(&app.project, app.playhead).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 1.0, "frame unchanged");

    // Delete of the clip under the playhead: a plain delete leaves a
    // gap of the same length, so the playhead keeps its offset in it.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 1.0;
    let anchor = edit_anchor(&app.project, app.playhead, 0);
    let batch = edit::delete_under(&app.project, app.playhead).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 1.0, "same position in the resulting gap");

    // Ripple delete between two clips: the playhead lands on the cut
    // the neighbours now share.
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments = vec![
        TimelineSegment::Clip(nle_core::Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        TimelineSegment::Clip(nle_core::Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(4),
            asset_out: Seconds::from_integer(6),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        TimelineSegment::Clip(nle_core::Clip {
            id: 3,
            asset: 1,
            asset_in: Seconds::from_integer(8),
            asset_out: Seconds::from_integer(10),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
    ];
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = 3.0; // inside clip #2 [2..4)
    let anchor = clip_anchor(&app.project, app.playhead).unwrap();
    assert_eq!(anchor.start, 2.0);
    let batch = edit::delete_ripple(&app.project, app.playhead).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, Some(Anchor::Media(anchor)));
    assert_eq!(app.playhead, 2.0, "cut between the neighbours");

    // Deleting a gap always ripples, so the playhead lands on the cut
    // its neighbours now share (its start).
    let mut app = App::new(asset_project(), PathBuf::from("t.json")); // [C1][gap 3..5][C2]
    app.playhead = 4.0; // inside the gap
    let anchor = edit_anchor(&app.project, app.playhead, 0);
    assert_eq!(
        anchor,
        Some(Anchor::Gap(GapAnchor {
            start: 3.0,
            at: 4.0
        }))
    );
    let batch = edit::delete_under(&app.project, app.playhead).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 3.0, "cut between the clips");

    // Over a gap clip_anchor is None, but edit_anchor is not.
    let app = App::new(asset_project(), PathBuf::from("t.json"));
    assert_eq!(clip_anchor(&app.project, 4.0), None);
    assert!(matches!(
        edit_anchor(&app.project, 4.0, 0),
        Some(Anchor::Gap(_))
    ));
}

// Removing a transition (`dd`/`D`) hands its trimmed spans back to the
// neighbours, so the playhead lands on the cut they then share — the
// transition's *end*.
#[test]
fn playhead_lands_on_the_seam_a_deleted_transition_leaves() {
    // [C1 src 1..4][C2 src 0..2]: a 1 s dissolve at the 3.0 cut.
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments = vec![
        TimelineSegment::Clip(nle_core::Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        TimelineSegment::Clip(nle_core::Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
    ];
    let mut app = App::new(p, PathBuf::from("t.json"));
    let batch = edit::add_transition(
        &app.project,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    // [C1' 0..2][transition 2..3][C2' 3..4]; the playhead sits in the blend.
    app.playhead = 2.5;

    let anchor = edit_anchor(&app.project, app.playhead, 0);
    assert_eq!(
        anchor,
        Some(Anchor::Gap(GapAnchor {
            start: 3.0,
            at: 2.5
        })),
        "the transition's end"
    );
    let batch = edit::delete_under(&app.project, app.playhead).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 3.0, "the cut the restore leaves");
    // …and the neighbours are whole again.
    let segs = &app.project.timeline.video_track().unwrap().segments;
    match &segs[..] {
        [TimelineSegment::Clip(a), TimelineSegment::Clip(b)] => {
            assert_eq!((a.asset_out, b.asset_in), (4.into(), 0.into()));
        }
        other => panic!("expected the two clips back, got {other:?}"),
    }
}

// An edit over a gap that doesn't remove it keeps the playhead where it
// is; only a removal leaves the seam (the gap anchor used to slide it to
// the gap's start for every edit).
#[test]
fn an_edit_over_a_gap_keeps_the_playhead() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json")); // [C1][gap 3..5][C2]
    app.playhead = 4.0; // inside the gap
    let anchor = edit_anchor(&app.project, app.playhead, 0);
    // An insert over the gap grows it — a non-removing edit.
    let batch = edit::insert_gap(&app.project, app.playhead, 15).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 4.0, "the playhead stays put");
}

// The anchor maps through a speed clip's rate: the media frame is a
// SOURCE position, and source = timeline × speed.
#[test]
fn playhead_reanchors_on_speed_clips() {
    // A 2× clip reading source [0,4) occupies timeline [0,2). The
    // playhead at 1.0 shows source frame 2.
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments = vec![TimelineSegment::Clip(nle_core::Clip {
        id: 1,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(4),
        speed: Seconds::from_integer(2),
        effects: Vec::new(),
    })];
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = 1.0;
    let anchor = edit_anchor(&app.project, app.playhead, 0);
    assert_eq!(
        anchor,
        Some(Anchor::Media(MediaAnchor {
            id: 1,
            source: Seconds::from_integer(2), // 1.0 × 2, not 1.0
            start: 0.0,
            at: 1.0
        }))
    );
    // Head trim to 0.5 (gap-left): the clip's head moves right onto the
    // playhead and a gap takes the trimmed material. Source 2 now sits
    // at timeline 0.5 (gap) + 0.5 (into the clip) = 1.0 — the same
    // media frame at the same playhead. A 1× mapping records source 1
    // and resolves to 0.5, off the frame.
    let batch = edit::trim_in(&app.project, 0.5).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 1.0, "media frame kept at the scaled offset");

    // A ripple shift under a 0.5× clip: source 1 is 2s into the clip
    // and stays 2s into it after the clip slides left.
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments = vec![
        TimelineSegment::Clip(nle_core::Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(1),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        TimelineSegment::Clip(nle_core::Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::new(1, 2),
            effects: Vec::new(),
        }),
    ];
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = 3.0; // inside clip #2 [1..5): source 0 + (3-1)×0.5 = 1
    let anchor = edit_anchor(&app.project, app.playhead, 0);
    assert_eq!(
        anchor,
        Some(Anchor::Media(MediaAnchor {
            id: 2,
            source: Seconds::from_integer(1),
            start: 1.0,
            at: 3.0
        }))
    );
    let batch = edit::delete_ripple(&app.project, 0.5).unwrap(); // drop #1
    app.history.apply(&mut app.project, batch, 0).unwrap();
    reanchor_playhead(&mut app, anchor);
    assert_eq!(app.playhead, 2.0, "still 2s into clip #2 after the shift");
}

// Undo restores the playhead to the frame the edit was made from.
#[test]
fn undo_restores_the_playhead() {
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    let fps = app.project.fps;
    app.playhead = 1.0;
    let at_frame = frame_of(fps.as_ref(), app.playhead);
    let batch = edit::trim_in_ripple(&app.project, app.playhead).unwrap();
    app.history
        .apply(&mut app.project, batch, at_frame)
        .unwrap();
    // move away, as the user would after editing
    app.playhead = 6.0;
    let outcome = app.history.undo(&mut app.project).unwrap();
    app.playhead = frame_to_secs(app.project.fps.as_ref(), outcome.playhead);
    assert_eq!(app.playhead, 1.0, "playhead returns to where the edit was");
}

// `_` inserts a gap of `count` frames at the playhead.
#[test]
fn insert_gap_key_uses_the_count() {
    // Over the [3..5) gap: 5 frames grows it by 5/30 s.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 4.0;
    let batch = edit::insert_gap(&app.project, app.playhead, 5).unwrap();
    app.history.apply(&mut app.project, batch, 0).unwrap();
    assert!(
        matches!(&app.project.timeline.video_track().unwrap().segments[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(2) + Seconds::new(5, 30))
    );
}

// A pending `d` is cancelled by any non-starter key, so `d`, `:`, Esc, `d`
// must not delete.
#[test]
fn a_pending_delete_is_cancelled_by_a_mode_change() {
    let mut app = App::new(asset_project(), temp_project_path("pending"));
    app.loaded = true;
    app.playhead = 1.0;
    let fake = FakeMpv::new();
    let before = app.project.timeline.video_track().unwrap().segments.len();
    handle_key(&mut app, &fake, key('d')); // arms the sequence
    handle_key(&mut app, &fake, key(':')); // …and the command line cancels it
    handle_key(
        &mut app,
        &fake,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    );
    handle_key(&mut app, &fake, key('d'));
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        before,
        "the stray `d` did not delete"
    );
}

// A yank is a read: it must not move the playhead. Reanchoring a no-op
// would slide it onto a blend's *end* (the anchor's transition rule).
#[test]
fn yanking_a_blend_leaves_the_playhead_put() {
    let mut app = App::new(asset_project(), temp_project_path("yank-blend"));
    app.loaded = true;
    app.playhead = 3.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('X'));
    // The playhead lands mid-blend; this test wants the transition anchor,
    // which needs the blend itself under the playhead.
    app.playhead = 2.0;
    assert_eq!(app.playhead, 2.0, "on the blend's start");
    handle_key(&mut app, &fake, key('y'));
    handle_key(&mut app, &fake, key('y'));
    assert_eq!(app.playhead, 2.0, "the yank left the playhead put");
}
