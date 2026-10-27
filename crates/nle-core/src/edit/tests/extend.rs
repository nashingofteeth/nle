use super::super::*;
use super::*;
use crate::timeline::{Clip, Gap, Segment};

#[test]
fn extend_tail_grows_clip_and_followers_shift() {
    let mut p = fixture(); // [Clip #1 [0..2)] [Gap 1] [Clip #2 [3..5)]
    let mut h = History::default();
    // extend tail of #1 to 2.5 (asset has material to 10): gap stays,
    // followers shift right, timeline grows by 0.5
    let batch = extend_edge(&p, 1, false, 2.5).unwrap();
    assert!(
        batch.desc.contains("extend tail of clip #1"),
        "{}",
        batch.desc
    );
    h.apply(&mut p, batch, 0).unwrap();
    let c = clip_by_id(&p, 1);
    assert_eq!(c.asset_out, Seconds::new(7, 2));
    let track = p.timeline.video_track().unwrap();
    assert!(
        matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
    );
    assert_eq!(p.timeline.end(), Seconds::new(11, 2));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn extend_tail_past_end_grows_timeline_from_material() {
    let mut p = fixture();
    p.timeline.video_track_mut().unwrap().segments.pop(); // drop clip #2
    p.timeline.video_track_mut().unwrap().segments.pop(); // drop the gap: [#1 [0..2)]
    let mut h = History::default();
    let batch = extend_edge(&p, 1, false, 4.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(5));
    assert_eq!(p.timeline.end(), Seconds::from_integer(4));
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(2));
}

#[test]
fn extend_tail_refuses_without_material() {
    let p = fixture(); // asset ends at 10; clip #1 out is 3
    assert!(matches!(
        extend_edge(&p, 1, false, 12.0),
        Err(EditError::BeyondAsset { .. })
    ));
    // wrong side is not an extend
    assert!(matches!(
        extend_edge(&p, 1, false, 1.0),
        Err(EditError::NothingToTrim(_))
    ));
}

#[test]
fn extend_head_reaches_earlier_material() {
    let mut p = fixture();
    // clip #2 src 0..2 has nothing before 0: give it headroom first
    p.timeline.video_track_mut().unwrap().segments[2] = Segment::Clip(Clip {
        id: 2,
        asset: 1,
        asset_in: Seconds::from_integer(1),
        asset_out: Seconds::from_integer(3),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let mut h = History::default();
    let batch = extend_edge(&p, 2, true, 2.5).unwrap();
    assert!(
        batch.desc.contains("extend head of clip #2"),
        "{}",
        batch.desc
    );
    h.apply(&mut p, batch, 0).unwrap();
    let c = clip_by_id(&p, 2);
    assert_eq!(c.asset_in, Seconds::new(1, 2));
    assert_eq!(p.timeline.end(), Seconds::new(11, 2));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(1));
}

#[test]
fn extend_head_refuses_without_material() {
    let p = fixture(); // clip #2 src 0..2 — nothing before 0
    assert!(matches!(
        extend_edge(&p, 2, true, 2.5),
        Err(EditError::BeforeAsset { .. })
    ));
    assert!(matches!(
        extend_edge(&p, 2, true, 4.0),
        Err(EditError::NothingToTrim(_))
    ));
    assert!(matches!(
        extend_edge(&p, 99, true, 1.0),
        Err(EditError::ClipNotFound(99))
    ));
}

#[test]
fn extend_gap_end_lengthens_trailing_gap() {
    let mut p = fixture();
    p.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(Segment::Gap(Gap {
            duration: Seconds::from_integer(3),
        })); // trailing [5..8)
    let mut h = History::default();
    let batch = extend_gap_end(&p, 10.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert!(
        matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(5))
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(10));
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(8));
    // not past the gap start, and no trailing gap at all, both refuse
    assert!(matches!(
        extend_gap_end(&p, 4.0),
        Err(EditError::NothingToTrim(_))
    ));
    let q = fixture(); // ends with a clip
    assert!(matches!(extend_gap_end(&q, 6.0), Err(EditError::NoGap)));
}

