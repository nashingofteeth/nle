use super::super::*;
use super::*;

#[test]
fn speed_change_always_ripples() {
    let mut p = fixture(); // #1 [0,2), gap 1s, #2 [0,2)
    let mut h = History::default();
    // Slow #1 to 0.5x: its 2s of source now occupies 4s of timeline, so
    // the gap and #2 slide right by 2s and the track grows.
    let batch = set_speed(&p, 1, Seconds::new(1, 2)).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let t = p.timeline.video_track().unwrap();
    assert_eq!(t.segments[0].duration(), Seconds::from_integer(4));
    assert_eq!(t.start_of(1), Seconds::from_integer(4));
    assert_eq!(t.end(), Seconds::from_integer(7));
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.source_duration(), Seconds::from_integer(2)); // untouched
    assert_eq!(c.speed, Seconds::new(1, 2));
}

#[test]
fn speed_round_trips_through_undo() {
    let mut p = fixture();
    let mut h = History::default();
    let batch = set_speed(&p, 2, Seconds::from_integer(2)).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_by_id(&p, 2).speed, Seconds::from_integer(2));
    h.undo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 2).speed, Seconds::from_integer(1));
    h.redo(&mut p).unwrap();
    assert_eq!(clip_by_id(&p, 2).speed, Seconds::from_integer(2));
}

#[test]
fn split_maps_timeline_to_source_at_speed() {
    // A 2x clip reading source [0,4) occupies timeline [0,2).
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
        id: 1,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(4),
        speed: Seconds::from_integer(2),
        effects: Vec::new(),
    })];
    let mut h = History::default();
    let batch = split(&p, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let segs = &p.timeline.video_track().unwrap().segments;
    let (Segment::Clip(l), Segment::Clip(r)) = (&segs[0], &segs[1]) else {
        panic!("expected two clips")
    };
    // 1s of timeline at 2x = 2s of source
    assert_eq!(l.asset_out, Seconds::from_integer(2));
    assert_eq!(r.asset_in, Seconds::from_integer(2));
    assert_eq!(l.duration(), Seconds::from_integer(1));
    assert_eq!(r.duration(), Seconds::from_integer(1));
    assert_eq!(l.speed, Seconds::from_integer(2));
}

#[test]
fn trim_scales_timeline_delta_into_source() {
    // A 0.5x clip reading source [0,4) occupies timeline [0,8).
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
        id: 1,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(4),
        speed: Seconds::new(1, 2),
        effects: Vec::new(),
    })];
    let mut h = History::default();
    // trim the head to timeline 2.0: 2s of timeline = 1s of source
    let batch = trim_in(&p, 2.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    assert_eq!(clip_by_id(&p, 1).asset_in, Seconds::from_integer(1));
    // the trimmed head became a 2s gap (timeline space)
    assert_eq!(
        p.timeline.video_track().unwrap().segments[0],
        Segment::Gap(Gap {
            duration: Seconds::from_integer(2)
        })
    );
}

#[test]
fn set_speed_rejects_non_positive() {
    let p = fixture();
    assert!(matches!(
        set_speed(&p, 1, Seconds::from_integer(0)),
        Err(EditError::BadSpeed(_))
    ));
    assert!(matches!(
        set_speed(&p, 1, Seconds::new(-1, 2)),
        Err(EditError::BadSpeed(_))
    ));
}

/// A rate so small that the quotient overflows the i64 rational is
/// refused — not wrapped (release) or panicked on (debug). The hostile
/// battery's `speed 1/i64::MAX` case found this.
#[test]
fn set_speed_refuses_a_rate_it_cannot_represent() {
    let p = clip_pair(); // a 3 s clip
    assert!(matches!(
        set_speed(&p, 1, Seconds::new(1, i64::MAX)),
        Err(EditError::TooLong(_))
    ));
}

#[test]
fn set_speed_refuses_a_sub_frame_span() {
    // A 2-frame source clip at 1/30fps retimed to 6× occupies 1/3 frame
    // of timeline: below one frame, so it must be refused — same rule
    // trims and splits enforce. 2× (1 frame exactly) is allowed.
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
        id: 1,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::new(2, 30),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    assert!(matches!(
        set_speed(&p, 1, Seconds::from_integer(6)),
        Err(EditError::TooShort)
    ));
    let batch = set_speed(&p, 1, Seconds::from_integer(2)).unwrap(); // exactly 1 frame
    History::default().apply(&mut p, batch, 0).unwrap();
    assert_eq!(
        clip_by_id(&p, 1).duration(),
        Seconds::new(1, 30),
        "2 frames at 2x = 1 frame"
    );

    // The case rounding used to hide: 0.9 frames is *not* a frame.
    // 3 s of source at 100× is 0.03 s = 0.9 frames at 30 fps.
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
        id: 1,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(3),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    assert!(matches!(
        set_speed(&p, 1, Seconds::from_integer(100)),
        Err(EditError::TooShort)
    ));
}

/// A rate is a rate, but the *timeline* duration it implies lands on the
/// frame grid. The source span stays exactly as selected and the rate
/// absorbs the snap, so the model and the render can't disagree.
#[test]
fn set_speed_lands_the_duration_on_the_frame_grid() {
    let fps = Seconds::from_integer(30);
    let frames = |d: Seconds| (d * fps).to_integer();
    let mut p = fixture(); // [C1 src 1..3] [gap 1] [C2 src 0..2] @30 fps
    let mut h = History::default();
    let batch = set_speed(&p, 1, Seconds::new(7, 10)).unwrap();
    h.apply(&mut p, batch, 0).unwrap();

    let c = clip_by_id(&p, 1);
    assert_eq!(frames(c.duration()), 86, "2 s / 0.7 = 2.857 s → 86 frames");
    assert_eq!(
        c.asset_out,
        Seconds::from_integer(3),
        "the selected source span is untouched"
    );
    assert_eq!(
        c.speed,
        Seconds::new(30, 43),
        "…and the rate is source / duration = 2 s / (86/30 s)"
    );
    for seg in &p.timeline.video_track().unwrap().segments {
        assert!(
            (seg.duration() * fps).is_integer(),
            "a segment is off the grid: {:?}",
            seg.duration()
        );
    }

    // No frame is claimed past the media either: a clip that already reads
    // to the end of its asset slows down as-is (the span does not grow).
    let mut p = Project::new();
    p.fps = Some(fps);
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("3"));
    p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
        id: 1,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(3), // exactly the media
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let mut h = History::default();
    let batch = set_speed(&p, 1, Seconds::new(7, 10)).unwrap();
    h.apply(&mut p, batch, 0).unwrap();
    let c = clip_by_id(&p, 1);
    // 3 / 0.7 = 4.2857 s = 128.57 frames → 129 frames at 30/43.
    assert_eq!(frames(c.duration()), 129);
    assert_eq!(c.asset_out, Seconds::from_integer(3));
    assert_eq!(c.speed, Seconds::new(30, 43));
}
