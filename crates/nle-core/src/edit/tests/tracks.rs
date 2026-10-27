use super::super::*;
use super::*;
use crate::timeline::{Gap, Segment};

#[test]
fn track_ops_target_the_chosen_track_only() {
    let mut p = two_track_fixture();
    let video_before = p.timeline.tracks[0].segments.clone();

    let mut h = History::default();
    let batch = trim_in_on(&p, 1, 1.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();

    assert_eq!(
        p.timeline.tracks[0].segments, video_before,
        "the video track is untouched"
    );
    // Audio track: [Gap 1][Clip #10 src 1..4]
    let audio = &p.timeline.tracks[1].segments;
    assert!(
        matches!(audio[0], Segment::Gap(_)),
        "the trimmed head became a gap"
    );
    let c = match &audio[1] {
        Segment::Clip(c) => c.clone(),
        Segment::Gap(_) | Segment::Transition(_) => panic!("expected the trimmed clip"),
    };
    assert_eq!((c.id, c.asset_in, c.asset_out), (10, 1.into(), 4.into()));

    // The history entry is track-scoped: undo restores only track 1.
    h.undo(&mut p).unwrap();
    assert!(matches!(
        &p.timeline.tracks[1].segments[0],
        Segment::Clip(c) if c.id == 10 && c.asset_in == Seconds::from_integer(0)
    ));
}

#[test]
fn split_on_edits_the_chosen_track() {
    let mut p = two_track_fixture();
    let video_before = p.timeline.tracks[0].segments.clone();

    let mut h = History::default();
    let batch = split_on(&p, 1, 2.0).unwrap();
    h.apply(&mut p, batch, 0).unwrap();

    assert_eq!(p.timeline.tracks[0].segments, video_before);
    let audio = &p.timeline.tracks[1].segments;
    assert_eq!(audio.len(), 2, "the audio clip split in two");
    match (&audio[0], &audio[1]) {
        (Segment::Clip(l), Segment::Clip(r)) => {
            assert_eq!((l.id, l.asset_in, l.asset_out), (10, 0.into(), 2.into()));
            assert_eq!((r.asset_in, r.asset_out), (2.into(), 4.into()));
            assert_ne!(r.id, 10, "the right half gets a fresh id");
        }
        _ => panic!("expected two clips"),
    }
}

#[test]
fn ops_reject_a_missing_track() {
    let p = two_track_fixture();
    assert!(matches!(
        trim_in_on(&p, 9, 1.0),
        Err(EditError::TrackNotFound(9))
    ));
    assert!(matches!(
        split_on(&p, 9, 1.0),
        Err(EditError::TrackNotFound(9))
    ));
    assert!(matches!(
        delete_under_on(&p, 9, 0.0),
        Err(EditError::TrackNotFound(9))
    ));
}

#[test]
fn absurd_lengths_are_refused_not_wrapped() {
    let p = fixture();
    // A gap of i64::MAX frames overflowed the duration sum and wrapped
    // the track's length negative (and would panic in a debug build).
    assert!(matches!(
        insert_gap(&p, 1.0, i64::MAX),
        Err(EditError::TooLong(_))
    ));
    // A position beyond the limit never reaches the arithmetic at all.
    assert!(matches!(
        insert_gap(&p, 1e18, 30),
        Err(EditError::TooLong(_))
    ));
    assert!(matches!(split(&p, 1e18), Err(EditError::TooLong(_))));
    // The limit is the limit: a long-but-sane gap still works.
    assert!(insert_gap(&p, 1.0, 30).is_ok());
}

#[test]
fn validate_refuses_a_track_longer_than_the_limit() {
    let mut p = fixture();
    p.timeline
        .video_track_mut()
        .unwrap()
        .segments
        .push(Segment::Gap(Gap {
            duration: Seconds::from_integer(MAX_TIMELINE_SECONDS + 1),
        }));
    assert!(matches!(
        p.validate(),
        Err(ProjectError::Timeline(TimelineError::TooLong { .. }))
    ));
}

#[test]
fn a_slow_down_past_the_timeline_limit_is_refused() {
    let p = fixture(); // clip #1 is 2 s of source
    // 2 s at 0.000001× is ~23 days: past the limit, and it used to be
    // reported as "timeline corrupt" after the fact.
    assert!(matches!(
        set_speed(&p, 1, Seconds::new(1, 1_000_000)),
        Err(EditError::TooLong(_))
    ));
    // A sane slow-down still works.
    assert!(set_speed(&p, 1, Seconds::new(1, 10)).is_ok());
}

#[test]
fn error_messages_name_the_actual_problem() {
    let p = fixture(); // [Clip 0..2] [Gap 2..3] [Clip 3..5]
    // Splitting a gap is impossible — and the message must not claim
    // the playhead is "not in a gap" when it is.
    assert!(matches!(split(&p, 2.5), Err(EditError::NoClip)));
    // A position past the end is not an "empty timeline".
    let msg = split(&p, 99.0).unwrap_err().to_string();
    assert!(msg.contains("outside it"), "{msg}");
}
