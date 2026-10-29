//! Unit tests for the action catalog, split by job: parsing, running,
//! and the register sidecar.

use std::path::{Path, PathBuf};

use super::*;
use crate::media::{RawFormat, RawProbe, RawStream};
use crate::timeline::{Clip, Gap};

mod parse;
mod register;
mod run;

fn defaults() -> ParseDefaults {
    ParseDefaults {
        at: Some(7.5),
        track: 5,
        clip: Some(42),
    }
}

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

/// [Gap 1 @0..1] [Clip #1 1..4] [Gap 2 @4..6] [Clip #2 6..8]
fn fixture() -> Project {
    let mut p = Project::new();
    p.fps = Some(Seconds::from_integer(30));
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe_report("10"));
    let seg = |id: u32, in_s: i64, out_s: i64| {
        Segment::Clip(Clip {
            id,
            asset: a,
            asset_in: Seconds::from_integer(in_s),
            asset_out: Seconds::from_integer(out_s),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    };
    p.timeline.video_track_mut().unwrap().segments = vec![
        Segment::Gap(Gap {
            duration: Seconds::from_integer(1),
        }),
        seg(1, 0, 3),
        Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        }),
        seg(2, 0, 2),
    ];
    p
}

fn run(p: &mut Project, reg: &mut Option<Register>, a: &Action) -> Result<Outcome, EditError> {
    let mut h = History::default();
    run_action(p, &mut h, reg, a, 0.0)
}
