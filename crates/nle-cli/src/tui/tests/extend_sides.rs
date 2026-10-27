use super::super::actions::clip_edge_under;
use super::super::*;
use super::*;

// Any gap (here a middle one) can be armed at either edge and
// extended. The tail ripples/overwrites what follows; the head
// overwrites the preceding segment.
#[test]
fn arm_and_extend_middle_gap() {
    let p = asset_project(); // [C1 0..3][gap 3..5][C2 5..7]
    // `]` (Left) at the gap's end arms its tail; `[` (Right) at its
    // start arms its head — each edge sits at the cut.
    assert_eq!(
        armed_on_side(&p, 5.0, Side::Left, 0),
        Some((Armed::GapTail { index: 1 }, 5.0))
    );
    assert_eq!(
        armed_on_side(&p, 3.0, Side::Right, 0),
        Some((Armed::GapHead { index: 1 }, 3.0))
    );
    // `e` defaults to the gap's head at a clip|gap cut.
    assert_eq!(
        resolve_armed(&p, 3.0, 0),
        Ok((Armed::GapHead { index: 1 }, 3.0))
    );
    assert_eq!(
        resolve_armed(&p, 5.0, 0),
        Ok((Armed::Clip { id: 2, head: true }, 5.0))
    );

    let plan = plan_for_armed(&p, Armed::GapTail { index: 1 }, 5.0, 0).unwrap();
    assert_eq!((plan.lo, plan.hi), (3.0, None), "unbounded");
    assert!(matches!(plan.preview, ExtendPreviewSpec::Black { origin } if origin == 3.0));
    assert_eq!(
        plan_commit(&p, &Armed::GapTail { index: 1 }, 5.0, 9.0, 0),
        Ok(CommitKind::GapTail)
    );

    // Head plan: unbounded leftward (a gap has no material — extending
    // the head inserts space, even past time 0).
    let head = plan_for_armed(&p, Armed::GapHead { index: 1 }, 3.0, 0).unwrap();
    assert_eq!((head.lo, head.hi), (f64::NEG_INFINITY, Some(5.0)));
    assert_eq!(
        plan_commit(&p, &Armed::GapHead { index: 1 }, 3.0, 1.0, 0),
        Ok(CommitKind::GapHead)
    );
    assert_eq!(
        plan_commit(&p, &Armed::GapHead { index: 1 }, 3.0, 3.0, 0),
        Ok(CommitKind::Noop)
    );
    // Committing at a gap's opposite end removes it (ripple), like a
    // delete everywhere else.
    assert_eq!(
        plan_commit(&p, &Armed::GapTail { index: 1 }, 5.0, 3.0, 0),
        Ok(CommitKind::Delete),
        "gap tail walked to its start"
    );
    assert_eq!(
        plan_commit(&p, &Armed::GapHead { index: 1 }, 3.0, 5.0, 0),
        Ok(CommitKind::Delete),
        "gap head walked to its end"
    );
    // Committing that delete ripples the gap shut.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 3.0;
    let batch = run_commit_action(
        &app,
        &Armed::GapTail { index: 1 },
        CommitKind::Delete,
        false,
    )
    .unwrap();
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    assert_eq!(
        app.project.timeline.end(),
        Seconds::from_integer(5),
        "gap removed, length shrank"
    );
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        2
    );

    // Head grow to 1.0: clip #1's tail is trimmed, the gap grows left.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 1.0;
    let batch = run_commit_action(
        &app,
        &Armed::GapHead { index: 1 },
        CommitKind::GapHead,
        false,
    )
    .unwrap();
    assert!(batch.desc.contains("trimmed clip #1"), "{}", batch.desc);
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    let segs = app.project.timeline.video_track().unwrap().segments.clone();
    assert!(matches!(&segs[0], TimelineSegment::Clip(c) if c.id == 1));
    assert_eq!(
        clip_of(&app.project, 1).duration(),
        Seconds::from_integer(1)
    );
    assert!(matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(4)));
    assert_eq!(
        app.project.timeline.end(),
        Seconds::from_integer(7),
        "length held"
    );

    // Head grow to 0.0 consumes clip #1 entirely.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 0.0;
    let batch = run_commit_action(
        &app,
        &Armed::GapHead { index: 1 },
        CommitKind::GapHead,
        false,
    )
    .unwrap();
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    let segs = app.project.timeline.video_track().unwrap().segments.clone();
    assert!(matches!(&segs[0], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(5)));
    assert_eq!(app.project.timeline.end(), Seconds::from_integer(7));

    // E (ripple): grow the gap tail to 7 — clip #2 slides right.
    let armed = Armed::GapTail { index: 1 };
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 7.0;
    let batch = run_commit_action(&app, &armed, CommitKind::GapTail, true).unwrap();
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    let segs = app.project.timeline.video_track().unwrap().segments.clone();
    assert!(matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(4)));
    assert_eq!(app.project.timeline.end(), Seconds::from_integer(9));
    assert_eq!(
        clip_of(&app.project, 2).duration(),
        Seconds::from_integer(2)
    );

    // e (replace): grow to 7 overwrites clip #2 entirely; length held.
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 7.0;
    let batch = run_commit_action(&app, &armed, CommitKind::GapTail, false).unwrap();
    assert!(batch.desc.contains("absorbed"), "{}", batch.desc);
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    let segs = app.project.timeline.video_track().unwrap().segments.clone();
    assert!(matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(4)));
    assert_eq!(
        app.project.timeline.end(),
        Seconds::from_integer(7),
        "length held"
    );
    assert_eq!(segs.len(), 2, "clip #2 replaced by gap");

    // Shrink it to 4: clip #2 slides left (either flag).
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 4.0;
    let batch = run_commit_action(&app, &armed, CommitKind::GapTail, false).unwrap();
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    let segs = app.project.timeline.video_track().unwrap().segments.clone();
    assert!(matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(1)));
    assert_eq!(app.project.timeline.end(), Seconds::from_integer(6));
    assert_eq!(
        clip_of(&app.project, 2).duration(),
        Seconds::from_integer(2)
    );
}

