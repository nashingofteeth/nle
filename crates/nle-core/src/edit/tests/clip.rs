use super::super::util::at_least_a_frame;
use super::super::*;
use super::*;
use crate::time::Seconds;
use crate::timeline::{Clip, Gap, Segment, TransitionKind};

#[test]
fn s_splits_a_clip() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = split(&p, 4.0).unwrap(); // mid clip #2 [3..5)
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2, 3]);
    assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(1));
    assert_eq!(clip_by_id(&p, 3).duration(), Seconds::from_integer(1));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);
}

#[test]
fn dd_replaces_clip_with_gap_d_removes() {
    let mut p = fixture();
    let mut h = History::default();
    // dd the first clip: a gap replaces it, fusing with the
    // neighboring gap into one — end preserved
    let batch = delete_under(&p, 1.0).unwrap();
    assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert_eq!(track.segments.len(), 2);
    assert!(
        matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(3))
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);

    // D the clip: removed, timeline shrinks by 2
    let batch = delete_ripple(&p, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_ids(&p), vec![2]);
    // the trailing gap survives (tail size preserved): end = 1(gap) + 2(clip)
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(3),
        "shrunk by the clip only"
    );

    // D the gap (re-inserted by undo? no — fresh fixture): D on a gap
    // shrinks the timeline by the gap's length
    let mut p2 = fixture();
    let batch = delete_ripple(&p2, 2.5).unwrap(); // the 1s gap
    h.apply(&mut p2, batch, 0).unwrap();
    assert_eq!(
        clip_by_id(&p2, 2).duration(),
        Seconds::from_integer(2),
        "spans intact"
    );
    assert_eq!(p2.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
}

#[test]
fn dd_on_trailing_gap_removes_it() {
    let mut p = fixture();
    p.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(Segment::Gap(Gap {
            duration: Seconds::from_integer(3),
        })); // trailing [5..8)
    let mut h = History::default();
    let batch = delete_under(&p, 6.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "tail gone");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(8));
}

#[test]
fn d_on_gap_between_matching_clips_rejoins_them() {
    // [Clip #1 [0..2) src 1..3] [Gap 1] [Clip #5 [3..5) src 3..5]:
    // contiguous halves of one source range.
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    p.timeline.video_track_mut().unwrap().segments = vec![
        Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(1),
            asset_out: Seconds::from_integer(3),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        Segment::Gap(Gap {
            duration: Seconds::from_integer(1),
        }),
        Segment::Clip(Clip {
            id: 5,
            asset: a,
            asset_in: Seconds::from_integer(3),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
    ];
    let mut h = History::default();
    let batch = delete_ripple(&p, 2.5).unwrap();
    assert!(batch.desc.contains("merged clips #1+#5"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    // one clip, earlier id, whole source range
    assert_eq!(clip_ids(&p), vec![1]);
    let c = clip_by_id(&p, 1);
    assert_eq!(c.asset_in, Seconds::from_integer(1));
    assert_eq!(c.asset_out, Seconds::from_integer(5));
    assert_eq!(p.timeline.end(), Seconds::from_integer(4));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 5]);
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn d_on_gap_between_unrelated_clips_leaves_both() {
    let mut p = fixture(); // clip #1 src 1..3, clip #2 src 0..2: not contiguous
    let mut h = History::default();
    let batch = delete_ripple(&p, 2.5).unwrap();
    assert!(!batch.desc.contains("merged"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);
    assert_eq!(p.timeline.end(), Seconds::from_integer(4));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);
}

#[test]
fn dd_clip_between_gaps_merges_into_one() {
    // [Gap 2] [Clip #1 dur 2] [Gap 3]: dd the clip.
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    p.timeline.video_track_mut().unwrap().segments = vec![
        Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        }),
        Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        Segment::Gap(Gap {
            duration: Seconds::from_integer(3),
        }),
    ];
    let mut h = History::default();
    let batch = delete_under(&p, 3.0).unwrap();
    assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert_eq!(track.segments.len(), 1);
    assert!(
        matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(7))
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(7), "end preserved");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);
    assert_eq!(clip_ids(&p), vec![1]);
}

