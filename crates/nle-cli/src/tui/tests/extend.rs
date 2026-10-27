use super::super::*;
use super::*;

#[test]
fn resolve_armed_matrix() {
    let p = asset_project();
    // Clip starts win (head-of-right at shared cuts).
    assert_eq!(
        resolve_armed(&p, 0.0, 0),
        Ok((Armed::Clip { id: 1, head: true }, 0.0))
    );
    assert_eq!(
        resolve_armed(&p, 5.0, 0),
        Ok((Armed::Clip { id: 2, head: true }, 5.0))
    );
    // Clip ends (gap starts resolve to the clip tail).
    // Clip end with a gap on the right: `e` arms the gap's head (the
    // edge at the cut), so it can be grown left over the clip.
    assert_eq!(
        resolve_armed(&p, 3.0, 0),
        Ok((Armed::GapHead { index: 1 }, 3.0))
    );
    assert_eq!(
        resolve_armed(&p, 7.0, 0),
        Ok((Armed::Clip { id: 2, head: false }, 7.0))
    );
    // Mid-segment positions refuse.
    assert!(resolve_armed(&p, 1.5, 0).is_err());
    assert!(resolve_armed(&p, 4.0, 0).is_err());
    // Shared clip|clip cut arms the right head.
    let mut q = asset_project();
    q.timeline.video_track_mut().unwrap().segments.remove(1);
    assert_eq!(
        resolve_armed(&q, 3.0, 0),
        Ok((Armed::Clip { id: 2, head: true }, 3.0))
    );
    // Virtual edges with gaps.
    let mut r = asset_project();
    r.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(3),
        }));
    assert_eq!(
        resolve_armed(&r, 10.0, 0),
        Ok((Armed::GapTail { index: 3 }, 10.0))
    );
    let mut s = asset_project();
    s.timeline.video_track_mut().unwrap().segments.insert(
        0,
        TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(2),
        }),
    );
    assert_eq!(
        resolve_armed(&s, 0.0, 0),
        Ok((Armed::GapHead { index: 0 }, 0.0))
    );
    // Empty timeline refuses.
    assert!(resolve_armed(&Project::new(), 0.0, 0).is_err());
}

#[test]
fn plan_extend_clip_bounds() {
    let p = asset_project();
    // Tail of #1 at 3.0: material-wide — lo is the far edge, hi is
    // the asset end mapped (gap no longer binds). Resolve is no longer
    // via plan_extend (e defaults to the gap on the right there).
    let plan = plan_for_armed(&p, Armed::Clip { id: 1, head: false }, 3.0, 0).unwrap();
    assert_eq!(plan.armed, Armed::Clip { id: 1, head: false });
    assert_eq!(plan.edge_pos, 3.0);
    assert_eq!((plan.lo, plan.hi), (0.0, Some(9.0)));
    assert_eq!(plan.mat, Some((-1.0, 9.0)));
    match plan.preview {
        ExtendPreviewSpec::Clip {
            src_start,
            length,
            origin,
            ..
        } => {
            assert_eq!(src_start, Seconds::from_integer(0));
            assert_eq!(length, Seconds::from_integer(10));
            assert_eq!(origin, -1.0);
        }
        _ => panic!("expected clip preview"),
    }
    // Head of #2 at 5.0: ain 0 means no left room.
    let plan = plan_extend(&p, 5.0, true, 0).unwrap();
    assert_eq!((plan.lo, plan.hi), (5.0, Some(7.0)));
    // Head of #1 at 0.0: the head material reaches past the timeline
    // start, so the range dips negative (prepend).
    let plan = plan_extend(&p, 0.0, false, 0).unwrap();
    assert_eq!((plan.lo, plan.hi), (-1.0, Some(3.0)));
    // Its preview stops at aout (4), not the whole asset, so mpv can't
    // play past the head's edge.
    match plan.preview {
        ExtendPreviewSpec::Clip { length, origin, .. } => {
            assert_eq!(length, Seconds::from_integer(4), "trimmed to aout");
            assert_eq!(origin, -1.0);
        }
        _ => panic!("expected clip preview"),
    }
    // Unknown asset duration: no extension room, trim/delete only,
    // no material strip, span-mapped preview.
    let mut q = asset_project();
    q.media[0].probe.format.duration = None;
    let plan = plan_for_armed(&q, Armed::Clip { id: 1, head: false }, 3.0, 0).unwrap();
    assert_eq!((plan.lo, plan.hi), (0.0, Some(3.0)));
    assert_eq!(plan.mat, None);
    match plan.preview {
        ExtendPreviewSpec::Clip {
            src_start,
            length,
            origin,
            ..
        } => {
            assert_eq!(src_start, Seconds::from_integer(1));
            assert_eq!(length, Seconds::from_integer(3));
            assert_eq!(origin, 0.0);
        }
        _ => panic!("expected clip preview"),
    }
}

