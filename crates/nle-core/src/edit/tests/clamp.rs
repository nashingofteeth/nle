use super::super::*;
use super::*;

#[test]
fn split_sides_refuses_sub_frame_pieces() {
    let p = fixture(); // fps 30
    let frame = Seconds::new(1, 30);
    let dur = Seconds::from_integer(2); // 60 frames
    // A quarter frame at either edge counts as "at that edge", so no
    // sliver. (Half a frame rounds up to a frame and is kept.)
    assert_eq!(split_sides(&p, dur, frame / 4), (false, true));
    assert_eq!(split_sides(&p, dur, dur - frame / 4), (true, false));
    // A whole-frame split leaves both pieces.
    assert_eq!(split_sides(&p, dur, frame * 10), (true, true));
}

#[test]
fn purge_stumps_snaps_the_grid_and_drops_sub_frame_segments() {
    let mut p = fixture();
    p.fps = Some(Seconds::new(30, 1));
    let t = p.timeline.video_track_mut().unwrap();
    t.segments = vec![
        // 60.4 frames of clip -> snaps to 60
        Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::new(604, 300),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        // a 0.3-frame stump -> dropped
        Segment::Clip(Clip {
            id: 2,
            asset: 1,
            asset_in: Seconds::from_integer(5),
            asset_out: Seconds::new(501, 100),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        // 30.6 frames of gap -> snaps to 31
        Segment::Gap(Gap {
            duration: Seconds::new(51, 50),
        }),
    ];
    let (cleaned, dropped) = purge_stumps(p.timeline.video_track().unwrap(), Seconds::new(30, 1));
    assert_eq!(dropped, 1);
    assert_eq!(cleaned.len(), 2);
    let Segment::Clip(c) = &cleaned[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.asset_out, Seconds::from_integer(2), "60 frames");
    let Segment::Gap(g) = &cleaned[1] else {
        panic!("expected gap")
    };
    assert_eq!(g.duration, Seconds::new(31, 30), "31 frames");
}

#[test]
fn clamp_to_media_trims_or_drops_past_the_end() {
    let fps = Seconds::from_integer(30);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![
            Segment::Clip(Clip::new(
                1,
                1,
                Seconds::from_integer(0),
                Seconds::from_integer(3),
            )),
            Segment::Clip(Clip::new(
                2,
                1,
                Seconds::from_integer(1),
                Seconds::from_integer(4),
            )),
        ],
    };
    // Media is 2s: both out-points overrun and are clamped onto whole
    // frames inside it.
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
    assert_eq!(notes.len(), 2, "{notes:?}");
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.asset_out, Seconds::from_integer(2));

    // A clip wholly past the media end is dropped.
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip::new(
            3,
            1,
            Seconds::from_integer(5),
            Seconds::from_integer(8),
        ))],
    };
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(t.segments.is_empty());
}

#[test]
fn clamp_to_media_floors_when_rounding_would_pass_the_media_end() {
    let fps = Seconds::from_integer(30);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip::new(
            1,
            1,
            Seconds::from_integer(2),
            Seconds::from_integer(4),
        ))],
    };
    // 3.49s of media: rounding the duration up to 45 frames would reach
    // 3.5s — past the file — so it floors to 44 instead.
    clamp_to_media(&mut t, &|_| Some(Seconds::new(349, 100)), fps);
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.asset_out - c.asset_in, Seconds::new(44, 30));
    assert!(c.asset_out <= Seconds::new(349, 100));
}

#[test]
fn clamp_to_media_clamps_a_negative_in_point() {
    let fps = Seconds::from_integer(30);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::new(-1, 2), // -0.5s: before the file start
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })],
    };
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(3)), fps);
    assert_eq!(notes.len(), 1, "{notes:?}");
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    assert_eq!(c.asset_in, Seconds::from_integer(0));
    assert_eq!(c.asset_out, Seconds::from_integer(2));
}

#[test]
fn clamp_to_media_scales_by_speed() {
    let fps = Seconds::from_integer(30);
    // A 2× clip: 4s of source occupies 2s of timeline. Media is 3.44s.
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::from_integer(2),
            effects: Vec::new(),
        })],
    };
    clamp_to_media(&mut t, &|_| Some(Seconds::new(344, 100)), fps);
    let Segment::Clip(c) = &t.segments[0] else {
        panic!("expected clip")
    };
    // The timeline duration must land on whole frames (51), and the
    // clamped out-point must stay inside the media.
    assert_eq!(c.duration(), Seconds::new(51, 30));
    assert_eq!(c.asset_out, Seconds::new(34, 10));
    assert!(c.asset_out <= Seconds::new(344, 100));
}

#[test]
fn clamp_to_media_ignores_nonpositive_speed() {
    let fps = Seconds::from_integer(30);
    let mut t = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![Segment::Clip(Clip {
            id: 1,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(3),
            speed: Seconds::from_integer(0),
            effects: Vec::new(),
        })],
    };
    // No panic, no change — `validate()` is the authority on bad speed.
    let notes = clamp_to_media(&mut t, &|_| Some(Seconds::from_integer(2)), fps);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(t.segments.len(), 1);
}