#[test]
fn d_on_clip_between_gaps_merges_the_gaps() {
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    p.timeline.video_track_mut().unwrap().segments = vec![
        Segment::Gap(Gap {
            duration: Seconds::from_integer(1),
        }),
        Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        Segment::Gap(Gap {
            duration: Seconds::from_integer(1),
        }),
    ];
    let mut h = History::default();
    let batch = delete_ripple(&p, 2.0).unwrap();
    assert!(batch.desc.contains("merged gaps"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert_eq!(track.segments.len(), 1);
    assert!(
        matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
    );
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(2),
        "shrunk by the clip"
    );
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1]);
}

#[test]
fn p_replaces_underneath() {
    // fixture: [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5) src 0..2]
    let mut p = fixture();
    let mut h = History::default();
    let snapshot = clip_by_id(&p, 1); // 2s

    // Over the tail of the gap and 1.5s into clip #2: the gap is
    // replaced up to the paste, #2 is trimmed at its near edge.
    let batch = paste(&p, &Segment::Clip(snapshot.clone()), 2.5).unwrap();
    assert!(batch.desc.contains("trimmed clip #2"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let segs = p.timeline.video_track().unwrap().segments.clone();
    assert_eq!(clip_ids(&p), vec![1, 3, 2]);
    assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::new(1, 2)));
    assert_eq!(
        clip_by_id(&p, 3).duration(),
        Seconds::from_integer(2),
        "pasted"
    );
    let c2 = clip_by_id(&p, 2);
    assert_eq!(c2.asset_in, Seconds::new(3, 2), "trimmed head");
    assert_eq!(c2.duration(), Seconds::new(1, 2));
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5),
        "length unchanged"
    );
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);

    // Fully covering the gap and #2: #2 is removed, the paste grows to
    // fill exactly the space it overwrote.
    let three = Clip {
        id: 0,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(3),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    };
    let batch = paste(&p, &Segment::Clip(three.clone()), 2.0).unwrap();
    assert!(batch.desc.contains("replaced clip #2"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 3]);
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5),
        "length unchanged"
    );
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);

    // Wholly inside clip #1: the clip splits, the right half gets a
    // fresh id, and the paste sits between the halves.
    let half = Clip {
        id: 0,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::new(1, 2),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    };
    let batch = paste(&p, &Segment::Clip(half.clone()), 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 3, 4, 2]);
    assert_eq!(
        clip_by_id(&p, 1).duration(),
        Seconds::from_integer(1),
        "head half"
    );
    assert_eq!(
        (clip_by_id(&p, 4).asset_in, clip_by_id(&p, 4).duration()),
        (Seconds::new(5, 2), Seconds::new(1, 2)),
        "tail half keeps the source tail"
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);

    // Past the end: the void becomes a gap, then the clip; the
    // timeline grows.
    let batch = paste(&p, &Segment::Clip(snapshot.clone()), 6.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert!(
        matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
    );
    assert_eq!(clip_ids(&p), vec![1, 2, 3]);
    assert_eq!(p.timeline.end(), Seconds::from_integer(8));
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn p_ripple_inserts_and_grows() {
    let mut p = fixture();
    let mut h = History::default();
    let snapshot = clip_by_id(&p, 1); // 2s
    let _tiny = Clip {
        id: 0,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::new(1, 2), // 0.5s
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    };

    // mid-clip: split + insert between
    let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 3, 4, 2]);
    assert_eq!(
        clip_by_id(&p, 1).duration(),
        Seconds::from_integer(1),
        "left half"
    );
    assert_eq!(
        clip_by_id(&p, 2).duration(),
        Seconds::from_integer(2),
        "source kept"
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(7), "grew by 2");
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));

    // in a gap: the gap splits, followers slide, timeline grows
    let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 2.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(7), "grew by 2");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));

    // past the end: the void becomes a gap, then the clip
    let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 7.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert!(
        matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(9));
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));

    // at a cut: the paste lands BEFORE the clip; the clip keeps its id
    let batch = paste_ripple(&p, &Segment::Clip(snapshot.clone()), 3.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        clip_by_id(&p, 2).duration(),
        Seconds::from_integer(2),
        "source kept"
    );
    assert_eq!(
        clip_by_id(&p, 3).duration(),
        Seconds::from_integer(2),
        "pasted"
    );
    h.undo(&mut p).unwrap();
}