// Extending a tail to the adjacent gap's far edge (exactly where the
// next clip starts — where `j` lands, and where 1s `l` steps often
// land) must absorb the now-fully-spanned gap, not call trim_in at the
// boundary and fail with "nothing to trim".
#[test]
fn commit_tail_to_gap_far_edge_absorbs() {
    let p = asset_project(); // [C1 [0..3) src1..4] [gap 2 @3..5] [C2 [5..7)]
    // `e` now defaults to the gap on the right, so arm the clip
    // explicitly with `]` (Left).
    let (tail1, edge) = armed_on_side(&p, 3.0, Side::Left, 0).unwrap();
    assert_eq!(tail1, Armed::Clip { id: 1, head: false });
    assert_eq!(
        plan_commit(&p, &tail1, edge, 5.0, 0),
        Ok(CommitKind::AbsorbTail),
        "the gap's far edge is not 'inside' the gap"
    );
    let mut app = App::new(asset_project(), PathBuf::from("t.json"));
    app.playhead = 5.0;
    let batch = run_commit_action(&app, &tail1, CommitKind::AbsorbTail, false).unwrap();
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    assert_eq!(clip_of(&app.project, 1).asset_out, Seconds::from_integer(6));
    assert_eq!(app.project.timeline.end(), Seconds::from_integer(7));
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        2,
        "gap absorbed"
    );
}

