use super::super::*;
use super::*;

#[test]
fn i_trims_clip_head_and_leaves_a_gap() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = trim_in(&p, 1.0).unwrap();
    assert!(batch.desc.contains("trim in"), "{}", batch.desc);
    h.apply(&mut p, batch, 0).unwrap();
    // [Gap 1 @0..1] [Clip #1 [1..2) src 2..3] [Gap 1 @2..3] [Clip #2]
    let track = p.timeline.video_track().unwrap();
    assert!(
        matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
    );
    let c = clip_by_id(&p, 1);
    assert_eq!(c.asset_in, Seconds::from_integer(2));
    assert_eq!(c.duration(), Seconds::from_integer(1));
    assert_eq!(clip_ids(&p), vec![1, 2]);
    assert_eq!(
        clip_by_id(&p, 2).duration(),
        Seconds::from_integer(2),
        "follower unmoved"
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 1).asset_in, Seconds::from_integer(1));
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 3);
}

#[test]
fn o_trims_clip_tail_and_leaves_a_gap() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = trim_out(&p, 4.0).unwrap(); // mid clip #2 [3..5)
    h.apply(&mut p, batch, 0).unwrap();
    let c = clip_by_id(&p, 2);
    assert_eq!(c.asset_out, Seconds::from_integer(1), "src 0..1");
    assert_eq!(c.duration(), Seconds::from_integer(1));
    // the trimmed material's gap sits AFTER the clip (index 3)
    let track = p.timeline.video_track().unwrap();
    assert!(
        matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
    );
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 2).asset_out, Seconds::from_integer(2));
}

#[test]
fn i_against_gap_grows_it_instead_of_splitting() {
    let mut p = fixture(); // [Clip #1] [Gap 1] [Clip #2 [3..5)]
    let mut h = History::default();
    let batch = trim_in(&p, 4.0).unwrap(); // 1s off clip #2's head
    h.apply(&mut p, batch, 0).unwrap();
    // no new segment: the existing gap absorbed the trimmed second
    let track = p.timeline.video_track().unwrap();
    assert_eq!(track.segments.len(), 3);
    assert!(
        matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
    );
    let c = clip_by_id(&p, 2);
    assert_eq!(c.asset_in, Seconds::from_integer(1));
    assert_eq!(c.duration(), Seconds::from_integer(1));
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
    h.undo(&mut p).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert!(
        matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
    );
    assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(0));
}

#[test]
fn o_against_gap_grows_it_instead_of_splitting() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = trim_out(&p, 1.0).unwrap(); // 1s off clip #1's tail
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert_eq!(track.segments.len(), 3);
    assert!(
        matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(2))
    );
    let c = clip_by_id(&p, 1);
    assert_eq!(c.asset_out, Seconds::from_integer(2));
    assert_eq!(p.timeline.end(), Seconds::from_integer(5), "end preserved");
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
}

#[test]
fn i_ripple_against_gap_leaves_no_gap_trace() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = trim_in_ripple(&p, 4.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    // only the clip shortened: the neighboring gap is untouched
    let track = p.timeline.video_track().unwrap();
    assert_eq!(track.segments.len(), 3);
    assert!(
        matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
    );
    assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(1));
    assert_eq!(p.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn o_ripple_against_gap_leaves_no_gap_trace() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = trim_out_ripple(&p, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert_eq!(track.segments.len(), 3);
    assert!(
        matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::from_integer(1))
    );
    assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(1));
    assert_eq!(p.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn i_against_clip_still_inserts_a_gap() {
    // [Clip #1] [Clip #2], no gap between: trimming still splits.
    let mut p = fixture();
    p.timeline.video_track_mut().unwrap().segments.remove(1);
    let mut h = History::default();
    let batch = trim_in(&p, 3.5).unwrap(); // 1.5 off clip #2's head
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert_eq!(track.segments.len(), 3);
    assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::new(3, 2)));
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.video_track().unwrap().segments.len(), 2);
}

#[test]
fn i_ripple_shrinks_and_slides_followers() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = trim_in_ripple(&p, 1.5).unwrap(); // mid clip #1
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        clip_by_id(&p, 1).duration(),
        Seconds::new(1, 2),
        "head trimmed 1.5"
    );
    assert_eq!(
        clip_by_id(&p, 2).duration(),
        Seconds::from_integer(2),
        "source kept"
    );
    assert_eq!(p.timeline.end(), Seconds::new(7, 2), "shrunk by 1.5");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn o_ripple_shrinks_and_slides_followers() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = trim_out_ripple(&p, 4.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_by_id(&p, 2).duration(), Seconds::from_integer(1));
    assert_eq!(p.timeline.end(), Seconds::from_integer(4), "shrunk by 1");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
}

#[test]
fn i_on_gap_extends_prev_clip_tail() {
    let mut p = fixture(); // gap [2..3)
    let mut h = History::default();
    let batch = trim_in(&p, 2.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let c = clip_by_id(&p, 1);
    assert_eq!(
        c.asset_out,
        Seconds::new(7, 2),
        "tail grew 0.5 into the gap"
    );
    // the region's remaining half stays a gap
    let track = p.timeline.video_track().unwrap();
    assert!(matches!(&track.segments[1], Segment::Gap(g) if g.duration == Seconds::new(1, 2)));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 1).asset_out, Seconds::from_integer(3));
}