// A timeline-start segment can extend left of time 0 by prepending its
// head material; the timeline grows at the front.
#[test]
fn first_clip_head_extends_past_timeline_start() {
    let p = asset_project(); // C1 [0..3) src 1..4 of a 10s asset
    let head1 = Armed::Clip { id: 1, head: true };
    let plan = plan_for_armed(&p, head1, 0.0, 0).unwrap();
    assert_eq!(
        (plan.lo, plan.hi),
        (-1.0, Some(3.0)),
        "one second of head room below zero"
    );
    assert_eq!(
        plan_commit(&p, &head1, 0.0, -1.0, 0),
        Ok(CommitKind::AbsorbHead)
    );

    for ripple in [false, true] {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = -1.0;
        let batch = run_commit_action(&app, &head1, CommitKind::AbsorbHead, ripple).unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        let c1 = clip_of(&app.project, 1);
        assert_eq!(c1.asset_in, Seconds::from_integer(0), "head material used");
        assert_eq!(c1.duration(), Seconds::from_integer(4));
        assert_eq!(
            app.project.timeline.end(),
            Seconds::from_integer(8),
            "grew at the front"
        );
    }
}

// Rendering must survive a negative playhead (head extend past 0).
#[test]
fn extend_head_below_zero_renders() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let p = asset_project();
    let plan = plan_for_armed(&p, Armed::Clip { id: 1, head: true }, 0.0, 0).unwrap();
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = -1.0;
    app.mode = Mode::Extend(ExtendState {
        track: 0,
        armed: plan.armed,
        edge_pos: plan.edge_pos,
        ripple: false,
        lo: plan.lo,
        hi: plan.hi,
        mat: plan.mat,
        preview_off: -1.0, // edge 0 at source 1
    });
    let mut term = Terminal::new(TestBackend::new(60, 4)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 60,
                height: 4,
            },
            &app,
        )
    })
    .unwrap();
}

// A gap has no material, so its head extends without bound — including
// past time 0, where it inserts empty space.
#[test]
fn gap_head_extends_past_zero() {
    // Insert a leading gap: [gap 2 @0..2][C1 2..5][gap 2 @5..7][C2 7..9]
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments.insert(
        0,
        TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(2),
        }),
    );
    let head0 = Armed::GapHead { index: 0 };
    let plan = plan_for_armed(&p, head0, 0.0, 0).unwrap();
    assert_eq!(plan.lo, f64::NEG_INFINITY, "unbounded leftward");
    assert_eq!(plan.hi, Some(2.0));
    assert_eq!(
        plan_commit(&p, &head0, 0.0, -1.0, 0),
        Ok(CommitKind::GapHead)
    );

    for ripple in [false, true] {
        let mut app = App::new(p.clone(), PathBuf::from("t.json"));
        app.playhead = -1.0;
        let batch = run_commit_action(&app, &head0, CommitKind::GapHead, ripple).unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        let segs = app.project.timeline.video_track().unwrap().segments.clone();
        assert!(
            matches!(&segs[0], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(3))
        );
        assert_eq!(
            app.project.timeline.end(),
            Seconds::from_integer(10),
            "grew at the front"
        );
    }
}