// Symmetric: a head reaching the adjacent gap's near edge absorbs it.
#[test]
fn commit_head_to_gap_near_edge_absorbs() {
    // [C1 [0..3)] [gap 1 @3..4] [C2 [4..6) src1..3]: one second of head
    // room, enough to reach the gap's near edge at 3.0.
    let mut p = asset_project();
    {
        let segs = &mut p.timeline.video_track_mut().unwrap().segments;
        segs[1] = TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(1),
        });
        segs[2] = TimelineSegment::Clip(nle_core::Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(3),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        });
    }
    let (head2, edge) = resolve_armed(&p, 4.0, 0).unwrap();
    assert_eq!(head2, Armed::Clip { id: 2, head: true });
    assert_eq!(
        plan_commit(&p, &head2, edge, 3.0, 0),
        Ok(CommitKind::AbsorbHead)
    );
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = 3.0;
    let batch = run_commit_action(&app, &head2, CommitKind::AbsorbHead, false).unwrap();
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    assert_eq!(clip_of(&app.project, 2).asset_in, Seconds::from_integer(0));
    assert_eq!(app.project.timeline.end(), Seconds::from_integer(6));
    assert_eq!(
        app.project.timeline.video_track().unwrap().segments.len(),
        2
    );
}

// `[` / `]` pick the clip on the left (ends at the cut) or right
// (starts at it). A gap side has no clip.
#[test]
fn armed_on_side_picks_clip_by_side() {
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments.remove(1); // C1 | C2 cut
    assert_eq!(
        armed_on_side(&p, 3.0, Side::Left, 0),
        Some((Armed::Clip { id: 1, head: false }, 3.0)),
        "[ is the clip ending at the cut"
    );
    assert_eq!(
        armed_on_side(&p, 3.0, Side::Right, 0),
        Some((Armed::Clip { id: 2, head: true }, 3.0)),
        "] is the clip starting at the cut"
    );
    // Against a gap, the gap is armed at its tail from either side.
    let q = asset_project(); // [C1] [gap] [C2]
    assert_eq!(
        armed_on_side(&q, 3.0, Side::Left, 0),
        Some((Armed::Clip { id: 1, head: false }, 3.0))
    );
    assert_eq!(
        armed_on_side(&q, 3.0, Side::Right, 0),
        Some((Armed::GapHead { index: 1 }, 3.0)),
        "gap on the right arms the gap head"
    );
    assert_eq!(
        armed_on_side(&q, 5.0, Side::Right, 0),
        Some((Armed::Clip { id: 2, head: true }, 5.0))
    );
    assert_eq!(
        armed_on_side(&q, 5.0, Side::Left, 0),
        Some((Armed::GapTail { index: 1 }, 5.0)),
        "gap on the left arms the gap"
    );
    // Nothing at a non-boundary.
    assert_eq!(armed_on_side(&q, 1.0, Side::Left, 0), None);
}

// A blend's far end is the neighbour's real edge: `]` at its end arms the
// preceding clip's tail there (not "no clip ends here"), and `[` at its
// start arms the following clip's head there.
#[test]
fn armed_on_side_reads_a_blends_far_edge() {
    let p = blend_pair(); // C1 0..2 · blend 2..3 · C2 3..5
    assert_eq!(
        armed_on_side(&p, 3.0, Side::Left, 0),
        Some((Armed::Clip { id: 1, head: false }, 3.0)),
        "`]` at the blend's end is C1's real tail"
    );
    assert_eq!(
        armed_on_side(&p, 2.0, Side::Right, 0),
        Some((Armed::Clip { id: 2, head: true }, 2.0)),
        "`[` at the blend's start is C2's real head"
    );
    // The near edges still name the adjacent clip; entering the mode
    // hops to the far end (`blend_far_end`).
    assert_eq!(
        armed_on_side(&p, 2.0, Side::Left, 0),
        Some((Armed::Clip { id: 1, head: false }, 2.0))
    );
    assert_eq!(
        armed_on_side(&p, 3.0, Side::Right, 0),
        Some((Armed::Clip { id: 2, head: true }, 3.0))
    );
}

