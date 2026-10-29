//! TUI unit tests, split by the mode or panel under test.
//!
//! Fixtures live here; each child file reaches them and the module under
//! test through `use super::*` (and `super::super` for private helpers).

use super::keys::command::run_command;
use super::panels::status::render_status;
use super::*;
use nle_core::Project;
use nle_core::edit;
use nle_core::media::{RawFormat, RawProbe, RawStream};
use nle_core::time::Seconds;
use nle_core::timeline::{Gap as TimelineGap, Timeline, Track, TrackKind};
use std::path::PathBuf;

mod command;
mod extend;
mod extend_sides;
mod fx;
mod keys;
mod motion;
mod playhead;
mod pool;
mod render;
mod status;
mod sync;
mod transition_form;

fn asset_project() -> Project {
    // [Clip #1 [0..3) src 1..4] [Gap 2 @3..5] [Clip #2 [5..7) src 0..2],
    // asset 1 is 10s long: headroom on both sides.
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    let probe = RawProbe {
        streams: vec![RawStream {
            index: 0,
            codec_type: Some("video".into()),
            codec_name: Some("h264".into()),
            width: Some(320),
            height: Some(240),
            r_frame_rate: Some("30/1".into()),
            ..Default::default()
        }],
        format: RawFormat {
            format_name: "mp4".into(),
            duration: Some("10".into()),
            ..Default::default()
        },
    };
    let (a, _) = p.add_asset("/media/red.mp4".into(), probe);
    p.timeline = Timeline {
        tracks: vec![Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![
                TimelineSegment::Clip(nle_core::Clip {
                    id: 1,
                    asset: a,
                    asset_in: Seconds::from_integer(1),
                    asset_out: Seconds::from_integer(4),
                    speed: Seconds::from_integer(1),
                    effects: Vec::new(),
                }),
                TimelineSegment::Gap(TimelineGap {
                    duration: Seconds::from_integer(2),
                }),
                TimelineSegment::Clip(nle_core::Clip {
                    id: 2,
                    asset: a,
                    asset_in: Seconds::from_integer(0),
                    asset_out: Seconds::from_integer(2),
                    speed: Seconds::from_integer(1),
                    effects: Vec::new(),
                }),
            ],
        }],
    };
    p
}

/// [C1 0..3][C2 3..6] with a 1 s dissolve at the 3.0 cut:
/// C1' 0..2 · blend [2, 3) · C2' 3..5.
fn blend_pair() -> Project {
    let mut p = asset_project();
    let a = p.media[0].id;
    let clip = |id, out: i64| {
        TimelineSegment::Clip(nle_core::Clip {
            id,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(out),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    };
    p.timeline.tracks[0].segments = vec![clip(1, 3), clip(2, 3)];
    let batch = edit::add_transition(
        &p,
        0,
        3.0,
        TransitionKind::Dissolve,
        Seconds::from_integer(1),
    )
    .unwrap();
    let mut h = edit::History::default();
    h.apply(&mut p, batch, 0).unwrap();
    p
}

/// Run a `:cmd` and report what the user sees (the action runner sets
/// the message itself; built-ins return a notice).
fn cmd(app: &mut App, line: &str) -> String {
    match run_command(app, &FakeMpv::new(), line) {
        Some(msg) => msg,
        None => app.message.clone(),
    }
}

/// A project the composite preview can build (gaps need a size).
fn watchable_project() -> Project {
    let mut p = asset_project();
    p.video = Some(nle_core::VideoSettings {
        width: 320,
        height: 240,
    });
    p
}

/// Put the app in the pool panel with `sel` selected (no player needed).
fn open_pool_mode(app: &mut App, sel: usize) {
    app.mode = Mode::Pool(PoolState {
        sel,
        pos: 0.0,
        resume: app.playhead,
    });
    app.pool_on_monitor = true;
}

fn clip_of(p: &Project, id: u32) -> nle_core::Clip {
    p.timeline
        .video_track()
        .unwrap()
        .segments
        .iter()
        .filter_map(|s| match s {
            TimelineSegment::Clip(c) => Some(c),
            TimelineSegment::Gap(_) | TimelineSegment::Transition(_) => None,
        })
        .find(|c| c.id == id)
        .unwrap()
        .clone()
}

fn seg_clip(id: u32, dur: i64) -> TimelineSegment {
    TimelineSegment::Clip(nle_core::Clip {
        id,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(dur),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    })
}

fn project() -> Project {
    let mut p = Project::new();
    p.fps = Some(Seconds::new(30, 1));
    p.timeline = Timeline {
        tracks: vec![Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![
                seg_clip(1, 3),
                TimelineSegment::Gap(TimelineGap {
                    duration: Seconds::from_integer(2),
                }),
                seg_clip(2, 2),
            ],
        }],
    };
    p
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// A project path under the system temp dir, so undo/redo's save doesn't
/// write into the repo. The counter keeps two calls with the same tag apart
/// (tests run in parallel in one process).
fn temp_project_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nle-tui-test-{}-{tag}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("t.json")
}

/// One rendered status row, as text (colors are not asserted).
fn status_row(app: &App) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(100, 1)).unwrap();
    term.draw(|f| render_status(f, f.area(), app)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..100u16).map(|x| buf[(x, 0)].symbol()).collect()
}

/// Give one clip an explicit effect list (the panel tests don't need the
/// edit path; `d`/`D`/undo are what they exercise).
fn set_effects(app: &mut App, clip: u32, effects: Vec<nle_core::timeline::Effect>) {
    for track in &mut app.project.timeline.tracks {
        for seg in &mut track.segments {
            if let TimelineSegment::Clip(c) = seg
                && c.id == clip
            {
                c.effects = effects.clone();
            }
        }
    }
}

// ---- input routing, against a fake monitor (no mpv process) ----

use std::cell::RefCell;

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Load(String, Option<f64>),
    SetPause(bool),
    TogglePause,
    SeekAbs(f64),
    SeekRel(f64),
    SetTitle(String),
}

/// Records what the TUI asks the monitor to do. Interior mutability because
/// the `Mpv` trait takes `&self`.
#[derive(Default)]
struct FakeMpv {
    state: RefCell<PlaybackState>,
    calls: RefCell<Vec<Call>>,
}

impl FakeMpv {
    fn new() -> Self {
        Self::default()
    }
    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }
}

impl Mpv for FakeMpv {
    fn state(&self) -> PlaybackState {
        self.state.borrow().clone()
    }
    fn load(&self, url: &str, start: Option<f64>) {
        self.calls
            .borrow_mut()
            .push(Call::Load(url.to_string(), start));
    }
    fn set_pause(&self, on: bool) {
        self.calls.borrow_mut().push(Call::SetPause(on));
    }
    fn toggle_pause(&self) {
        self.calls.borrow_mut().push(Call::TogglePause);
    }
    fn seek_absolute(&self, target: f64) {
        self.calls.borrow_mut().push(Call::SeekAbs(target));
    }
    fn seek_relative(&self, delta: f64) {
        self.calls.borrow_mut().push(Call::SeekRel(delta));
    }
    fn set_media_title(&self, title: &str) {
        self.calls
            .borrow_mut()
            .push(Call::SetTitle(title.to_string()));
    }
}
