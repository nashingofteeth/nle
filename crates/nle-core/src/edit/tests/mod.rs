//! Unit tests for the edit module, split by the family under test.
//!
//! Fixtures live here; each child file reaches them and the module under
//! test through `use super::*`.

use super::*;
use crate::media::{RawFormat, RawProbe, RawStream};
use crate::project::VideoSettings;
use crate::time::Seconds;
use crate::timeline::{
    Clip, Gap, Segment, TrackKind, Transition, TransitionKind, TransitionSource,
};

mod blend_trim;
mod clamp;
mod clip;
mod effects;
mod extend;
mod history;
mod speed;
mod tracks;
mod transitions;
mod trim;

/// A file with both streams (like the demo's red.mp4) — clips can go
/// on a video *or* an audio track, which is what most fixtures need.
fn probe_report(duration: &str) -> RawProbe {
    RawProbe {
        streams: vec![
            RawStream {
                index: 0,
                codec_type: Some("video".into()),
                codec_name: Some("h264".into()),
                width: Some(320),
                height: Some(240),
                r_frame_rate: Some("30/1".into()),
                ..Default::default()
            },
            RawStream {
                index: 1,
                codec_type: Some("audio".into()),
                codec_name: Some("aac".into()),
                sample_rate: Some("48000".into()),
                channels: Some(2),
                ..Default::default()
            },
        ],
        format: RawFormat {
            format_name: "mp4".into(),
            duration: Some(duration.into()),
            ..Default::default()
        },
    }
}

/// [Clip #1 [0..2) src 1..3] [Gap 1 @2..3] [Clip #2 [3..5) src 0..2]
fn fixture() -> Project {
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    p.video = Some(VideoSettings {
        width: 320,
        height: 240,
    });
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
            id: 2,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
    ];
    p
}

fn clip_by_id(p: &Project, id: u32) -> Clip {
    p.timeline
        .video_track()
        .unwrap()
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Clip(c) => Some(c),
            Segment::Gap(_) | Segment::Transition(_) => None,
        })
        .find(|c| c.id == id)
        .unwrap()
        .clone()
}

fn clip_ids(p: &Project) -> Vec<u32> {
    p.timeline
        .video_track()
        .unwrap()
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Clip(c) => Some(c.id),
            Segment::Gap(_) | Segment::Transition(_) => None,
        })
        .collect()
}

/// Two tracks: video track 0 holds clip #1, audio track 1 holds clip #10.
fn two_track_fixture() -> Project {
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    p.timeline.tracks[0].segments = vec![Segment::Clip(Clip {
        id: 1,
        asset: a,
        asset_in: Seconds::from_integer(1),
        asset_out: Seconds::from_integer(3),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    let ai = p.timeline.add_track(TrackKind::Audio);
    p.timeline.tracks[ai].segments = vec![Segment::Clip(Clip {
        id: 10,
        asset: a,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(4),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })];
    p
}

/// The playback rate of one transition side: its source span over the
/// transition's `dur`.
fn side_rate(p: &Project, index: usize, outgoing: bool) -> Seconds {
    let seg = &p.timeline.video_track().unwrap().segments[index];
    let Segment::Transition(t) = seg else {
        panic!("expected a transition at {index}, got {seg:?}")
    };
    let side = if outgoing { &t.a } else { &t.b };
    match side {
        TransitionSource::Clip {
            src_in, src_out, ..
        } => (*src_out - *src_in) / t.dur,
        TransitionSource::Black => panic!("expected a clip side"),
    }
}

fn transition_seg(id: u32, dur: Seconds, a_out: Seconds) -> Segment {
    Segment::Transition(Transition {
        id,
        dur,
        kind: TransitionKind::Dissolve,
        a: TransitionSource::Clip {
            asset: 1,
            src_in: Seconds::from_integer(0),
            src_out: a_out,
            effects: Vec::new(),
        },
        b: TransitionSource::Black,
    })
}

/// [Clip #1 [0..3) src 0..3] [Clip #2 [3..6) src 0..3] — a plain cut.
fn clip_pair() -> Project {
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
    p
}