// A ripple paste at/over a transition is refused, not a panic: a
// transition owns its neighbours' trimmed spans and can't be split.
#[test]
fn ripple_paste_over_a_transition_is_refused() {
    let mut p = fixture(); // [C1 0..2][gap 2..3][C2 3..5]
    let mut h = History::default();
    let batch = add_transition(
        &p,
        0,
        2.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    // The blend owns [1, 2).
    let snapshot = Segment::Clip(clip_by_id(&p, 2));
    for at in [1.0, 1.5, 1.9] {
        assert!(
            matches!(paste_ripple(&p, &snapshot, at), Err(EditError::Transition)),
            "ripple paste at {at} should be refused"
        );
    }
    // …and a paste next to the blend still works.
    assert!(paste_ripple(&p, &snapshot, 0.5).is_ok());
}

// Appending at/past the end never leaves two adjacent gaps: the void
// between the track end and the playhead is one segment with the payload.
#[test]
fn appending_past_the_end_coalesces_gaps() {
    let gap = Segment::Gap(Gap {
        duration: Seconds::from_integer(1),
    });
    let no_adjacent_gaps = |p: &Project| {
        let segs = &p.timeline.video_track().unwrap().segments;
        assert!(
            !segs
                .windows(2)
                .any(|w| matches!(w, [Segment::Gap(_), Segment::Gap(_)])),
            "adjacent gaps: {segs:?}"
        );
    };

    // An inserted gap past the end: the void [5,7) and the 1 s gap merge.
    let mut p = fixture(); // [C1 0..2][gap 2..3][C2 3..5]
    let mut h = History::default();
    let batch = insert_gap(&p, 7.0, 30).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    no_adjacent_gaps(&p);
    assert_eq!(p.timeline.end(), Seconds::from_integer(8), "void + gap");

    // …and a pasted gap payload too.
    let mut p = fixture();
    let mut h = History::default();
    let batch = paste(&p, &gap, 7.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    no_adjacent_gaps(&p);
    assert_eq!(p.timeline.end(), Seconds::from_integer(8));
}

// A ripple paste inside a gap whose start is off the frame grid drops the
// sub-frame remainders instead of stranding them (as `paste_on` does).
#[test]
fn a_ripple_paste_in_an_off_grid_gap_leaves_no_slivers() {
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    let clip = |id, out: Seconds| {
        Segment::Clip(Clip {
            id,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: out,
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    };
    // A clip ending half a frame early, then a 1 s gap: the playhead at
    // 2.0 is one half-frame into it.
    p.timeline.video_track_mut().unwrap().segments = vec![
        clip(1, Seconds::new(119, 60)),
        Segment::Gap(Gap {
            duration: Seconds::from_integer(1),
        }),
        clip(2, Seconds::from_integer(2)),
    ];
    let payload = Segment::Clip(clip_by_id(&p, 2));
    let mut h = History::default();
    let batch = paste_ripple(&p, &payload, 2.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    for s in &p.timeline.video_track().unwrap().segments {
        if let Segment::Gap(g) = s {
            assert!(
                at_least_a_frame(&p, &g.duration).is_ok(),
                "no sub-frame gap: {g:?}"
            );
        }
    }
}

#[test]
fn paste_gap_payloads() {
    // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
    let p = fixture();
    let payload = Segment::Gap(Gap {
        duration: Seconds::from_integer(1),
    });

    // Ripple-paste the gap into clip #1: the clip splits.
    let mut q = p.clone();
    let mut h = History::default();
    let batch = paste_ripple(&q, &payload, 1.0).unwrap();
    assert!(batch.desc.contains("gap inserted"), "{}", batch.desc);
    h.apply(&mut q, batch, 0).unwrap();
    assert!(
        matches!(&q.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
    );
    assert_eq!(
        q.timeline.end(),
        Seconds::from_integer(6),
        "grew by the gap"
    );

    // Ripple-paste into the existing gap: they merge.
    let mut q = p.clone();
    let mut h = History::default();
    let batch = paste_ripple(&q, &payload, 2.5).unwrap();
    h.apply(&mut q, batch, 0).unwrap();
    assert!(
        matches!(&q.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
    );
    assert_eq!(q.timeline.end(), Seconds::from_integer(6));

    // Replace-paste the gap at 1.0 overwrites [1,2) with gap and
    // coalesces with the gap already starting at 2.
    let mut q = p.clone();
    let mut h = History::default();
    let batch = paste(&q, &payload, 1.0).unwrap();
    h.apply(&mut q, batch, 0).unwrap();
    assert!(
        matches!(&q.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2)),
        "coalesced"
    );
    assert_eq!(q.timeline.end(), Seconds::from_integer(5), "length held");

    // Pasting a gap at the END of an existing gap extends it rather
    // than making a second adjacent gap.
    let two = Segment::Gap(Gap {
        duration: Seconds::from_integer(2),
    });
    let mut q = p.clone();
    let mut h = History::default();
    let batch = paste_ripple(&q, &two, 3.0).unwrap();
    h.apply(&mut q, batch, 0).unwrap();
    let segs = q.timeline.video_track().unwrap().segments.clone();
    assert_eq!(segs.len(), 3, "no extra gap segment");
    assert!(
        matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(3)),
        "gap extended"
    );
    assert_eq!(
        q.timeline.end(),
        Seconds::from_integer(7),
        "followers push right"
    );

    // Replace-paste at the gap's end extends it and overwrites the
    // covered span (length held).
    let mut q = p.clone();
    let mut h = History::default();
    let batch = paste(&q, &two, 3.0).unwrap();
    h.apply(&mut q, batch, 0).unwrap();
    let segs = q.timeline.video_track().unwrap().segments.clone();
    assert_eq!(segs.len(), 2, "clip #2 overwritten, no extra gap");
    assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(3)));
    assert_eq!(q.timeline.end(), Seconds::from_integer(5), "length held");
}

#[test]
fn insert_gap_splits_clip_adds_to_gap_and_grows() {
    // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
    let mut p = fixture();
    let mut h = History::default();
    // Over clip #1 at 1.0: split, insert 1 frame (30fps) between.
    let batch = insert_gap(&p, 1.0, 1).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let segs = p.timeline.video_track().unwrap().segments.clone();
    assert_eq!(segs.len(), 5, "clip split + gap inserted");
    assert!(
        matches!(&segs[0], Segment::Clip(c) if c.id == 1 && c.duration() == Seconds::from_integer(1))
    );
    assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::new(1, 30)));
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5) + Seconds::new(1, 30)
    );
    h.undo(&mut p).unwrap();

    // Over the gap at 2.5: the gap grows by the count.
    let batch = insert_gap(&p, 2.5, 15).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert!(
        matches!(&p.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::new(3, 2))
    );
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5) + Seconds::new(1, 2)
    );
    h.undo(&mut p).unwrap();

    // Past the end at 7.0: void gap + one frame.
    let batch = insert_gap(&p, 7.0, 1).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(7) + Seconds::new(1, 30)
    );
    h.undo(&mut p).unwrap();

    // Zero/negative count refuses.
    assert!(matches!(insert_gap(&p, 1.0, 0), Err(EditError::TooShort)));
}