// Entering from either edge of a blend arms the same real edge, and the
// preview maps the material there: a tail at the blend's end reads
// `a.src_out`, a head at its start `b.src_in` (it used to be one blend
// length off, so the monitor showed the wrong frame).
#[test]
fn brackets_from_either_blend_edge_arm_the_same_neighbour() {
    let fake = FakeMpv::new();
    for (pos, k, armed, edge, off) in [
        (2.0, ']', Armed::Clip { id: 1, head: false }, 3.0, 0.0),
        (3.0, ']', Armed::Clip { id: 1, head: false }, 3.0, 0.0),
        (2.0, '[', Armed::Clip { id: 2, head: true }, 2.0, 2.0),
        (3.0, '[', Armed::Clip { id: 2, head: true }, 2.0, 2.0),
    ] {
        let mut app = App::new(blend_pair(), temp_project_path("blend-brackets"));
        app.loaded = true;
        app.playhead = pos;
        handle_key(&mut app, &fake, key(k));
        match &app.mode {
            Mode::Extend(st) => {
                assert_eq!((st.armed, st.edge_pos), (armed, edge), "{k} at {pos}");
                assert_eq!(st.preview_off, off, "{k} at {pos}: preview source");
            }
            _ => panic!("{k} at {pos} did not enter: {}", app.message),
        }
    }
}

// Switching sides inside extend mode applies the same hop: from the
// blend's end, `[` arms the following clip's head at the blend's start.
#[test]
fn switching_sides_at_a_blend_end_lands_on_the_real_edge() {
    let fake = FakeMpv::new();
    let mut app = App::new(blend_pair(), temp_project_path("blend-switch"));
    app.loaded = true;
    app.playhead = 2.0;
    handle_key(&mut app, &fake, key(']')); // C1 tail at the blend's end
    assert!(matches!(
        &app.mode,
        Mode::Extend(st) if st.armed == Armed::Clip { id: 1, head: false }
    ));
    handle_key(&mut app, &fake, key('[')); // C2 head at the blend's start
    assert!(
        matches!(
            &app.mode,
            Mode::Extend(st)
                if st.armed == Armed::Clip { id: 2, head: true } && st.edge_pos == 2.0
        ),
        "{:?} — {}",
        app.mode,
        app.message
    );
}

// `<` / `>` (and `,` / `.`): head/tail of the clip under the playhead.
#[test]
fn clip_edge_under_finds_head_and_tail() {
    let p = asset_project(); // C1 [0..3), gap [3..5), C2 [5..7)
    assert_eq!(clip_edge_under(&p, 1.0, true, 0), Some((1, 0.0)));
    assert_eq!(clip_edge_under(&p, 1.0, false, 0), Some((1, 3.0)));
    assert_eq!(clip_edge_under(&p, 6.0, true, 0), Some((2, 5.0)));
    assert_eq!(clip_edge_under(&p, 6.0, false, 0), Some((2, 7.0)));
    assert_eq!(clip_edge_under(&p, 4.0, true, 0), None, "in a gap");
}

// Extend mode: flipping the armed edge of the SAME clip re-plans its
// range against that end (no clip change).
#[test]
fn flip_extend_edge_replans_same_clip() {
    let p = asset_project(); // C2 [5..7) src 0..2, asset 10s
    let (s, e) = clip_span(&p, 2, 0).unwrap();
    assert_eq!((s, e), (5.0, 7.0));
    // head -> tail: range becomes [start, end + tail room]
    let tail = plan_for_armed(&p, Armed::Clip { id: 2, head: false }, e, 0).unwrap();
    assert_eq!(tail.lo, 5.0);
    assert_eq!(tail.hi, Some(15.0)); // 7 + (10 - 2)
    // tail -> head: range becomes [start - head room, end]
    let head = plan_for_armed(&p, Armed::Clip { id: 2, head: true }, s, 0).unwrap();
    assert_eq!(head.lo, 5.0); // src in is 0: no head room
    assert_eq!(head.hi, Some(7.0));
}