#[test]
fn extend_gap_tail_moves_any_gap() {
    let mut p = fixture(); // [Clip #1 [0..2)] [Gap 1 @2..3] [Clip #2 [3..5)]
    let mut h = History::default();
    // Ripple (E): grow to [2..4), clip #2 shifts right, timeline grows.
    let batch = extend_gap_tail(&p, 1, 4.0, true).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert!(
        matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(6));
    assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));

    // Replace (e): grow to [2..4.5) overwrites clip #2's head — it is
    // trimmed, not shifted, and the timeline length is preserved.
    let batch = extend_gap_tail(&p, 1, 4.5, false).unwrap();
    assert!(batch.desc.contains("trimmed clip #2"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    assert!(
        matches!(&p.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::new(5, 2))
    );
    assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::new(3, 2));
    assert_eq!(clip_by_id(&p, 2).duration(), Seconds::new(1, 2));
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "length held");
    h.undo(&mut p).unwrap();

    // Shrink to [2..2.5): clip #2 slides left either way.
    let batch = extend_gap_tail(&p, 1, 2.5, false).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(p.timeline.end(), Seconds::new(9, 2));
    // Not a gap / at/before its start both refuse.
    assert!(matches!(
        extend_gap_tail(&p, 0, 1.0, false),
        Err(EditError::NoGap)
    ));
    assert!(matches!(
        extend_gap_tail(&p, 1, 0.0, true),
        Err(EditError::NothingToTrim(_))
    ));
}

#[test]
fn notices_use_frame_stamps() {
    let p = fixture(); // 30 fps
    let b = split(&p, 1.0).unwrap();
    assert!(b.desc.contains("at f30"), "{}", b.desc);
    // Errors too: at the clip's edge there is nothing to trim.
    let e = trim_in(&p, 0.0).unwrap_err();
    assert!(e.to_string().contains("f0"), "{e}");
    assert!(!e.to_string().contains("0.000000"), "{e}");
}

#[test]
fn extend_gap_head_replaces_the_previous_segment() {
    let mut p = fixture(); // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
    let mut h = History::default();
    // Grow the gap's head to 1.0: clip #1 is trimmed to [0..1).
    let batch = extend_gap_head(&p, 1, 1.0, false).unwrap();
    assert!(batch.desc.contains("trimmed clip #1"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    let segs = &p.timeline.video_track().unwrap().segments;
    assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(1));
    assert!(matches!(&segs[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2)));
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "length held");
    h.undo(&mut p).unwrap();
    // Grow to 0.0: clip #1 is absorbed whole.
    let batch = extend_gap_head(&p, 1, 0.0, false).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert!(
        matches!(&p.timeline.video_track().unwrap().segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(3))
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    // Rightward (at/past the start) refuses; a clip is not a gap.
    let fresh = fixture();
    assert!(matches!(
        extend_gap_head(&fresh, 1, 2.0, false),
        Err(EditError::NothingToTrim(_))
    ));
    assert!(matches!(
        extend_gap_head(&fresh, 1, 3.0, false),
        Err(EditError::NothingToTrim(_))
    ));
    assert!(matches!(
        extend_gap_head(&fresh, 0, 1.0, false),
        Err(EditError::NoGap)
    ));

    // Ripple: insert space at the head — clip #1 is untouched, the gap
    // grows, and clip #2 shifts right.
    let mut q = fixture();
    let mut h2 = History::default();
    let batch = extend_gap_head(&q, 1, 1.0, true).unwrap();
    assert!(batch.desc.contains("insert gap"), "{}", batch.desc);
    h2.apply(&mut q, batch, 0).unwrap();
    assert_eq!(
        clip_by_id(&q, 1).duration(),
        Seconds::from_integer(2),
        "clip #1 kept"
    );
    assert!(
        matches!(&q.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
    );
    assert_eq!(q.timeline.end(), Seconds::from_integer(6), "grew by 1");

    // Inward (head moved right): ripples — the gap shortens and clip #1
    // is untouched.
    let mut r = fixture();
    let mut h3 = History::default();
    let batch = extend_gap_head(&r, 1, 2.5, false).unwrap();
    assert!(batch.desc.contains("ripple trim gap in"), "{}", batch.desc);
    h3.apply(&mut r, batch, 0).unwrap();
    assert_eq!(
        clip_by_id(&r, 1).duration(),
        Seconds::from_integer(2),
        "clip #1 kept"
    );
    assert_eq!(r.timeline.end(), Seconds::new(9, 2));
}

#[test]
fn absorb_over_gap_and_clip_removes_and_trims() {
    // [Clip #1 [0..2) src 1..3] [Gap 1] [Clip #2 [3..5) src 0..2],
    // asset is 10s: extend tail of #1 to 4.0.
    let mut p = fixture();
    let mut h = History::default();
    let batch = absorb_outward(&p, 1, false, 4.0).unwrap();
    assert!(batch.desc.contains("absorbed"), "{}", batch.desc);
    assert!(batch.desc.contains("trimmed clip #2"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    // #1 spans [0..4), the gap is gone, #2 keeps [4..5) src 1..2.
    assert_eq!(clip_ids(&p), vec![1, 2]);
    assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(5));
    let c2 = clip_by_id(&p, 2);
    assert_eq!(c2.asset_in, Seconds::from_integer(1));
    assert_eq!(c2.duration(), Seconds::from_integer(1));
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);
    assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
    assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);
}

#[test]
fn absorb_over_two_gaps_and_a_clip() {
    // [Clip #1 [0..2)] [Gap 1] [Clip #2 [3..5)] [Gap 1] [Clip #3 [6..8)]:
    // extend tail of #1 to 6.5 absorbs both gaps, removes #2, trims #3.
    let mut p = fixture();
    {
        let track = p.timeline.video_track_mut().unwrap();
        track.segments.push(Segment::Gap(Gap {
            duration: Seconds::from_integer(1),
        }));
        track.segments.push(Segment::Clip(Clip {
            id: 3,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }));
    }
    let mut h = History::default();
    let batch = absorb_outward(&p, 1, false, 6.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 3]);
    assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::new(15, 2));
    assert_eq!(clip_by_id(&p, 3).asset_in, Seconds::new(1, 2));
    assert_eq!(p.timeline.end(), Seconds::from_integer(8));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2, 3]);
    assert_eq!(p.timeline.end(), Seconds::from_integer(8));
}