// `_` at a gap's far edge (or a clip head just after a gap) grows that
// gap instead of spawning a second, adjacent one.
#[test]
fn insert_gap_merges_into_an_adjacent_gap() {
    // [Clip #1 [0..2)] [Gap 1 @2..3] [Clip #2 [3..5)]: `_` at C2's head.
    let mut p = fixture();
    let mut h = History::default();
    let batch = insert_gap(&p, 3.0, 5).unwrap();
    assert!(batch.desc.contains("insert gap"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let segs = p.timeline.video_track().unwrap().segments.clone();
    assert_eq!(segs.len(), 3, "no extra gap segment");
    assert!(
        matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1) + Seconds::new(5, 30))
    );
    assert_eq!(
        p.timeline.end(),
        Seconds::from_integer(5) + Seconds::new(5, 30),
        "followers shifted"
    );
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);

    // Trailing gap: `_` at the track end grows it, no new segment.
    let mut q = fixture();
    q.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(Segment::Gap(Gap {
            duration: Seconds::from_integer(3),
        })); // trailing [5..8)
    let mut h = History::default();
    let batch = insert_gap(&q, 8.0, 1).unwrap();
    h.apply(&mut q, batch, 0).unwrap();
    let segs = q.timeline.video_track().unwrap().segments.clone();
    assert_eq!(segs.len(), 4, "no extra gap segment");
    assert!(
        matches!(&segs[3], Segment::Gap(g) if g.duration == Seconds::from_integer(3) + Seconds::new(1, 30))
    );
}
