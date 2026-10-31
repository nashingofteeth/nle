use super::*;

#[test]
fn edits_record_undo_history() {
    let mut p = fixture();
    let mut reg = None;
    let mut h = History::default();
    let out = run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Split { track: 0, at: 2.5 },
        2.5,
    )
    .unwrap();
    assert!(out.changed && out.playhead.is_none());
    assert!(h.can_undo());
    // Undo returns to the frame the edit was made from.
    let undone = run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(undone.playhead, Some(75), "2.5 s @ 30 fps");
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 4);
}

// `--clip` finds the clip's own track: `effect` used to need an explicit
// `--track`, unlike delete/yank/speed.
#[test]
fn add_effect_finds_the_clips_own_track() {
    let mut p = fixture();
    let vi = p.timeline.add_track(TrackKind::Video);
    p.timeline.tracks[vi].segments = vec![Segment::Clip(Clip {
        id: 9,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(1),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let mut reg = None;
    let out = run(
        &mut p,
        &mut reg,
        &Action::AddEffect {
            track: 0, // the default track; clip #9 lives on the second
            clip: 9,
            effect: Effect::Eq {
                brightness: 0.0,
                contrast: 1.0,
                saturation: 1.0,
            },
            index: None,
        },
    )
    .unwrap();
    assert!(out.changed, "{}", out.desc);
    match &p.timeline.tracks[vi].segments[0] {
        Segment::Clip(c) => assert_eq!(c.effects.len(), 1, "the effect landed"),
        other => panic!("expected a clip, got {other:?}"),
    }
}

#[test]
fn a_transition_action_retimes_an_existing_blend() {
    let mut p = fixture();
    let mut reg = None;
    let action = |len| Action::Transition {
        track: 0,
        at: 4.0,
        kind: TransitionKind::Dissolve,
        len,
    };
    let dur = |p: &Project| {
        p.timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .find_map(|s| match s {
                Segment::Transition(t) => Some(t.dur),
                _ => None,
            })
            .unwrap()
    };
    run(&mut p, &mut reg, &action(TransitionLen::Frames(30))).unwrap();
    assert_eq!(dur(&p), Seconds::from_integer(1));

    // The original cut is the blend's far edge, and still addresses it.
    let out = run(&mut p, &mut reg, &action(TransitionLen::Frames(15))).unwrap();
    assert!(out.changed);
    assert!(out.desc.contains("15 frames"), "{}", out.desc);
    assert_eq!(dur(&p), Seconds::new(1, 2));

    // A rate is relative to the current length: 50% doubles it.
    run(&mut p, &mut reg, &action(TransitionLen::Percent(50.0))).unwrap();
    assert_eq!(dur(&p), Seconds::from_integer(1));

    // 100% is a no-op, not a history entry.
    let out = run(&mut p, &mut reg, &action(TransitionLen::Percent(100.0))).unwrap();
    assert!(!out.changed);
    assert!(out.desc.contains("unchanged"), "{}", out.desc);

    // A rate with no blend at the position refuses.
    let out = run(
        &mut p,
        &mut reg,
        &Action::Transition {
            track: 0,
            at: 0.5,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Percent(50.0),
        },
    );
    assert!(matches!(out, Err(EditError::NoTransition(_))));
}

// The cut a transition is added to sits in the middle of the new blend
// (the neighbours overlap there), so the playhead lands on its midpoint —
// not on either edge.
#[test]
fn a_transition_lands_on_its_midpoint() {
    // (at a seam the blend sits on the *preceding* frames: the clip
    // gives up material backward, so the blend here owns [t−dur, t).)
    let mut p = fixture(); // [Gap][Clip #1 1..4][Gap][Clip #2 6..8]
    let mut reg = None;
    let out = run(
        &mut p,
        &mut reg,
        &Action::Transition {
            track: 0,
            at: 4.0,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Frames(30),
        },
    )
    .unwrap();
    // The clip gives up 1 s, so the blend owns [3, 4): midpoint 3.5.
    assert_eq!(out.playhead, Some(105), "the middle of the blend");
    // Deleting at the blend's first frame still removes the blend and
    // hands the trimmed frames back.
    let out = run(
        &mut p,
        &mut reg,
        &Action::Delete {
            track: 0,
            target: Target::At(3.0),
            ripple: false,
        },
    )
    .unwrap();
    assert!(out.desc.contains("transition"), "{}", out.desc);
    let segs = &p.timeline.video_track().unwrap().segments;
    assert!(
        !segs.iter().any(|s| matches!(s, Segment::Transition(_))),
        "the blend is gone: {segs:?}"
    );
    match &segs[..] {
        [
            Segment::Gap(_),
            Segment::Clip(c),
            Segment::Gap(_),
            Segment::Clip(_),
        ] => {
            assert_eq!(
                (c.asset_in, c.asset_out),
                (0.into(), 3.into()),
                "the clip is whole again"
            );
        }
        other => panic!("expected the clip restored, got {other:?}"),
    }
}

// The frame-grid degenerate case: a ONE-frame blend's midpoint
// (start + half a frame) rounds up out of it — the blend owns a single
// frame, and the playhead must stay on it, not hop past it.
#[test]
fn a_one_frame_blend_lands_on_its_own_frame() {
    let mut p = fixture(); // [Gap][Clip #1 1..4][Gap][Clip #2 6..8]
    let mut reg = None;
    // The blend at a seam sits on the preceding frames (the clip gives
    // up material backward), so a one-frame blend at 4.0 owns [4−1f, 4):
    // frame 119, not 120.
    let out = run(
        &mut p,
        &mut reg,
        &Action::Transition {
            track: 0,
            at: 4.0,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Frames(1),
        },
    )
    .unwrap();
    assert_eq!(out.playhead, Some(119), "the blend's own frame");
}

#[test]
fn yank_fills_the_register_and_paste_consumes_it() {
    let mut p = fixture();
    let mut reg = None;
    let out = run(
        &mut p,
        &mut reg,
        &Action::Yank {
            track: 0,
            target: Target::Clip(1),
        },
    )
    .unwrap();
    assert!(!out.changed, "a yank writes the register, not the project");
    assert!(matches!(
        reg,
        Some(Register {
            segment: Segment::Clip(Clip { id: 1, .. }),
            ..
        })
    ));
    // Pasting without a register is an error; with one it inserts.
    assert!(matches!(
        run(
            &mut p,
            &mut None,
            &Action::Paste {
                track: 0,
                at: 7.0,
                ripple: false,
                segment: None,
            },
        )
        .unwrap_err(),
        EditError::RegisterEmpty
    ));
    run(
        &mut p,
        &mut reg,
        &Action::Paste {
            track: 0,
            at: 7.0,
            ripple: true,
            segment: None,
        },
    )
    .unwrap();
    // Pasting inside clip #2 splits it: gap, #1, gap, #2a, pasted, #2b.
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 6);
}

#[test]
fn track_edits_are_undoable() {
    let mut p = fixture();
    let mut reg = None;
    let mut h = History::default();

    // Add → undo → redo.
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::TrackAdd {
            kind: TrackKind::Audio,
        },
        0.0,
    )
    .unwrap();
    assert_eq!(p.timeline.tracks.len(), 2);
    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(p.timeline.tracks.len(), 1, "the add undoes");
    run_action(&mut p, &mut h, &mut reg, &Action::Redo, 0.0).unwrap();
    assert_eq!(p.timeline.tracks.len(), 2, "and redoes");

    // Mute → undo restores the flag.
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::TrackMute { track: 1 },
        0.0,
    )
    .unwrap();
    assert!(p.timeline.track(1).unwrap().muted);
    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert!(!p.timeline.track(1).unwrap().muted, "the mute undoes");

    // Put a clip on the new track through the catalog (so the stack
    // stays in step), then remove the track: undo brings it back with
    // its clip.
    let a = p.media[0].id;
    let clip = Segment::Clip(Clip {
        id: 0,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Paste {
            track: 1,
            at: 0.0,
            ripple: true,
            segment: Some(clip),
        },
        0.0,
    )
    .unwrap();
    assert_eq!(p.timeline.track(1).unwrap().segments.len(), 1);
    run_action(&mut p, &mut h, &mut reg, &Action::TrackRm { track: 1 }, 0.0).unwrap();
    assert_eq!(p.timeline.tracks.len(), 1);
    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(p.timeline.tracks.len(), 2, "the track is back");
    assert_eq!(
        p.timeline.track(1).unwrap().segments.len(),
        1,
        "with its clip"
    );
}

#[test]
fn a_refused_track_edit_leaves_the_project_alone() {
    let mut p = fixture();
    let mut h = History::default();
    // Removing the only video track fails validation — and the snapshot
    // is rolled back, not left half-applied.
    let err = h.edit_tracks(&mut p, 0, |p| {
        p.timeline.tracks.remove(0);
        Ok("removed the only video track".to_string())
    });
    assert!(err.is_err(), "a project needs a video track");
    assert_eq!(p.timeline.tracks.len(), 1, "the edit rolled back");
    assert!(!h.can_undo(), "nothing was recorded");
}

#[test]
fn a_track_list_changed_outside_the_stack_refuses_to_undo() {
    let mut p = fixture();
    let mut reg = None;
    let mut h = History::default();
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::TrackAdd {
            kind: TrackKind::Audio,
        },
        0.0,
    )
    .unwrap();
    // Something (a stale sidecar, an out-of-band write) moved the tracks.
    p.timeline.add_track(TrackKind::Video);
    assert!(matches!(
        run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0),
        Err(EditError::History(_))
    ));
}