#[test]
fn absorb_head_mirrors_tail() {
    // Extend head of #2 left to 1.0: absorbs the gap, trims nothing
    // else (#1 ends at 2.0 > 1.0, so #1 is only... wait: covered is
    // [1.0, 3.0): the gap [2..3) fully, #1 [0..2) straddled -> trim
    // #1's tail to 1.0.
    let mut p = fixture();
    // clip #2 needs 2s of head material for the reach to 1.0.
    p.timeline.video_track_mut().unwrap().segments[2] = Segment::Clip(Clip {
        id: 2,
        asset: 1,
        asset_in: Seconds::from_integer(2),
        asset_out: Seconds::from_integer(4),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let mut h = History::default();
    let batch = absorb_outward(&p, 2, true, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
    let c1 = clip_by_id(&p, 1);
    assert_eq!(c1.asset_out, Seconds::from_integer(2), "trimmed tail");
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
    assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(2));
}

#[test]
fn absorb_sub_frame_remainder_removes_whole() {
    // A straddled follower left with under a frame is absorbed whole
    // instead of leaving a sliver. Reachable via hand-placed
    // off-grid ends (snapped positions always leave whole frames).
    let mut p = fixture();
    p.timeline.video_track_mut().unwrap().segments[2] = Segment::Clip(Clip {
        id: 2,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::new(99, 50),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    }); // C2 [3..4.98)
    let mut h = History::default();
    // 4.97 snaps to frame 149 (4.9667), leaving a 0.013s stump:
    // under half a 30fps frame, so #2 is absorbed whole.
    let batch = absorb_outward(&p, 1, false, 4.97).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_ids(&p), vec![1]);
    assert_eq!(p.timeline.end(), Seconds::new(149, 30));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_ids(&p), vec![1, 2]);
}

#[test]
fn absorb_validates_direction_and_material() {
    let p = fixture();
    // inward is not an extend
    assert!(matches!(
        absorb_outward(&p, 1, false, 1.0),
        Err(EditError::NothingToTrim(_))
    ));
    assert!(matches!(
        absorb_outward(&p, 2, true, 6.0),
        Err(EditError::NothingToTrim(_))
    ));
    // past the asset end refuses
    assert!(matches!(
        absorb_outward(&p, 1, false, 12.0),
        Err(EditError::BeyondAsset { .. })
    ));
    assert!(matches!(
        absorb_outward(&p, 99, false, 3.0),
        Err(EditError::ClipNotFound(99))
    ));
}
