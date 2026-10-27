use super::super::*;
use super::*;

// A transition renders as video (`xfade`); on an audio track the export
// graph would carry a filter it can't apply, so refuse it — as effects are.
#[test]
fn a_transition_needs_a_video_track() {
    let mut p = clip_pair(); // [C1 src 0..3][C2 src 0..3] on v1
    let ai = p.timeline.add_track(TrackKind::Audio);
    p.timeline.tracks[ai].segments = vec![
        Segment::Clip(Clip::new(
            1,
            1,
            Seconds::from_integer(0),
            Seconds::from_integer(2),
        )),
        Segment::Clip(Clip::new(
            2,
            1,
            Seconds::from_integer(0),
            Seconds::from_integer(2),
        )),
    ];
    assert!(matches!(
        add_transition(
            &p,
            ai,
            2.0,
            TransitionKind::Dissolve,
            Seconds::from_integer(1)
        ),
        Err(EditError::TransitionsNeedVideo)
    ));
    // The video track is unaffected.
    assert!(
        add_transition(
            &p,
            0,
            3.0,
            TransitionKind::Dissolve,
            Seconds::from_integer(1)
        )
        .is_ok()
    );
}

#[test]
fn add_transition_at_a_clip_clip_seam() {
    // [A src 0..3] [B src 0..3]: a 1 s dissolve at the 3.0 cut.
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    let clip = |id, in_, out| {
        Segment::Clip(Clip {
            id,
            asset: a,
            asset_in: Seconds::from_integer(in_),
            asset_out: Seconds::from_integer(out),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    };
    p.timeline.video_track_mut().unwrap().segments = vec![clip(1, 0, 3), clip(2, 0, 3)];

    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();

    let segs = &p.timeline.video_track().unwrap().segments;
    assert_eq!(segs.len(), 3, "A', transition, B'");
    // Each clip gave up its 1 s at the seam.
    match &segs[0] {
        Segment::Clip(c) => assert_eq!(c.asset_out, Seconds::from_integer(2)),
        s => panic!("expected a clip, got {s:?}"),
    }
    match &segs[2] {
        Segment::Clip(c) => assert_eq!(c.asset_in, Seconds::from_integer(1)),
        s => panic!("expected a clip, got {s:?}"),
    }
    match &segs[1] {
        Segment::Transition(t) => {
            assert_eq!(t.dur, Seconds::from_integer(1));
            assert_eq!(
                t.a,
                TransitionSource::Clip {
                    asset: a,
                    src_in: Seconds::from_integer(2),
                    src_out: Seconds::from_integer(3),
                    effects: Vec::new(),
                }
            );
            assert_eq!(
                t.b,
                TransitionSource::Clip {
                    asset: a,
                    src_in: Seconds::from_integer(0),
                    src_out: Seconds::from_integer(1),
                    effects: Vec::new(),
                }
            );
        }
        s => panic!("expected a transition, got {s:?}"),
    }
    // clip↔clip shortens the timeline by `d`.
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn add_transition_at_a_clip_gap_seam() {
    // fixture(): [C1 src 1..3] [gap 1] [C2 src 0..2]; a 1 s fade at the
    // 2.0 clip|gap cut.
    let mut p = fixture();
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        2.0,
        TransitionKind::FadeBlack,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();

    let segs = &p.timeline.video_track().unwrap().segments;
    assert_eq!(segs.len(), 4, "C1', transition, gap, C2");
    // The clip gives up 1 s; the gap (and the timeline length) do not.
    match &segs[0] {
        Segment::Clip(c) => assert_eq!(c.asset_out, Seconds::from_integer(2)),
        s => panic!("expected a clip, got {s:?}"),
    }
    match &segs[1] {
        Segment::Transition(t) => {
            assert_eq!(t.b, TransitionSource::Black, "the gap side is black");
            assert!(
                matches!(t.a, TransitionSource::Clip { .. }),
                "the clip side"
            );
        }
        s => panic!("expected a transition, got {s:?}"),
    }
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5),
        "length unchanged"
    );
}

#[test]
fn retime_transition_keeps_each_sides_rate() {
    // [A src 0..4] [B src 0..4], a 1 s dissolve at the cut; retiming to
    // 2 s takes another second from each side, to 0.5 s gives it back.
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    let clip = |id, in_, out| {
        Segment::Clip(Clip {
            id,
            asset: a,
            asset_in: Seconds::from_integer(in_),
            asset_out: Seconds::from_integer(out),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    };
    p.timeline.video_track_mut().unwrap().segments = vec![clip(1, 0, 4), clip(2, 0, 4)];
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        4.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();

    let segs = |p: &Project| p.timeline.video_track().unwrap().segments.clone();
    // 2 s: each clip keeps 2 s and the blend covers 2 s of each.
    let batch = retime_transition_on(&p, 0, 4.0, Seconds::from_integer(2))
        .unwrap()
        .expect("a change");
    h.apply(&mut p, batch, 0).unwrap();
    let segs_now = segs(&p);
    let Segment::Transition(t) = &segs_now[1] else {
        panic!("expected a blend")
    };
    assert_eq!(t.dur, Seconds::from_integer(2));
    match (&segs_now[0], &segs_now[2]) {
        (Segment::Clip(a), Segment::Clip(b)) => {
            assert_eq!(
                (a.asset_out, b.asset_in),
                (Seconds::from_integer(2), Seconds::from_integer(2))
            );
            assert_eq!(
                (a.duration(), b.duration()),
                (Seconds::from_integer(2), Seconds::from_integer(2))
            );
        }
        other => panic!("expected clips, got {other:?}"),
    }
    assert_eq!(p.timeline.end(), Seconds::from_integer(6), "8 - 2");

    // One undo restores the 1 s blend and both edges.
    h.undo(&mut p).unwrap();
    let back = segs(&p);
    let Segment::Transition(t) = &back[1] else {
        panic!("expected a blend")
    };
    assert_eq!(t.dur, Seconds::from_integer(1));
    match (&back[0], &back[2]) {
        (Segment::Clip(a), Segment::Clip(b)) => {
            assert_eq!(
                (a.asset_out, b.asset_in),
                (Seconds::from_integer(3), Seconds::from_integer(1))
            );
        }
        other => panic!("expected clips, got {other:?}"),
    }

    // Growing past the handles refuses.
    assert!(matches!(
        retime_transition_on(&p, 0, 4.0, Seconds::from_integer(5)),
        Err(EditError::BlendNoHandle { .. })
    ));
    // A no-op length is not a batch.
    assert!(
        retime_transition_on(&p, 0, 4.0, Seconds::from_integer(1))
            .unwrap()
            .is_none()
    );
}

#[test]
fn retime_a_fade_keeps_the_timeline_length() {
    // fixture(): [C1 src 1..3] [gap 1] [C2 src 0..2]; a 1 s fade at 2.0.
    let mut p = fixture();
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        2.0,
        TransitionKind::FadeBlack,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    // Shrink to 0.5 s: the clip gets half a second back, the black side
    // has no material, and the timeline stays 5 s.
    let batch = retime_transition_on(&p, 0, 2.0, Seconds::new(1, 2))
        .unwrap()
        .expect("a change");
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    match &track.segments[0] {
        Segment::Clip(c) => assert_eq!(c.asset_out, Seconds::new(5, 2)),
        s => panic!("expected a clip, got {s:?}"),
    }
    let Segment::Transition(t) = &track.segments[1] else {
        panic!("expected a blend")
    };
    assert_eq!(t.dur, Seconds::new(1, 2));
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn add_transition_refuses_bad_seams() {
    let p = fixture();
    // Mid-clip isn't a boundary.
    assert!(matches!(
        add_transition(
            &p,
            0,
            1.5,
            TransitionKind::Dissolve,
            Seconds::from_integer(1)
        ),
        Err(EditError::NothingToTrim(_))
    ));
    // Longer than the neighbours.
    assert!(matches!(
        add_transition(
            &p,
            0,
            2.0,
            TransitionKind::Dissolve,
            Seconds::from_integer(5)
        ),
        Err(EditError::TooShort)
    ));
    // Both sides black (leading gap): at 0 the left is the edge and the
    // right is a gap.
    let mut q = fixture();
    q.timeline.video_track_mut().unwrap().segments.insert(
        0,
        Segment::Gap(Gap {
            duration: Seconds::from_integer(1),
        }),
    );
    assert!(matches!(
        add_transition(
            &q,
            0,
            0.0,
            TransitionKind::Dissolve,
            Seconds::from_integer(1)
        ),
        Err(EditError::NoClip)
    ));
}

// `x` then `dd` is a round trip: removing a transition hands each
// neighbour's trimmed span back.
#[test]
fn deleting_a_transition_restores_the_neighbours() {
    let mut p = clip_pair();
    let before = p.clone();
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5),
        "clip|clip shortens the timeline"
    );

    // `dd` with the playhead inside the transition.
    let batch = delete_under(&p, 2.5).unwrap();
    assert!(batch.desc.contains("left and right"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(p, before, "x then dd is a round trip");
}

// The clip|gap case: only the clip side has a handle to give back; the
// gap (and the length) stay.
#[test]
fn deleting_a_clip_gap_transition_restores_the_clip() {
    let mut p = fixture();
    let before = p.clone();
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        2.0,
        TransitionKind::FadeBlack,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    // [C1' 0..1][transition 1..2][gap 2..3][C2 3..5]
    let batch = delete_under(&p, 1.5).unwrap();
    assert!(batch.desc.contains("restored the left"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        p, before,
        "the clip's tail comes back, the gap is untouched"
    );
}

// A neighbour edited since `x` no longer lines up: it is left alone (its
// material stays gone) while the other side still gets its own back.
#[test]
fn deleting_a_transition_leaves_a_stale_neighbour_alone() {
    let mut p = clip_pair();
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    // [C1' 0..2 src 0..2][transition 2..3][C2' 3..5 src 1..3]
    // Pull #2's head further in: it no longer starts where `b` ends.
    let Segment::Clip(c2) = &mut p.timeline.video_track_mut().unwrap().segments[2] else {
        unreachable!()
    };
    c2.asset_in += Seconds::from_integer(1);

    let batch = delete_under(&p, 2.5).unwrap();
    assert!(batch.desc.contains("restored the left"), "{}", batch.desc);
    assert!(!batch.desc.contains("right"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let segs = &p.timeline.video_track().unwrap().segments;
    assert_eq!(segs.len(), 2, "the transition goes either way");
    match &segs[..] {
        [Segment::Clip(a), Segment::Clip(b)] => {
            assert_eq!(a.asset_out, Seconds::from_integer(3), "left handle back");
            assert_eq!(
                b.asset_in,
                Seconds::from_integer(2),
                "the stale neighbour is untouched"
            );
        }
        other => panic!("expected the two clips, got {other:?}"),
    }
}

// With no handle left to restore, the removed blend's span joins the
// neighbouring gap(s): two adjacent gaps would be the one shape track
// deletes never produce, and the span can't just vanish (that ripples
// everything after it). Any restored handle covers the whole span (the
// sides overlay it), so these cases all have no restore left.
#[test]
fn deleting_a_transition_fuses_the_span_into_the_gaps() {
    let mut p = clip_pair(); // [C1 0..3][C2 3..6]
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    // [C1' 0..2][T 2..3][C2' 3..5]: delete both handles
    // -> [gap 0..2][T 2..3][gap 3..5]
    let batch = delete_under(&p, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let batch = delete_under(&p, 4.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let end = p.timeline.end(); // 5

    let batch = delete_under(&p, 2.5).unwrap();
    assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let segs = &p.timeline.video_track().unwrap().segments;
    match &segs[..] {
        [Segment::Gap(g)] => {
            assert_eq!(g.duration, Seconds::from_integer(5), "one fused gap");
        }
        other => panic!("expected a single gap, got {other:?}"),
    }
    assert_eq!(p.timeline.end(), end, "no ripple: the length holds");

    // A gap on one side only: with the right handle stale (the clip
    // changed since), the left gap alone absorbs the span, the track
    // keeps its length, and the stale clip never moves.
    let mut p = clip_pair();
    let batch = add_transition(
        &p,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let batch = delete_under(&p, 1.0).unwrap(); // C1' -> gap [0..2)
    h.apply(&mut p, batch, 0).unwrap();
    let Segment::Clip(c2) = &mut p.timeline.video_track_mut().unwrap().segments[2] else {
        unreachable!()
    };
    c2.asset_in += Seconds::from_integer(1); // stale right handle
    let end = p.timeline.end(); // 5

    let batch = delete_under(&p, 2.5).unwrap();
    assert!(batch.desc.contains("span joins the gap"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let segs = &p.timeline.video_track().unwrap().segments;
    match &segs[..] {
        [Segment::Gap(g), Segment::Clip(c)] => {
            assert_eq!(g.duration, Seconds::from_integer(3), "2 + the span");
            assert_eq!(c.asset_in, Seconds::from_integer(2), "C2 stays stale");
        }
        other => panic!("expected gap/clip, got {other:?}"),
    }
    assert_eq!(p.timeline.end(), end, "no ripple: the length holds");
    assert_eq!(
        p.timeline.video_track().unwrap().start_of(1),
        Seconds::from_integer(3),
        "C2 is unmoved"
    );
}

/// A straddled transition that would shrink to under a frame is dropped
/// whole (like a clip stump) rather than left as a sliver the export
/// can't render.
#[test]
fn a_straddled_transition_sliver_is_absorbed() {
    let p = fixture();
    let mut ops = Vec::new();
    let mut absorbed = Vec::new();
    // A 2/30 s transition, trimmed down to 1/60 s (half a frame).
    trim_or_absorb_head(
        &p,
        0,
        Straddle {
            index: 1,
            start: Seconds::from_integer(0),
            seg: transition_seg(7, Seconds::new(2, 30), Seconds::from_integer(1)),
        },
        Seconds::new(1, 20),
        &mut ops,
        &mut absorbed,
    )
    .unwrap();
    assert!(
        matches!(ops[..], [EditOp::Remove { index: 1, .. }]),
        "{ops:?}"
    );
    assert_eq!(absorbed, vec!["transition #7".to_string()]);

    // One whole frame left: it shrinks to that frame instead.
    let mut ops = Vec::new();
    let mut absorbed = Vec::new();
    trim_or_absorb_head(
        &p,
        0,
        Straddle {
            index: 1,
            start: Seconds::from_integer(0),
            seg: transition_seg(7, Seconds::new(2, 30), Seconds::from_integer(1)),
        },
        Seconds::new(1, 30),
        &mut ops,
        &mut absorbed,
    )
    .unwrap();
    match &ops[..] {
        [
            EditOp::Replace {
                after: Segment::Transition(t),
                ..
            },
        ] => assert_eq!(t.dur, Seconds::new(1, 30), "shrunk to the one frame left"),
        other => panic!("expected a shrink, got {other:?}"),
    }
    assert!(absorbed.is_empty());
}