#[test]
fn o_on_gap_extends_next_clip_head() {
    let mut p = fixture();
    // clip #2's head needs source before its in-point: src 1..3
    p.timeline.video_track_mut().unwrap().segments[2] = Segment::Clip(Clip {
        id: 2,
        asset: 1,
        asset_in: Seconds::from_integer(1),
        asset_out: Seconds::from_integer(3),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let mut h = History::default();
    let batch = trim_out(&p, 2.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let c = clip_by_id(&p, 2);
    assert_eq!(c.asset_in, Seconds::new(1, 2), "head grew 0.5 earlier");
    assert_eq!(c.duration(), Seconds::new(5, 2));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 2).asset_in, Seconds::from_integer(1));
}

#[test]
fn o_on_gap_without_head_material_is_rejected() {
    let p = fixture(); // clip #2 src 0..2 — nothing before 0
    assert!(matches!(
        trim_out(&p, 2.5),
        Err(EditError::BeforeAsset { .. })
    ));
}

#[test]
fn o_on_trailing_gap_shortens_the_tail() {
    let mut p = fixture();
    p.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(Segment::Gap(Gap {
            duration: Seconds::from_integer(3),
        })); // trailing [5..8)
    let mut h = History::default();
    let batch = trim_out(&p, 6.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert!(matches!(&track.segments[3], Segment::Gap(g) if g.duration == Seconds::new(3, 2)));
    assert_eq!(p.timeline.end(), Seconds::new(13, 2), "6.5");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(8));
}

#[test]
fn i_on_leading_gap_shortens_from_the_front() {
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
    ];
    let mut h = History::default();
    let batch = trim_in(&p, 1.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let track = p.timeline.video_track().unwrap();
    assert!(matches!(&track.segments[0], Segment::Gap(g) if g.duration == Seconds::new(1, 2)));
    assert_eq!(
        p.timeline.end(),
        Seconds::new(5, 2),
        "everything slid left 1.5"
    );
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(4));
}

#[test]
fn ripple_gap_trims_shrink_without_growing_neighbours() {
    // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
    let mut p = fixture();
    let mut h = History::default();
    // In-edge at 2.5: remove 0.5 from the gap; clip #1 is untouched.
    let batch = ripple_gap_in(&p, 2.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        clip_by_id(&p, 1).duration(),
        Seconds::from_integer(2),
        "clip #1 intact"
    );
    assert!(
        matches!(&p.timeline.video_track().unwrap().segments[1], Segment::Gap(g) if g.duration == Seconds::new(1, 2))
    );
    assert_eq!(p.timeline.end(), Seconds::new(9, 2), "shrunk by 0.5");
    h.undo(&mut p).unwrap();
    assert_eq!(p.timeline.end(), Seconds::from_integer(5));
    // Out-edge at 2.5: same net shrink, from the back.
    let batch = ripple_gap_out(&p, 2.5).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_by_id(&p, 1).duration(), Seconds::from_integer(2));
    assert_eq!(p.timeline.end(), Seconds::new(9, 2));
    // Not on a gap → NoGap; at the gap's start → nothing to remove.
    assert!(matches!(ripple_gap_in(&p, 0.5), Err(EditError::NoGap)));
    let fresh = fixture();
    assert!(matches!(
        ripple_gap_in(&fresh, 2.0),
        Err(EditError::NothingToTrim(_))
    ));
}

#[test]
fn trim_consume_target_and_delete_segment() {
    // [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5)]
    let p = fixture();
    // `o` targets the segment STARTING at the playhead.
    assert_eq!(trim_consume_target(&p, 2.0, true), Some(1)); // the gap
    assert_eq!(trim_consume_target(&p, 3.0, true), Some(2)); // clip #2
    // `i` targets the segment ENDING there.
    assert_eq!(trim_consume_target(&p, 2.0, false), Some(0)); // clip #1
    assert_eq!(trim_consume_target(&p, 3.0, false), Some(1)); // the gap
    // Not at an edge → no consume.
    assert_eq!(trim_consume_target(&p, 1.0, true), None);
    assert_eq!(trim_consume_target(&p, 1.0, false), None);
    assert_eq!(
        trim_consume_target(&p, 0.0, false),
        None,
        "nothing to the left"
    );

    // A plain clip delete leaves a gap (merged with the neighbour);
    // a ripple delete closes; a gap always ripples.
    let mut q = fixture();
    let mut h = History::default();
    let batch = delete_segment(&q, 0, false).unwrap();
    h.apply(&mut q, batch, 0).unwrap();
    assert_eq!(q.timeline.end(), Seconds::from_integer(5), "length held");
    assert!(
        matches!(&q.timeline.video_track().unwrap().segments[0], Segment::Gap(g) if g.duration == Seconds::from_integer(3))
    );
    h.undo(&mut q).unwrap();
    let batch = delete_segment(&q, 0, true).unwrap();
    h.apply(&mut q, batch, 0).unwrap();
    assert_eq!(
        q.timeline.end(),
        Seconds::from_integer(3),
        "shrunk by clip #1"
    );
    assert!(matches!(
        &q.timeline.video_track().unwrap().segments[0],
        Segment::Gap(_)
    ));
}