// `<` / `>` flip either end of a clip or a gap.
#[test]
fn flip_target_covers_clips_and_gaps() {
    let p = asset_project(); // [C1 0..3][gap 3..5][C2 5..7]
    assert_eq!(
        flip_target(&p, Armed::Clip { id: 1, head: false }, true, 0),
        Some((Armed::Clip { id: 1, head: true }, 0.0))
    );
    assert_eq!(
        flip_target(&p, Armed::Clip { id: 1, head: true }, false, 0),
        Some((Armed::Clip { id: 1, head: false }, 3.0))
    );
    assert_eq!(
        flip_target(&p, Armed::GapHead { index: 1 }, false, 0),
        Some((Armed::GapTail { index: 1 }, 5.0))
    );
    assert_eq!(
        flip_target(&p, Armed::GapTail { index: 1 }, true, 0),
        Some((Armed::GapHead { index: 1 }, 3.0))
    );
    // Leading gap head flips to its tail.
    let mut s = asset_project();
    s.timeline.video_track_mut().unwrap().segments.insert(
        0,
        TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(2),
        }),
    );
    assert_eq!(
        flip_target(&s, Armed::GapHead { index: 0 }, false, 0),
        Some((Armed::GapTail { index: 0 }, 2.0))
    );
}

#[test]
fn commit_at_opposite_end_deletes_armed_clip() {
    // [C1 [0..3)] [gap [3..5)] [C2 [5..7)] [C3 [7..9)]: arm the head of
    // #2 at 5.0, walk to its far end (7.0), commit. #2 is consumed, so
    // #2 goes and the follower #3 survives — not the other way round.
    let mut p = asset_project();
    p.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(TimelineSegment::Clip(nle_core::Clip {
            id: 3,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }));
    let mut app = App::new(p, PathBuf::from("t.json"));
    let head2 = Armed::Clip { id: 2, head: true };
    assert_eq!(
        plan_commit(&app.project, &head2, 5.0, 7.0, 0),
        Ok(CommitKind::Delete)
    );
    app.playhead = 7.0;
    let batch = run_commit_action(&app, &head2, CommitKind::Delete, false).unwrap();
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    let ids: Vec<u32> = app
        .project
        .timeline
        .video_track()
        .unwrap()
        .segments
        .iter()
        .filter_map(|s| match s {
            TimelineSegment::Clip(c) => Some(c.id),
            TimelineSegment::Gap(_) | TimelineSegment::Transition(_) => None,
        })
        .collect();
    assert_eq!(ids, vec![1, 3], "armed #2 removed, follower #3 kept");
    assert_eq!(app.project.timeline.end(), Seconds::from_integer(9));

    // Ripple flag closes the removed clip instead of leaving a gap.
    let mut app2 = App::new(asset_project(), PathBuf::from("t.json"));
    app2.playhead = 7.0;
    let batch = run_commit_action(&app2, &head2, CommitKind::Delete, true).unwrap();
    let mut h2 = History::default();
    h2.apply(&mut app2.project, batch, 0).unwrap();
    let end = app2.project.timeline.end();
    assert!(end < Seconds::from_integer(7), "shrank: {end}");
}

// Entering extend mode at a clip|transition cut hops to the blend's far
// end — the transition is part of the clip.
#[test]
fn extend_mode_hops_over_a_blend() {
    let mut p = asset_project();
    p.video = Some(nle_core::VideoSettings {
        width: 320,
        height: 240,
    });
    let mut app = App::new(p, temp_project_path("extend-blend"));
    app.loaded = true;
    app.playhead = 3.0;
    let fake = FakeMpv::new();
    handle_key(&mut app, &fake, key('X')); // blend [2, 3), playhead 2.5
    // This test is about entering at the blend's edge; place it there.
    app.playhead = 2.0;
    assert_eq!(app.playhead, 2.0, "on the blend's start");
    handle_key(&mut app, &fake, key('e'));
    assert!(
        matches!(&app.mode, Mode::Extend(st) if st.edge_pos == 3.0),
        "expected extend mode at the blend's far end; message = {:?}",
        app.message
    );
    assert!(app.message.contains("blend"), "{}", app.message);
}