// Rendering must survive a gap head armed below 0 (unbounded lo).
#[test]
fn gap_head_below_zero_renders() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut p = asset_project();
    p.timeline.video_track_mut().unwrap().segments.insert(
        0,
        TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(2),
        }),
    );
    let plan = plan_for_armed(&p, Armed::GapHead { index: 0 }, 0.0, 0).unwrap();
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = -1.0;
    app.mode = Mode::Extend(ExtendState {
        track: 0,
        armed: plan.armed,
        edge_pos: plan.edge_pos,
        ripple: false,
        lo: plan.lo,
        hi: plan.hi,
        mat: plan.mat,
        preview_off: 0.0,
    });
    let mut term = Terminal::new(TestBackend::new(60, 4)).unwrap();
    term.draw(|f| {
        render_timeline(
            f,
            Rect {
                x: 0,
                y: 0,
                width: 60,
                height: 4,
            },
            &app,
        )
    })
    .unwrap();
}

#[test]
fn plan_extend_virtual_edges() {
    let mut p = asset_project();
    p.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(3),
        }));
    let plan = plan_extend(&p, 10.0, false, 0).unwrap();
    assert_eq!(plan.armed, Armed::GapTail { index: 3 });
    assert_eq!(plan.lo, 7.0);
    assert_eq!(plan.hi, None);
    assert_eq!(plan.mat, None);
    assert!(matches!(plan.preview, ExtendPreviewSpec::Black { origin } if origin == 7.0));
    // Leading gap: unbounded head range, black preview, no material.
    let mut s = asset_project();
    s.timeline.video_track_mut().unwrap().segments.insert(
        0,
        TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(2),
        }),
    );
    let plan = plan_extend(&s, 0.0, false, 0).unwrap();
    assert_eq!(plan.armed, Armed::GapHead { index: 0 });
    assert_eq!((plan.lo, plan.hi), (f64::NEG_INFINITY, Some(2.0)));
    assert_eq!(plan.mat, None);
}

#[test]
fn clamp_extend_holds_both_modes() {
    let st = ExtendState {
        track: 0,
        armed: Armed::GapTail { index: 1 },
        edge_pos: 7.0,
        ripple: false,
        lo: 7.0,
        hi: Some(9.0),
        mat: None,
        preview_off: 7.0,
    };
    assert_eq!(clamp_extend(&st, 5.0), 7.0);
    assert_eq!(clamp_extend(&st, 8.0), 8.0);
    assert_eq!(clamp_extend(&st, 99.0), 9.0);
    let open = ExtendState {
        track: 0,
        hi: None,
        ..st.clone()
    };
    assert_eq!(clamp_extend(&open, 1e9), 1e9);
}

// Seek-clamping for a head: never past the segment's end (nor before
// the material/timeline start).
#[test]
fn extend_head_nav_clamps_at_segment_end() {
    let fps = Seconds::new(30, 1);
    let st = ExtendState {
        track: 0,
        armed: Armed::Clip { id: 1, head: true },
        edge_pos: 0.0,
        ripple: false,
        lo: -1.0,
        hi: Some(3.0),
        mat: None,
        preview_off: -1.0,
    };
    assert_eq!(
        snap_extend_target(Some(&fps), &st, 99.0),
        3.0,
        "blocked at the end"
    );
    assert_eq!(
        snap_extend_target(Some(&fps), &st, -99.0),
        -1.0,
        "blocked at the start"
    );
    assert_eq!(snap_extend_target(Some(&fps), &st, 1.0), 1.0);
}

