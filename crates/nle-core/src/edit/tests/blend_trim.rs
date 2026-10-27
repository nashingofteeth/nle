use super::super::*;
use super::*;
use crate::timeline::{
    Clip, Segment, Track, TrackKind, Transition, TransitionKind, TransitionSource,
};

// The transition is part of both neighbours: `o`/`O` moves the blend's
// right edge (the preceding clip's far end) and `i`/`I` its left edge. The
// clip on the moved side covers what the blend gives up, so no material is
// lost and `span / dur` stays 1×.
#[test]
fn trimming_through_a_blend_shrinks_it() {
    let blend = |p: &Project| -> Transition {
        match &p.timeline.video_track().unwrap().segments[1] {
            Segment::Transition(t) => t.clone(),
            other => panic!("expected the blend, got {other:?}"),
        }
    };
    let side = |t: &Transition, out: bool| match if out { &t.a } else { &t.b } {
        TransitionSource::Clip {
            src_in, src_out, ..
        } => (*src_in, *src_out),
        other => panic!("expected a clip side, got {other:?}"),
    };

    // [C1 src 0..3][C2 src 0..3] with a 1 s dissolve at the 3.0 cut:
    // C1' [0,2) src 0..2 · T [2,3) {a 2..3, b 0..1} · C2' [3,5) src 1..3.
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

    // `o` at 2.5 → the blend [2, 2.5); C2's head moves out to cover it.
    let batch = trim_at_on(&p, 0, 2.5, true, false).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(blend(&p).dur, Seconds::new(1, 2), "the blend shrank");
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5),
        "length unchanged"
    );
    match &p.timeline.video_track().unwrap().segments[..] {
        [Segment::Clip(c1), Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(c1.asset_out, Seconds::from_integer(2), "C1 is whole");
            assert_eq!(c2.asset_in, Seconds::new(1, 2), "C2 reclaimed its head");
            assert_eq!(t.a.asset(), Some(1));
            assert_eq!(
                side(t, true),
                (Seconds::from_integer(2), Seconds::new(5, 2)),
                "a keeps its start, gives up its far end"
            );
            assert_eq!(
                side(t, false),
                (Seconds::from_integer(0), Seconds::new(1, 2)),
                "b keeps its start and ends at C2's new in"
            );
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
    assert_eq!(
        (side_rate(&p, 1, true), side_rate(&p, 1, false)),
        (Seconds::from_integer(1), Seconds::from_integer(1)),
        "both sides still play at their neighbour's rate"
    );

    // Off the midpoint (`o` at 2.25 snaps to frame 68 = 34/15): the
    // blend [2, 34/15); `b` must still end at C2's new in or the seam
    // skips (and a retime on this blend would refuse with NoClipSide).
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
    let batch = trim_at_on(&p, 0, 2.25, true, false).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    match &p.timeline.video_track().unwrap().segments[..] {
        [_, Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(
                side(t, false),
                (Seconds::from_integer(0), Seconds::new(4, 15)),
                "b spans back from C2's new in"
            );
            assert_eq!(c2.asset_in, Seconds::new(4, 15), "C2 reclaimed its head");
            assert!(
                retime_transition_on(&p, 0, 2.125, Seconds::from_integer(1))
                    .unwrap()
                    .is_some(),
                "the trimmed blend can still be retimed"
            );
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
    assert_eq!(
        side_rate(&p, 1, false),
        Seconds::from_integer(1),
        "b still plays at C2's rate off the midpoint"
    );
    assert_eq!(
        (side_rate(&p, 1, true), side_rate(&p, 1, false)),
        (Seconds::from_integer(1), Seconds::from_integer(1)),
        "both sides still play at their neighbour's rate"
    );

    // `i` at 2.5 on a fresh blend → the blend [2.5, 3); C1's tail moves out.
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
    let batch = trim_at_on(&p, 0, 2.5, false, false).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(blend(&p).dur, Seconds::new(1, 2), "the blend shrank");
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5),
        "length unchanged"
    );
    match &p.timeline.video_track().unwrap().segments[..] {
        [Segment::Clip(c1), Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(c1.asset_out, Seconds::new(5, 2), "C1's tail moved out");
            assert_eq!(c2.asset_in, Seconds::from_integer(1), "C2 is whole");
            assert_eq!(
                side(t, true),
                (Seconds::new(5, 2), Seconds::from_integer(3)),
                "a follows C1's out"
            );
            assert_eq!(
                side(t, false),
                (Seconds::new(1, 2), Seconds::from_integer(1)),
                "b keeps its end"
            );
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
    assert_eq!(
        (side_rate(&p, 1, true), side_rate(&p, 1, false)),
        (Seconds::from_integer(1), Seconds::from_integer(1)),
        "both sides still play at their neighbour's rate"
    );
}

// A clip beside a blend hands the trimmed material to the blend instead of
// leaving a gap: `o` on the preceding clip moves the blend's left edge,
// `i` on the following clip its right edge. The visible span shrinks and
// the blend grows by the same amount, so the timeline length is unchanged
// — that is the transition being part of both neighbours.
#[test]
fn a_trim_beside_a_blend_grows_it() {
    // C2 starts at source 1, so a 2 s blend has 2 s of its pre-roll.
    let blend_with_handles = || {
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
        p.timeline.video_track_mut().unwrap().segments = vec![clip(1, 0, 3), clip(2, 1, 4)];
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
        // C1 [0,2) src 0..2 · T [2,3) {a 2..3, b 1..2} · C2 [3,5) src 2..4.
        p
    };
    let side = |t: &Transition, out: bool| match if out { &t.a } else { &t.b } {
        TransitionSource::Clip {
            src_in, src_out, ..
        } => (*src_in, *src_out),
        other => panic!("expected a clip side, got {other:?}"),
    };

    // `o` at 1.0 on C1 → C1 [0,1), the blend [1,3), C2 untouched.
    let mut p = blend_with_handles();
    let mut h = History::default();
    let batch = trim_at_on(&p, 0, 1.0, true, false).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    match &p.timeline.video_track().unwrap().segments[..] {
        [Segment::Clip(c1), Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(c1.asset_out, Seconds::from_integer(1), "C1's out moved");
            assert_eq!(t.dur, Seconds::from_integer(2), "the blend grew");
            assert_eq!(
                side(t, true),
                (Seconds::from_integer(1), Seconds::from_integer(3)),
                "a follows C1's new out, keeping its far end"
            );
            assert_eq!(
                side(t, false),
                (Seconds::from_integer(0), Seconds::from_integer(2)),
                "b keeps C2's in, taking its pre-roll"
            );
            assert_eq!(c2.asset_in, Seconds::from_integer(2), "C2 is untouched");
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5),
        "no gap, no ripple: the blend absorbed the trim"
    );
    assert_eq!(
        (side_rate(&p, 1, true), side_rate(&p, 1, false)),
        (Seconds::from_integer(1), Seconds::from_integer(1)),
        "both sides still play at their neighbour's rate"
    );

    // `i` at 4.0 on C2 → C2 [4,5), the blend [2,4), C1 untouched.
    let mut p = blend_with_handles();
    let mut h = History::default();
    let batch = trim_at_on(&p, 0, 4.0, false, false).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    match &p.timeline.video_track().unwrap().segments[..] {
        [Segment::Clip(c1), Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(c1.asset_out, Seconds::from_integer(2), "C1 is untouched");
            assert_eq!(t.dur, Seconds::from_integer(2), "the blend grew");
            assert_eq!(
                side(t, true),
                (Seconds::from_integer(2), Seconds::from_integer(4)),
                "a keeps C1's out, taking its post-roll"
            );
            assert_eq!(
                side(t, false),
                (Seconds::from_integer(1), Seconds::from_integer(3)),
                "b follows C2's new in"
            );
            assert_eq!(c2.asset_in, Seconds::from_integer(3), "C2's in moved");
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "no gap");
    assert_eq!(
        (side_rate(&p, 1, true), side_rate(&p, 1, false)),
        (Seconds::from_integer(1), Seconds::from_integer(1)),
        "both sides still play at their neighbour's rate"
    );

    // `O` at 1.0 instead keeps the blend's duration and ripples: C1 [0,1),
    // the blend [1,2), C2 [2,4) — one second shorter, no gap.
    let mut p = blend_with_handles();
    let mut h = History::default();
    let batch = trim_at_on(&p, 0, 1.0, true, true).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    match &p.timeline.video_track().unwrap().segments[..] {
        [Segment::Clip(c1), Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(c1.asset_out, Seconds::from_integer(1), "C1's out moved");
            assert_eq!(t.dur, Seconds::from_integer(1), "the blend kept its dur");
            assert_eq!(
                side(t, true),
                (Seconds::from_integer(1), Seconds::from_integer(2)),
                "a follows C1's new out"
            );
            assert_eq!(
                side(t, false),
                (Seconds::from_integer(1), Seconds::from_integer(2)),
                "b is unmoved"
            );
            assert_eq!(c2.asset_in, Seconds::from_integer(2), "C2 slid intact");
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(4),
        "a ripple trim shortens the timeline"
    );
    assert_eq!(
        (side_rate(&p, 1, true), side_rate(&p, 1, false)),
        (Seconds::from_integer(1), Seconds::from_integer(1)),
        "both sides still play at their neighbour's rate"
    );

    // `I` at 4.0: C2 [3,4), the blend [2,3) with `b` following the new in.
    let mut p = blend_with_handles();
    let mut h = History::default();
    let batch = trim_at_on(&p, 0, 4.0, false, true).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    match &p.timeline.video_track().unwrap().segments[..] {
        [Segment::Clip(_), Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(t.dur, Seconds::from_integer(1), "the blend kept its dur");
            assert_eq!(
                side(t, true),
                (Seconds::from_integer(2), Seconds::from_integer(3)),
                "a is unmoved"
            );
            assert_eq!(
                side(t, false),
                (Seconds::from_integer(2), Seconds::from_integer(3)),
                "b follows C2's new in"
            );
            assert_eq!(c2.asset_in, Seconds::from_integer(3), "C2's in moved");
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(4),
        "rippled shorter"
    );
    assert_eq!(
        (side_rate(&p, 1, true), side_rate(&p, 1, false)),
        (Seconds::from_integer(1), Seconds::from_integer(1)),
        "both sides still play at their neighbour's rate"
    );

    // Without the handle the trim is refused rather than written broken
    // (a negative source used to round-trip into `-1/1` in the project).
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
    match trim_at_on(&p, 0, 1.0, true, false) {
        Err(EditError::BlendNoHandle { id: 3, detail }) => {
            assert!(detail.contains("before its in-point"), "{detail}");
        }
        other => panic!("expected a refused trim, got {other:?}"),
    }
}

// Extending a neighbour into a blend shrinks it the same way the trim does:
// the side follows the clip's edge, so the crossfade plays at the
// neighbour's rate (it used to keep its old span and double the rate).
#[test]
fn extending_into_a_blend_keeps_the_rate() {
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
    // red's out absorbs the blend's left 0.5 s (`e`, non-ripple).
    let batch = absorb_outward_on(&p, 0, 1, false, 2.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    match &p.timeline.video_track().unwrap().segments[..] {
        [Segment::Clip(c1), Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(c1.asset_out, Seconds::new(5, 2), "red absorbed 0.5 s");
            assert_eq!(t.dur, Seconds::new(1, 2), "the blend shrank");
            assert_eq!(c2.asset_in, Seconds::from_integer(1), "blue is whole");
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
    assert_eq!(
        (side_rate(&p, 1, true), side_rate(&p, 1, false)),
        (Seconds::from_integer(1), Seconds::from_integer(1)),
        "both sides still play at their neighbour's rate"
    );
}

// A ripple extend shifts the blend's side with the clip's new edge, so the
// clip's material does not play twice (once in its span, once in the blend).
#[test]
fn a_ripple_extend_shifts_the_blend_side() {
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
    // red's out ripples out to 2.5; the blend shifts right with it.
    let batch = extend_edge(&p, 1, false, 2.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    match &p.timeline.video_track().unwrap().segments[..] {
        [Segment::Clip(c1), Segment::Transition(t), Segment::Clip(c2)] => {
            assert_eq!(c1.asset_out, Seconds::new(5, 2), "red grew");
            assert_eq!(t.dur, Seconds::from_integer(1), "the blend kept its length");
            let TransitionSource::Clip {
                src_in, src_out, ..
            } = &t.a
            else {
                panic!("expected a clip side")
            };
            assert_eq!(*src_in, c1.asset_out, "a follows red's new out");
            assert_eq!(*src_out, Seconds::new(7, 2), "a kept its span");
            assert_eq!(c2.asset_in, Seconds::from_integer(1), "blue is whole");
        }
        other => panic!("expected clip/blend/clip, got {other:?}"),
    }
}

/// A transition's side renders its span into the transition's own `dur`,
/// so `span / dur` must equal the neighbouring clip's rate. Retiming the
/// clip retimes the side: `dur` and the cut (`a.src_out` / `b.src_in`)
/// stay put, the inner edge follows — and that edge is the clip's in/out
/// point, so the two stay contiguous.
#[test]
fn speed_retimes_the_transition_sides_it_owns() {
    // [C1 src 0..3][C2 src 0..3] with a 1 s dissolve at the 3.0 cut:
    // C1' src 0..2 [0,2) · T{a 2..3, b 0..1} [2,3) · C2' src 1..3 [3,5)
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

    // The clip that *leads* into the transition owns the `a` side. At 2×
    // the whole contribution is the first 3 s of the take: 0.5 s of clip
    // plus the transition's own 1 s, so the clip gives up 1 s of source.
    let batch = set_speed(&p, 1, Seconds::from_integer(2)).unwrap();
    assert!(
        batch.desc.contains("transition side(s) retimed"),
        "{}",
        batch.desc
    );
    let mut q = p.clone();
    h.apply(&mut q, batch, 0).unwrap();
    let c = clip_by_id(&q, 1);
    assert_eq!(
        (c.speed, c.asset_in, c.asset_out),
        (2.into(), 0.into(), 1.into()),
        "the clip's own span shrank to what the transition leaves"
    );
    let Segment::Transition(t) = &q.timeline.video_track().unwrap().segments[1] else {
        panic!("expected the transition")
    };
    assert_eq!(
        t.dur,
        Seconds::from_integer(1),
        "the crossfade length holds"
    );
    let (ain, aout) = match &t.a {
        TransitionSource::Clip {
            src_in, src_out, ..
        } => (*src_in, *src_out),
        TransitionSource::Black => panic!("expected a clip side"),
    };
    assert_eq!(
        (ain, aout),
        (Seconds::from_integer(1), Seconds::from_integer(3)),
        "a keeps the cut (src_out) and follows the clip (src_in)"
    );
    assert_eq!(
        side_rate(&q, 1, true),
        2.into(),
        "a plays at the clip's rate"
    );
    assert_eq!(
        side_rate(&q, 1, false),
        1.into(),
        "b keeps the other clip's"
    );
    assert_eq!(
        clip_by_id(&q, 2),
        clip_by_id(&p, 2),
        "the follower is untouched"
    );
    assert_eq!(q.timeline.end(), Seconds::new(7, 2), "0.5 + 1 + 2");

    // One undo covers the clip and the side it retimed.
    h.undo(&mut q).unwrap();
    assert_eq!(q, p, "undo retimes the side back with the clip");

    // The mirror: the clip that *follows* the transition owns the `b` side.
    let mut q = p.clone();
    let batch = set_speed(&p, 2, Seconds::from_integer(2)).unwrap();
    History::default().apply(&mut q, batch, 0).unwrap();
    let c = clip_by_id(&q, 2);
    assert_eq!(
        (c.speed, c.asset_in, c.asset_out),
        (2.into(), 2.into(), 3.into())
    );
    assert_eq!(
        side_rate(&q, 1, false),
        2.into(),
        "b plays at the clip's rate"
    );
    assert_eq!(side_rate(&q, 1, true), 1.into(), "a keeps the other clip's");
}

/// A clip between two transitions is retimed as one rate across all three
/// spans, and refused when the two sides would need more material than the
/// clip has.
#[test]
fn speed_retimes_a_clip_between_two_transitions() {
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("30"));
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
    p.timeline.video_track_mut().unwrap().segments =
        vec![clip(1, 0, 4), clip(2, 0, 4), clip(3, 0, 4)];
    let mut h = History::default();
    for at in [4.0, 7.0] {
        let batch = add_transition(
            &p,
            0,
            at,
            TransitionKind::Dissolve,
            Seconds::from_integer(1),
        )
        .unwrap();
        h.apply(&mut p, batch, 0).unwrap();
    }
    // [C1' 0..3][T{a 3..4, b 0..1}][C2'' 1..3][T{a 3..4, b 0..1}][C3' 1..4]
    // Both sides must give up `dur × rate` of source, so 2× would need all
    // 4 s of the middle clip and is refused.
    assert!(matches!(
        set_speed(&p, 2, Seconds::from_integer(2)),
        Err(EditError::TooShort)
    ));

    // 1.5× fits: 4 s of source / 1.5 = 2.667 s, minus the two 1 s sides.
    let batch = set_speed(&p, 2, Seconds::from_integer(3) / Seconds::from_integer(2)).unwrap();
    let mut q = p.clone();
    History::default().apply(&mut q, batch, 0).unwrap();
    let c = clip_by_id(&q, 2);
    assert_eq!(c.speed, Seconds::from_integer(3) / Seconds::from_integer(2));
    assert_eq!(
        (c.asset_in, c.asset_out),
        (Seconds::new(3, 2), Seconds::new(5, 2))
    );
    assert_eq!(c.duration(), Seconds::new(2, 3), "1 s of source at 1.5×");
    assert_eq!(
        side_rate(&q, 1, false),
        Seconds::new(3, 2),
        "the leading side"
    );
    assert_eq!(
        side_rate(&q, 3, true),
        Seconds::new(3, 2),
        "the trailing side"
    );
}

#[test]
fn undo_restores_a_transition_bit_for_bit() {
    let mut p = fixture();
    let track = p.timeline.video_track_mut().unwrap();
    let idx = track.segments.len();
    track.segments.push(transition_seg(
        50,
        Seconds::from_integer(1),
        Seconds::from_integer(1),
    ));
    let mut h = History::default();
    let before = p.clone();
    let batch = Batch {
        ops: vec![EditOp::Remove {
            track: 0,
            index: idx,
            segment: p.timeline.video_track().unwrap().segments[idx].clone(),
        }],
        desc: "remove transition".into(),
    };
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        p.timeline.video_track().unwrap().segments.len(),
        3,
        "fixture's 3 segments (the transition was removed)"
    );
    h.undo(&mut p).unwrap();
    assert_eq!(p, before, "the transition came back exactly");
}

#[test]
fn clamp_to_media_snaps_transition_dur_and_clamps_sources() {
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        // 0.35 s isn't a whole frame at 30 fps; the `a` source runs
        // past a 3 s asset.
        segments: vec![transition_seg(
            1,
            Seconds::new(35, 100),
            Seconds::from_integer(5),
        )],
    };
    let notes = clamp_to_media(
        &mut t,
        &|_| Some(Seconds::from_integer(3)),
        Seconds::from_integer(30),
    );
    assert_eq!(notes.len(), 2, "{notes:?}");
    let Segment::Transition(tr) = &t.segments[0] else {
        panic!("expected a transition")
    };
    assert_eq!(tr.dur, Seconds::new(11, 30), "snapped to a whole frame");
    match &tr.a {
        TransitionSource::Clip { src_out, .. } => {
            assert_eq!(*src_out, Seconds::from_integer(3), "clamped to the media")
        }
        TransitionSource::Black => panic!("expected a clip side"),
    }
}