#[test]
fn purge_is_undoable() {
    let mut p = fixture();
    // Drift clip #1 to 60.4 frames at 30 fps: purge snaps it to 60.
    if let Segment::Clip(c) = &mut p.timeline.tracks[0].segments[1] {
        c.asset_out = c.asset_in + Seconds::new(604, 300);
    }
    let mut reg = None;
    let mut h = History::default();
    let out = run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::PurgeStumps { track: None },
        0.0,
    )
    .unwrap();
    assert!(out.desc.contains("purged"), "{}", out.desc);
    let span = |p: &Project| match &p.timeline.tracks[0].segments[1] {
        Segment::Clip(c) => c.asset_out - c.asset_in,
        other => panic!("expected a clip, got {other:?}"),
    };
    assert_eq!(span(&p), Seconds::from_integer(2), "snapped to 60 frames");
    run_action(&mut p, &mut h, &mut reg, &Action::Undo, 0.0).unwrap();
    assert_eq!(span(&p), Seconds::new(604, 300), "the drift is back");
}

/// Clip ids are unique across tracks: a clip-addressed action finds its
/// own track, so `nile speed ID` needs no `--track` (same as
/// delete/yank/extend).
#[test]
fn a_clip_addressed_action_finds_its_own_track() {
    let mut p = fixture();
    let a = p.media[0].id;
    let ai = p.timeline.add_track(TrackKind::Audio);
    let clip = Segment::Clip(Clip {
        id: 0,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let mut h = History::default();
    let mut reg = None;
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::Paste {
            track: ai,
            at: 0.0,
            ripple: true,
            segment: Some(clip),
        },
        0.0,
    )
    .unwrap();
    let id = match &p.timeline.track(ai).unwrap().segments[0] {
        Segment::Clip(c) => c.id,
        other => panic!("expected a clip, got {other:?}"),
    };
    // The clip lives on the audio track; `track: 0` must still find it.
    run_action(
        &mut p,
        &mut h,
        &mut reg,
        &Action::SetSpeed {
            track: 0,
            clip: id,
            rate: Seconds::from_integer(2),
        },
        0.0,
    )
    .unwrap();
    assert_eq!(
        p.timeline.track(ai).unwrap().segments[0].duration(),
        Seconds::from_integer(1),
        "the audio-track clip was retimed"
    );
}

#[test]
fn extend_moves_an_edge_under_the_same_rules_as_the_tui() {
    let mut p = fixture();
    let mut reg = None;
    run(
        &mut p,
        &mut reg,
        &Action::Extend {
            track: 0,
            target: Target::Clip(1),
            edge: Edge::Out,
            to: 5.0,
            ripple: false,
        },
    )
    .unwrap();
    // The tail grew into the gap; the gap shrank to keep the length.
    let segs = &p.timeline.video_track().unwrap().segments;
    assert!(matches!(segs[1], Segment::Clip(Clip { id: 1, .. })));
    assert_eq!(
        crate::extend::clip_span(&p, 1, 0),
        Some((1.0, 5.0)),
        "{segs:?}"
    );
}