// Frame-stepping an armed head must reach negative positions when the
// range allows it (a clip at the timeline start prepends head material);
// the old code floored the index at 0.
#[test]
fn extend_step_reaches_a_negative_head_range() {
    let fps = Seconds::new(30, 1);
    assert_eq!(extend_step_index(-1.0, Some(3.0), fps, 0.0, -1, 1), -1);
    assert_eq!(
        extend_step_index(-1.0, Some(3.0), fps, -1.0, -1, 1),
        -30,
        "clamps at lo"
    );
    assert_eq!(
        extend_step_index(f64::NEG_INFINITY, Some(2.0), fps, 0.0, -1, 1),
        -1,
        "unbounded gap head"
    );
    assert_eq!(
        extend_step_index(0.0, Some(2.0), fps, 2.0, 1, 5),
        60,
        "clamps at hi"
    );
}

// Extending a tail to the asset's end leaves the material bound
// off-grid (blue.mp4's 6.006s). seek_to_mode used to park the playhead
// there unsnapped while the commit snapped the cut to a frame, so the
// playhead read red and `]` reported no clip ending there.
#[test]
fn extend_to_material_end_lands_on_grid() {
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    let probe = RawProbe {
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
    };
    let (a, _) = p.add_asset("/media/blue.mp4".into(), probe);
    p.timeline.video_track_mut().unwrap().segments = vec![
        TimelineSegment::Clip(nle_core::Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(3),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        TimelineSegment::Clip(nle_core::Clip {
            id: 2,
            asset: a,
            asset_in: Seconds::new(1, 2),
            asset_out: Seconds::new(7, 2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
    ];
    let (armed, edge) = resolve_armed(&p, 6.0, 0).unwrap();
    assert_eq!(armed, Armed::Clip { id: 2, head: false });
    let plan = plan_extend(&p, 6.0, false, 0).unwrap();
    let hi = plan.hi.unwrap();
    assert!((hi - 8.506).abs() < 1e-6, "material end off-grid: {hi}");
    let st = ExtendState {
        track: 0,
        armed: plan.armed,
        edge_pos: plan.edge_pos,
        ripple: false,
        lo: plan.lo,
        hi: plan.hi,
        mat: plan.mat,
        preview_off: edge - 3.5,
    };
    // Navigating past the end clamps to the material bound and snaps
    // to the frame grid, staying within the reachable range.
    let fps = p.fps;
    let target = snap_extend_target(fps.as_ref(), &st, hi + 1.0);
    assert_eq!(target, 8.5);
    assert!(target <= hi + 1e-9);

    // Commit there: the new cut is exactly at the snapped playhead.
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = target;
    let kind = plan_commit(&app.project, &armed, edge, target, 0).unwrap();
    let batch = run_commit_action(&app, &armed, kind, false).unwrap();
    let mut h = History::default();
    h.apply(&mut app.project, batch, 0).unwrap();
    assert!(
        on_boundary(&app.project, target, 0),
        "cut recognised on-grid"
    );
    assert_eq!(
        armed_on_side(&app.project, target, Side::Left, 0),
        Some((Armed::Clip { id: 2, head: false }, target)),
        "] finds the clip ending at the new cut"
    );
    assert!(
        !on_boundary(&app.project, hi, 0),
        "the raw off-grid bound is not a cut"
    );
}

#[test]
fn plan_commit_routing_table() {
    let p = asset_project(); // C1 [0..3), gap [3..5), C2 [5..7)
    let tail1 = Armed::Clip { id: 1, head: false };
    assert_eq!(
        plan_commit(&p, &tail1, 3.0, 4.0, 0),
        Ok(CommitKind::GrowTailIntoGap)
    );
    assert_eq!(
        plan_commit(&p, &tail1, 3.0, 1.0, 0),
        Ok(CommitKind::TrimTail)
    );
    assert_eq!(plan_commit(&p, &tail1, 3.0, 0.0, 0), Ok(CommitKind::Delete));
    assert_eq!(plan_commit(&p, &tail1, 3.0, 3.0, 0), Ok(CommitKind::Noop));
    let head2 = Armed::Clip { id: 2, head: true };
    assert_eq!(
        plan_commit(&p, &head2, 5.0, 6.0, 0),
        Ok(CommitKind::TrimHead)
    );
    assert_eq!(
        plan_commit(&p, &head2, 5.0, 4.0, 0),
        Ok(CommitKind::GrowHeadIntoGap)
    );
    assert_eq!(plan_commit(&p, &head2, 5.0, 7.0, 0), Ok(CommitKind::Delete));
    assert_eq!(plan_commit(&p, &head2, 5.0, 5.0, 0), Ok(CommitKind::Noop));
    // Routing is flag-independent (ripple splits at run time).
    assert_eq!(
        plan_commit(&p, &tail1, 3.0, 4.0, 0),
        plan_commit(&p, &tail1, 3.0, 4.0, 0)
    );
    // Past-end tail with no following gap.
    let mut q = asset_project();
    q.timeline.video_track_mut().unwrap().segments.truncate(1);
    let tail = Armed::Clip { id: 1, head: false };
    assert_eq!(
        plan_commit(&q, &tail, 3.0, 4.0, 0),
        Ok(CommitKind::GrowTailPastEnd)
    );
    // Beyond the adjacent gap: absorb (both flags route here; run
    // splits e-replace vs E-push).
    assert_eq!(
        plan_commit(&p, &tail1, 3.0, 6.0, 0),
        Ok(CommitKind::AbsorbTail)
    );
    assert_eq!(
        plan_commit(&p, &head2, 5.0, 1.0, 0),
        Ok(CommitKind::AbsorbHead)
    );
    // Directly into the next clip: absorb as well.
    let mut s = asset_project();
    s.timeline.video_track_mut().unwrap().segments.remove(1);
    let tail = Armed::Clip { id: 1, head: false };
    assert_eq!(
        plan_commit(&s, &tail, 3.0, 4.0, 0),
        Ok(CommitKind::AbsorbTail)
    );
    // Virtual edges.
    let mut r = asset_project();
    r.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(3),
        }));
    assert_eq!(
        plan_commit(&r, &Armed::GapTail { index: 3 }, 10.0, 8.0, 0),
        Ok(CommitKind::GapTail)
    );
    assert_eq!(
        plan_commit(&r, &Armed::GapTail { index: 3 }, 10.0, 12.0, 0),
        Ok(CommitKind::GapTail)
    );
    assert_eq!(
        plan_commit(&r, &Armed::GapTail { index: 3 }, 10.0, 10.0, 0),
        Ok(CommitKind::Noop)
    );
}

#[test]
fn run_commit_maps_kinds_to_ops() {
    let p = asset_project();
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = 4.0; // inside the [3..5) gap
    let tail1 = Armed::Clip { id: 1, head: false };
    let b = run_commit_action(&app, &tail1, CommitKind::GrowTailIntoGap, false).unwrap();
    assert!(b.desc.contains("trim gap in"), "{}", b.desc);
    let b = run_commit_action(&app, &tail1, CommitKind::GrowTailIntoGap, true).unwrap();
    assert!(b.desc.contains("extend tail"), "{}", b.desc);
    // Opposite-end commit deletes the ARMED clip (tail1 = #1), not the
    // segment the playhead's boundary resolves to.
    let b = run_commit_action(&app, &tail1, CommitKind::Delete, true).unwrap();
    assert!(b.desc.contains("ripple delete"), "{}", b.desc);
    let b = run_commit_action(&app, &tail1, CommitKind::Delete, false).unwrap();
    assert!(b.desc.starts_with("delete clip #1"), "{}", b.desc);
    // Absorb splits by flag: e replaces covered content, E pushes it.
    app.playhead = 6.0; // past the gap, inside clip #2
    let b = run_commit_action(&app, &tail1, CommitKind::AbsorbTail, false).unwrap();
    assert!(b.desc.contains("absorbed"), "{}", b.desc);
    let b = run_commit_action(&app, &tail1, CommitKind::AbsorbTail, true).unwrap();
    assert!(b.desc.contains("extend tail"), "{}", b.desc);
}

// An extend commit records its undo position as where the mode was
// entered (the armed edge), not the playhead it moved to.
#[test]
fn extend_commit_undo_returns_to_entry() {
    let p = asset_project(); // C1 tail armed at 3.0
    let mut app = App::new(p, PathBuf::from("t.json"));
    app.playhead = 3.0; // entered here
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
    // Commit with the playhead moved to 6.0, via the run_edit path.
    app.playhead = 6.0;
    let batch = run_commit_action(
        &app,
        &Armed::Clip { id: 1, head: false },
        CommitKind::AbsorbTail,
        false,
    )
    .unwrap();
    // Mirror run_edit's attribution: extend uses the entry edge.
    let from = match &app.mode {
        Mode::Extend(st) => st.edge_pos,
        _ => app.playhead,
    };
    assert_eq!(from, 3.0);
    let frame = frame_of(app.project.fps.as_ref(), from);
    app.history.apply(&mut app.project, batch, frame).unwrap();
    // Move away, then undo.
    app.playhead = 1.0;
    let outcome = app.history.undo(&mut app.project).unwrap();
    app.playhead = frame_to_secs(app.project.fps.as_ref(), outcome.playhead);
    assert_eq!(app.playhead, 3.0, "back to where extend mode was entered");
}

// After committing a tail extend the playhead lands on the new clip
// end (the edge we extended to), for both ripple and absorb.
#[test]
fn tail_extend_commit_lands_on_the_new_end() {
    for (kind, ripple) in [
        (CommitKind::AbsorbTail, false),
        (CommitKind::AbsorbTail, true),
    ] {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 6.0; // extend C1's tail [0..3) out to 6.0
        app.mode = Mode::Extend(ExtendState {
            track: 0,
            armed: Armed::Clip { id: 1, head: false },
            edge_pos: 3.0,
            ripple,
            lo: 0.0,
            hi: Some(9.0),
            mat: None,
            preview_off: -1.0,
        });
        let target = app.playhead;
        let batch =
            run_commit_action(&app, &Armed::Clip { id: 1, head: false }, kind, ripple).unwrap();
        // Mirror run_edit's extend attribution (the armed edge).
        let frame = frame_of(app.project.fps.as_ref(), 3.0);
        app.history.apply(&mut app.project, batch, frame).unwrap();
        let end = {
            let track = app.project.timeline.video_track().unwrap();
            let si = track.clip_index(1).unwrap();
            secs(&(track.start_of(si) + track.segments[si].duration()))
        };
        assert_eq!(
            end, target,
            "clip end == committed target (ripple={ripple})"
        );
    }
}

// The status projects the post-commit timeline length.
#[test]
fn projected_total_reflects_pending_commit() {
    let make = |ripple: bool, armed: Armed, edge: f64, ph: f64| {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = ph;
        let st = ExtendState {
            track: 0,
            armed,
            edge_pos: edge,
            ripple,
            lo: 0.0,
            hi: None,
            mat: None,
            preview_off: 0.0,
        };
        app.mode = Mode::Extend(st.clone());
        (app, st)
    };
    // Ripple-growing the gap to 7 pushes clip #2 right: timeline 9.
    let (app, st) = make(true, Armed::GapTail { index: 1 }, 5.0, 7.0);
    assert_eq!(projected_total(&app, &st), 9.0);
    // Replace-growing absorbs clip #2: length held at 7.
    let (app, st) = make(false, Armed::GapTail { index: 1 }, 5.0, 7.0);
    assert_eq!(projected_total(&app, &st), 7.0);
    // A clip tail grown past the end projects the new end.
    let (app, st) = make(true, Armed::Clip { id: 2, head: false }, 7.0, 8.0);
    assert_eq!(projected_total(&app, &st), 8.0);
}
