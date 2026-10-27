//! The terminal dashboard: timeline visualization, vim keys, live playhead.
//!
//! Input model: a small vim. Counts work (`3l` = seek 3s), `gg`/`G` jump to
//! ends, `j`/`k` walk cut points, `w`/`b` walk clip starts, `:` opens a
//! command line. Playback keys are sent to mpv; state flows back through
//! the reader thread and is rendered on the next tick (~20fps redraw).

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

#[cfg(test)]
use nle_core::Batch;
use nle_core::EditError;
use nle_core::action::{
    self, Action, Edge, EffectArgs, GapLen, ParseDefaults, ParseError, Target, TransitionLen,
    effect_from_args,
};
use nle_core::edit;
use nle_core::timeline::{
    Effect, Segment as TimelineSegment, Track, TrackKind, TransitionKind, TransitionSource,
};
use nle_core::{
    Armed, Clip, CommitKind, History, Project, Seconds, clip_span, commit_batch, frame_of,
    frame_to_secs, gap_span, load_register, plan_commit, resolve_armed, save_register, secs_f64,
    track_label,
};

use crate::mpv::{Mpv, PlaybackState};
use crate::preview;

/// Seconds → f64 for display/playhead math. The model stays rational;
/// only rendering and mpv IPC touch floats.
pub fn secs(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

/// Snap a float position to the nearest frame of `fps` — the single
/// quantization for playhead position (display, marker, edits agree).
pub fn snap_to_frame(t: f64, fps: Option<&Seconds>) -> f64 {
    match fps {
        Some(f) => {
            let n = (t * secs(f)).round();
            n / secs(f)
        }
        None => t,
    }
}

/// Magnetic snap: within ~1.5 frames of a cut, the playhead lands exactly
/// on it. EDL drift makes seeks to cut times settle a frame past them;
/// this keeps the playhead (and with it the white indicator, `k`/`b`
/// targets, and edit points) canonical.
pub fn snap_to_cut(t: f64, project: &Project, track: usize) -> f64 {
    let Some(fps) = project.fps.as_ref() else {
        return t;
    };
    let radius = 1.5 / secs(fps);
    let mut best = t;
    let mut best_d = radius;
    for c in cut_points(project, track) {
        let d = (t - c).abs();
        if d <= best_d {
            best_d = d;
            best = c;
        }
    }
    best
}

/// The bounds of the gap containing `t` — between the last clip ending
/// at-or-before `t` and the first clip starting after it (the trailing
/// bound is the sequence tail when one exists). None when `t` is inside
/// a clip, or when there's no actual gap (no tail past the last clip).
fn gap_under(project: &Project, t: f64, ti: usize) -> Option<(Seconds, Seconds)> {
    let track = project.timeline.track(ti)?;
    if clip_under(project, t, ti).is_some() {
        return None; // inside a clip
    }
    let gt = grid_time(project, t);
    let si = track.index_at(gt)?;
    // merge adjacent gaps around si into one region
    let mut first = si;
    while first > 0 && matches!(track.segments[first - 1], TimelineSegment::Gap(_)) {
        first -= 1;
    }
    let mut last = si;
    while last + 1 < track.segments.len()
        && matches!(track.segments[last + 1], TimelineSegment::Gap(_))
    {
        last += 1;
    }
    let start = track.start_of(first);
    let end = track.start_of(last) + track.segments[last].duration();
    if end <= start {
        return None; // no materialized gap (no tail past the last clip)
    }
    Some((start, end))
}

/// "62.25 s" → "01:02:08" — MM:SS:FF, the last field being the frame
/// within the second at the project's frame rate (SMPTE style). Falls
/// back to tenths ("01:02.2") when no fps is known. The frame index is
/// ROUNDED (t is frame-snapped anyway) so the timecode always agrees
/// with the f{n} counter — no float fuzz in the fields.
pub fn fmt_timecode(t: f64, fps: Option<&Seconds>) -> String {
    let t = t.max(0.0);
    match fps {
        Some(f) => {
            let f = secs(f);
            let fps_n = f.round() as u64;
            let frames = (t * f).round() as u64;
            let (ff, whole) = if fps_n > 0 {
                (frames % fps_n, frames / fps_n)
            } else {
                (0, 0)
            };
            let (h, m, s) = (whole / 3600, (whole % 3600) / 60, whole % 60);
            if h > 0 {
                format!("{h}:{m:02}:{s:02}:{ff:02}")
            } else {
                format!("{m:02}:{s:02}:{ff:02}")
            }
        }
        None => {
            let h = (t / 3600.0).floor() as u64;
            let m = ((t % 3600.0) / 60.0).floor() as u64;
            let s = t % 60.0;
            if h > 0 {
                format!("{h}:{m:02}:{s:04.1}")
            } else {
                format!("{m:02}:{s:04.1}")
            }
        }
    }
}

// ------------------------------------------------------------ cut motions

/// Half a frame at the project's frame rate — the tolerance within which
/// a playhead counts as "standing on" a cut. mpv's EDL positions drift by
/// up to a frame (source frame-boundary rounding), so motions must be
/// drift-tolerant, not exact.
pub fn half_frame(project: &Project) -> f64 {
    project.fps.as_ref().map(|f| 0.5 / secs(f)).unwrap_or(1e-6)
}

/// All clip boundaries in seconds: 0, every clip start, every clip end.
pub fn cut_points(project: &Project, ti: usize) -> Vec<f64> {
    let Some(track) = project.timeline.track(ti) else {
        return vec![0.0];
    };
    let mut pts = vec![0.0];
    let mut acc = Seconds::from_integer(0);
    for s in &track.segments {
        acc += s.duration();
        pts.push(secs(&acc));
    }
    pts
}

pub fn next_cut(points: &[f64], t: f64, eps: f64) -> Option<f64> {
    points.iter().copied().find(|&p| p > t + eps)
}

pub fn prev_cut(points: &[f64], t: f64, eps: f64) -> Option<f64> {
    points.iter().rev().copied().find(|&p| p < t - eps)
}

pub fn next_clip_start(project: &Project, t: f64, eps: f64, ti: usize) -> Option<f64> {
    let track = project.timeline.track(ti)?;
    let mut acc = Seconds::from_integer(0);
    for s in &track.segments {
        if matches!(s, TimelineSegment::Gap(_)) {
            acc += s.duration();
            continue;
        }
        let start = secs(&acc);
        if start > t + eps {
            return Some(start);
        }
        acc += s.duration();
    }
    None
}

pub fn prev_clip_start(project: &Project, t: f64, eps: f64, ti: usize) -> Option<f64> {
    let track = project.timeline.track(ti)?;
    let mut acc = Seconds::from_integer(0);
    let mut last_start = None;
    for s in &track.segments {
        if matches!(s, TimelineSegment::Gap(_)) {
            acc += s.duration();
            continue;
        }
        let start = secs(&acc);
        if start >= t - eps {
            break;
        }
        last_start = Some(start);
        acc += s.duration();
    }
    last_start
}

/// Start of the next gap after the playhead — like `j`, but it skips over
/// clip boundaries and lands only where a gap begins.
pub fn next_gap_start(project: &Project, t: f64, eps: f64, ti: usize) -> Option<f64> {
    let track = project.timeline.track(ti)?;
    let mut acc = Seconds::from_integer(0);
    for s in &track.segments {
        let start = secs(&acc);
        if matches!(s, TimelineSegment::Gap(_)) && start > t + eps {
            return Some(start);
        }
        acc += s.duration();
    }
    None
}

/// Start of the previous gap before the playhead (skips clip boundaries).
pub fn prev_gap_start(project: &Project, t: f64, eps: f64, ti: usize) -> Option<f64> {
    let track = project.timeline.track(ti)?;
    let mut acc = Seconds::from_integer(0);
    let mut last = None;
    for s in &track.segments {
        let start = secs(&acc);
        if matches!(s, TimelineSegment::Gap(_)) && start < t - eps {
            last = Some(start);
        }
        acc += s.duration();
    }
    last
}

// ------------------------------------------------------------ app state

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Command(String),
    Extend(ExtendState),
    Pool(PoolState),
    Fx(FxState),
    Transition(TransitionForm),
}

/// Extend-mode session state. All bounds are timeline seconds; `hi` is
/// `None` for unbounded ranges (gap extension past the end). `mat` is
/// the asset-material span for display (clip-armed only — gaps have no
/// material). `preview_off` translates seeks: `mpv = playhead - off`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtendState {
    /// The track the armed boundary lives on.
    pub track: usize,
    pub armed: Armed,
    pub edge_pos: f64,
    pub ripple: bool,
    pub lo: f64,
    pub hi: Option<f64>,
    pub mat: Option<(f64, f64)>,
    pub preview_off: f64,
}

/// Media-pool panel state: the selected row, the preview clock (asset
/// seconds), and the timeline playhead to restore on close. A `Mode` variant
/// so "pool is open" and its selection can't drift apart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolState {
    pub sel: usize,
    pub pos: f64,
    pub resume: f64,
}

/// A one-field transition length form, opened by `x`/`X` when the playhead
/// is inside a blend. `frames` is the typed text (blank = keep `current`).
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionForm {
    pub id: u32,
    /// The blend's start (the action targets it).
    pub start: f64,
    pub kind: TransitionKind,
    pub frames: String,
    pub current: i64,
}

/// Effects-panel state: the clip whose effects are listed, the row under the
/// cursor, and the open "add effect" form (if any). A `Mode` variant so
/// "panel is open" and its target can't drift apart.
#[derive(Debug, Clone, PartialEq)]
pub struct FxState {
    pub clip: u32,
    pub sel: usize,
    pub form: Option<FxForm>,
}

/// A structured "add/edit effect" form: one string per parameter. The values
/// are typed into pre-labelled fields (no shell-style flags in the TUI), then
/// handed to `effect_from_args` — the same parser the CLI uses — so the
/// required fields and neutral defaults can't drift.
#[derive(Debug, Clone, PartialEq)]
pub struct FxForm {
    pub kind: &'static str,
    pub fields: Vec<FxField>,
    pub focus: usize,
    /// `Some(index)` when the form edits an existing effect in place;
    /// `None` appends a new one.
    pub editing: Option<usize>,
    /// Scale only: keep the width/height ratio while typing (`a` toggles).
    pub lock: bool,
    /// The ratio to preserve when `lock` is on (`width / height`).
    pub aspect: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FxField {
    pub label: &'static str,
    pub value: String,
    /// The applied value while editing: shown beside the blank input and used
    /// when the input is left blank.
    pub previous: Option<String>,
    pub kind: FieldKind,
    /// The letter that focuses this field. `None` for text fields, which must
    /// accept every letter as input.
    pub key: Option<char>,
}

impl FxField {
    /// The string to parse: what was typed, or the previous value while
    /// editing with the input still blank.
    fn effective(&self) -> &str {
        let typed = self.value.trim();
        if typed.is_empty() {
            self.previous.as_deref().unwrap_or("")
        } else {
            typed
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldKind {
    /// Decimal (percentages, eq values; `-` is allowed).
    Float,
    /// A file path (any printable character).
    Path,
    /// An on/off option: space (or its jump key) toggles it.
    Toggle,
}

/// Which two-key sequence is waiting for its second keystroke (`gg`, `dd`, `yy`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Pending {
    None,
    G,
    D,
    Y,
}

/// A session file's identity as we last saw it: modification time, length,
/// and (on Unix) the inode. Atomic saves rename a fresh temp file into
/// place, so the inode changes even when the filesystem's mtime
/// granularity is coarser than the write — an mtime-only watch can miss a
/// foreign write that lands in the same tick.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
    ino: u64,
}

fn stamp(path: &std::path::Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    #[cfg(not(unix))]
    let ino = 0;
    Some(Stamp {
        mtime: meta.modified().ok(),
        len: meta.len(),
        ino,
    })
}

/// The files a session shares with the shell — `nle.json`, its undo
/// sidecar, and the yank register — and how we last saw them. A foreign
/// stamp means a shell command got there first.
#[derive(Debug, Default)]
struct Seen {
    project: Option<Stamp>,
    history: Option<Stamp>,
    register: Option<Stamp>,
}

/// A background job (an ffmpeg render, a heavy import): its label for
/// notices, whether the project file changed underneath us (reload after),
/// and the channel its worker reports on. One at a time — the monitor
/// keeps playing while it runs.
struct Job {
    label: &'static str,
    reload: bool,
    rx: std::sync::mpsc::Receiver<Result<String, String>>,
}

/// A monitor preview rebuild in flight. Proxy renders are seconds of ffmpeg,
/// so the EDL is built on a worker while the status row shows progress.
struct PreviewJob {
    rx: std::sync::mpsc::Receiver<PreviewMsg>,
    /// When the build started — the status row waits a moment before
    /// announcing it, so a cached/no-proxy rebuild doesn't flash.
    started: Instant,
}

enum PreviewMsg {
    /// `(finished renders, total renders, fraction of the one in flight)`.
    Progress {
        done: usize,
        total: usize,
        frac: f64,
    },
    Done(Result<String, String>),
}

/// What the status row shows while a preview renders.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PreviewProgress {
    pub done: usize,
    pub total: usize,
    pub frac: f64,
}

pub struct App {
    pub project: Project,
    pub path: PathBuf,
    pub mode: Mode,
    /// Playhead in seconds (from mpv; EDL time == timeline time).
    pub playhead: f64,
    /// Pending vim count ("3" in `3l`).
    pub count: Option<u32>,
    /// Pending two-key sequence.
    pub pending: Pending,
    /// Yanked segment, for `p`/`P` — the shared register (`nle.clipboard.json`),
    /// so `nle yank` and `yy` fill the same slot. Carries the clip's asset
    /// path, so a register from a rebuilt project resolves (or refuses)
    /// instead of pasting the wrong media.
    pub register: Option<nle_core::Register>,
    /// Undo/redo stack of edit batches (session-only).
    pub history: History,
    pub reload_requested: bool,
    pub quit: bool,
    pub message: String,
    /// The `:` line was opened from the pool (`i`): leaving it must put the
    /// timeline back on the monitor, or the monitor keeps playing the pooled
    /// file while the UI shows the timeline.
    command_from_pool: bool,
    /// The monitor currently holds the pool's raw file (set by
    /// `load_pool_preview`, cleared when the timeline EDL is loaded). While an
    /// async EDL reload is in flight the mode is already Normal, so this keeps
    /// `sync` from mapping the pooled clock onto the timeline playhead.
    pool_on_monitor: bool,
    /// After closing the pool, the timeline EDL is loaded at this position.
    /// Keep ignoring mpv's old pooled clock until the new file reports it.
    pool_handoff: Option<f64>,
    /// Pause state to apply once the post-pool timeline EDL is loaded.
    pending_pause: Option<bool>,
    // synced from mpv every tick
    pub paused: bool,
    pub loaded: bool,
    pub dead: bool,
    /// mpv reached the end of the timeline (playthrough, or seek past the
    /// last frame) — the two-endings contract: EOF renders flush at the
    /// border, the last-frame position renders where it actually is.
    pub eof: bool,
    pub mpv_error: Option<String>,
    pub exit_message: Option<String>,
    /// Remembered pool state so reopening the panel returns to the same
    /// asset and preview clock. Sticky UI state, not panel state (whether the
    /// panel is open is `Mode::Pool`).
    last_pool_sel: usize,
    last_pool_pos: f64,
    last_pool_asset: Option<u32>,
    /// The scale form's aspect lock is sticky across forms and edits and on by
    /// default; turning it off keeps later scale forms unlocked.
    fx_scale_lock: bool,
    /// Which track edits and navigation act on. Clamped via `active()`.
    pub active_track: usize,
    /// The background render/import, if one is running.
    job: Option<Job>,
    /// The monitor's EDL being built off-thread (proxy renders); `None` when
    /// idle.
    preview: Option<PreviewJob>,
    /// An edit landed while a preview rendered: build again when it ends.
    preview_dirty: bool,
    /// Off in tests and `--dump` (no worker, no monitor): only the real TUI
    /// pays for proxy renders.
    preview_enabled: bool,
    /// Progress for the status row while `preview` runs.
    pub preview_progress: Option<PreviewProgress>,
    /// When the status-bar crocodile's mouth is open — a delete just removed
    /// material from the timeline. `None`/past means closed.
    chomp_until: Option<Instant>,
    /// Session files as we last read/wrote them (see [`Seen`]).
    seen: Seen,
    /// A foreign change arrived while a mode held state; reload when back
    /// in Normal.
    reload_pending: bool,
}

impl App {
    pub fn new(project: Project, path: PathBuf) -> Self {
        let mut app = Self {
            project,
            path,
            mode: Mode::Normal,
            playhead: 0.0,
            count: None,
            pending: Pending::None,
            register: None,
            history: History::default(),
            reload_requested: false,
            quit: false,
            message: String::new(),
            command_from_pool: false,
            pool_on_monitor: false,
            pool_handoff: None,
            pending_pause: None,
            paused: true,
            loaded: false,
            dead: false,
            eof: false,
            mpv_error: None,
            exit_message: None,
            last_pool_sel: 0,
            last_pool_pos: 0.0,
            last_pool_asset: None,
            fx_scale_lock: true,
            active_track: 0,
            job: None,
            preview: None,
            preview_dirty: false,
            preview_enabled: false,
            preview_progress: None,
            chomp_until: None,
            seen: Seen::default(),
            reload_pending: false,
        };
        // The disk state we start from: anything later is foreign.
        app.note_seen();
        app
    }

    /// The active track index, clamped to a valid one.
    pub fn active(&self) -> usize {
        self.active_track
            .min(self.project.timeline.tracks.len().saturating_sub(1))
    }

    /// The active track, if any.
    pub fn track(&self) -> Option<&Track> {
        self.project.timeline.track(self.active())
    }

    pub fn set_message(&mut self, msg: &str) {
        crate::log::line(msg);
        self.message = msg.to_string();
    }

    /// Remember the on-disk times of the session files — after a load, or
    /// after a save we made (so our own writes don't read as foreign).
    fn note_seen(&mut self) {
        self.seen = Seen {
            project: stamp(&self.path),
            history: stamp(&History::sidecar_path(&self.path)),
            register: stamp(&action::register_path(&self.path)),
        };
    }

    fn note_project(&mut self) {
        self.seen.project = stamp(&self.path);
    }

    fn note_history(&mut self) {
        self.seen.history = stamp(&History::sidecar_path(&self.path));
    }

    fn note_register(&mut self) {
        self.seen.register = stamp(&action::register_path(&self.path));
    }

    /// Whether a shell wrote one of the shared files since we last read or
    /// wrote it.
    fn external_change(&self) -> bool {
        stamp(&self.path) != self.seen.project
            || stamp(&History::sidecar_path(&self.path)) != self.seen.history
            || stamp(&action::register_path(&self.path)) != self.seen.register
    }

    pub fn sync(&mut self, st: &PlaybackState) {
        // Authority split: while PLAYING, mpv's streamed position drives
        // the playhead (only it knows the live position). While PAUSED,
        // the MODEL owns it — the playhead only moves through our own
        // seeks and frame-steps, so mpv's drift-flavored reports (EDL
        // boundaries sit off the project grid) can never fight the
        // canonical positions that cuts, the white indicator, and edit
        // keys depend on.
        //
        // The pool left the monitor, but mpv may still report the pooled file
        // until `loadfile` swaps in the timeline EDL. Wait until the new file
        // reports the landing position before letting its clock drive the
        // playhead — otherwise the pool's end can be snapped onto a cut.
        let mut landed = false;
        if self.pool_on_monitor
            && let Some(landing) = self.pool_handoff
            && st.file_loaded
        {
            let tol = self.project.fps.as_ref().map_or(0.05, |f| 2.0 / secs(f));
            if st.time_pos.is_some_and(|t| (t - landing).abs() <= tol) {
                self.pool_handoff = None;
                self.pool_on_monitor = false;
                // The model already owns the landing; don't snap it here.
                self.playhead = landing.clamp(0.0, self.last_frame());
                landed = true;
            }
        }
        // In extend mode mpv plays the material preview, whose position 0 is
        // the range start: translate back to timeline coordinates.
        if !st.paused
            && st.file_loaded
            && !landed
            && let Some(t) = st.time_pos
        {
            if is_pool(self) {
                // The monitor is a raw pooled file: position is asset
                // seconds, no EDL translation. After the EOF hand-back the
                // mode is still Pool but the monitor is the timeline EDL.
                if self.pool_on_monitor {
                    set_pool_pos(self, t);
                }
            } else if !self.pool_on_monitor {
                // `pool_on_monitor` covers the hand-off window: mode is
                // already Normal, but the asynchronous EDL reload has not
                // landed yet and mpv is still reporting the pooled file.
                let t = match &self.mode {
                    Mode::Extend(st) => t + st.preview_off,
                    _ => t,
                };
                let snapped = snap_to_frame(t, self.project.fps.as_ref());
                self.playhead = match &self.mode {
                    // Extend mode: stay within the armed range — the
                    // material preview can't drive the playhead past the
                    // segment's end (or before its start).
                    Mode::Extend(st) => clamp_extend(st, snapped),
                    _ => snap_to_cut(snapped, &self.project, self.active())
                        .clamp(0.0, self.last_frame()),
                };
            }
        }
        self.paused = st.paused;
        self.loaded = st.file_loaded;
        self.dead = st.dead;
        self.eof = st.eof;
        // Pool at EOF: pin the asset clock to its last frame so the strip
        // marker and the status don't read a couple frames short of the
        // probe duration. The timeline cursor is untouched (and restored on
        // close).
        if is_pool(self) && st.eof {
            let sel = pool_sel(self).unwrap_or(0);
            if let Some(d) = self.project.media.get(sel).and_then(|a| a.duration()) {
                let last = pool_last_frame(self, secs(&d));
                set_pool_pos(self, last);
            }
        }
        if let Some(e) = &st.last_error {
            if self.mpv_error.as_deref() != Some(e.as_str()) {
                crate::log::line(&format!("mpv: {e}"));
            }
            self.mpv_error = Some(e.clone());
        }
    }

    pub fn total(&self) -> f64 {
        // Timeline::end includes the sequence tail — deletes/trims of the
        // last clip leave a trailing gap that the UI must keep showing.
        secs(&self.project.timeline.end())
    }

    /// The last representable playhead position — a timeline of `dur` spans
    /// `[0, dur)`, so `dur` itself is past the final frame.
    fn last_frame(&self) -> f64 {
        secs(&self.project.last_frame())
    }

    fn guard_loaded(&mut self) -> bool {
        if self.loaded {
            true
        } else {
            self.set_message("monitor is still loading…");
            false
        }
    }

    fn seek_rel(&mut self, player: &dyn Mpv, delta: f64) {
        if self.guard_loaded() {
            player.seek_relative(delta);
            // Model-authoritative while paused: h/l moves the playhead by
            // the same arithmetic the display uses.
            self.playhead = (self.playhead + delta).clamp(0.0, self.last_frame());
            self.note_handoff_seek(self.playhead);
        }
    }

    fn seek_to(&mut self, player: &dyn Mpv, t: f64) {
        if self.guard_loaded() {
            // A timeline of `dur` spans `[0, dur)`: clamp onto its last frame,
            // not the end boundary.
            let target = t.clamp(0.0, self.last_frame());
            // Model position is authoritative when paused: set it now.
            self.playhead = target;
            self.note_handoff_seek(target);
            // Nudge a few ms INTO the target instant: EDL segment
            // boundaries sit a hair after the cut (source frame rounding),
            // so an exact seek would display the PREVIOUS segment's last
            // frame (gap/black) — the clip's color only appears after one
            // manual frame-step. 4ms clears the drift seen on this
            // machine's timelines.
            player.seek_absolute(target + 0.004);
        }
    }

    /// A seek during a monitor hand-off is the *new* landing: mpv applies it
    /// to the file being loaded, so its first report is this position and the
    /// wait ends there. Leaving the old landing would make `sync` wait for a
    /// position the user already left — and snap the playhead back to it if a
    /// report happened to pass. Both absolute and relative seeks call this.
    fn note_handoff_seek(&mut self, target: f64) {
        if self.pool_on_monitor && !is_pool(self) {
            self.pool_handoff = Some(target);
        }
    }

    /// Move the playhead by one frame (f/F): model-authoritative, clamped
    /// to the sequence. mpv steps its own (drifted) neighbor — close
    /// enough that the next step lands on the frame we display.
    fn step_playhead(&mut self, dir: i64) {
        let Some(fps) = self.project.fps.as_ref() else {
            return;
        };
        let f = secs(fps);
        let last = ((self.total() * f).ceil() as i64 - 1).max(0);
        let idx = ((self.playhead * f).round() as i64 + dir).clamp(0, last);
        self.playhead = idx as f64 / f;
    }
}

// ------------------------------------------------------------ event loop

/// How long the transport must be still before the session playhead is
/// written to the project. Long enough that holding `l` or playing back
/// doesn't write a project file per frame; short enough that a shell sees
/// where the cursor is shortly after it stops moving.
const PLAYHEAD_FLUSH_IDLE: Duration = Duration::from_millis(1000);

/// A monitor that does nothing: the `--dump` path renders state, it never
/// plays, so the handlers get a no-op player.
struct NullMpv;

impl Mpv for NullMpv {
    fn state(&self) -> PlaybackState {
        PlaybackState {
            time_pos: None,
            duration: None,
            paused: true,
            file_loaded: false,
            eof: false,
            dead: false,
            last_error: None,
        }
    }
    fn load(&self, _url: &str, _start: Option<f64>) {}
    fn set_pause(&self, _on: bool) {}
    fn toggle_pause(&self) {}
    fn seek_absolute(&self, _target: f64) {}
    fn seek_relative(&self, _delta: f64) {}
    fn set_media_title(&self, _title: &str) {}
}

/// Render the TUI once at `size` and return a text dump: the project path and
/// model state (as `nle show` prints it), then the grid with a column ruler.
/// Feed `keys` first (`\n` = Enter, `\e` = Esc) to reach a state.
///
/// The keys run through the real handlers, which save the project, the undo
/// sidecar, and the register — so the dump runs on a temp **copy** of all
/// three: a diagnostic must never touch what it is diagnosing (a dump with an
/// editing key used to rewrite the project you were looking at).
pub fn dump(project: Project, path: PathBuf, size: (u16, u16), keys: &str) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // A private scratch dir: the keys run through the real handlers, which
    // save, so everything must land somewhere disposable.
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("nle-dump-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    let copy = dir.join(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("dump.json")),
    );
    project.save(&copy).expect("save the dump copy");
    // Bring the session state along (undo depth, the register) so the dump
    // shows the same thing, but every save lands in the scratch dir.
    for sidecar in [
        nle_core::edit::History::sidecar_path(&path),
        path.with_extension("clipboard.json"),
    ] {
        if let (Ok(bytes), Some(name)) = (std::fs::read(&sidecar), sidecar.file_name()) {
            let _ = std::fs::write(dir.join(name), bytes);
        }
    }

    let mut app = App::new(project, copy);
    // The handlers guard on this (mpv "loaded"); the dump has no monitor.
    app.loaded = true;
    let player = NullMpv;
    for k in keys.chars() {
        let code = match k {
            '\n' => KeyCode::Enter,
            '\t' => KeyCode::Tab,
            '\x1b' => KeyCode::Esc,
            c => KeyCode::Char(c),
        };
        handle_key(&mut app, &player, KeyEvent::new(code, KeyModifiers::NONE));
    }
    let (w, h) = size;
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("a test terminal");
    term.draw(|f| render(f, &app)).expect("a draw");
    let buf = term.backend().buffer();

    let mut out = format!("project: {}\n", path.display());
    out.push_str(&crate::show_report(&app.project));
    out.push_str(&format!(
        "\nmode: {:?} · active: {} · playhead: {} s (f{}) · paused: {} · eof: {} · pool-on-monitor: {} · pool-handoff: {:?}\nmessage: {:?}\n",
        app.mode,
        track_label(&app.project, app.active()),
        app.playhead,
        frame_of(app.project.fps.as_ref(), app.playhead),
        app.paused,
        app.eof,
        app.pool_on_monitor,
        app.pool_handoff,
        app.message,
    ));
    out.push_str(&format!("=== grid {w}x{h} ===\n"));
    let ruler = |step: u16| -> String {
        let mut s = String::from("    ");
        for x in 0..w {
            s.push(if x % step == 0 { '|' } else { ' ' });
        }
        s
    };
    out.push_str(&ruler(10));
    out.push('\n');
    out.push_str("    ");
    for x in 0..w {
        out.push(char::from_digit((x / 10 % 10) as u32, 10).unwrap_or(' '));
    }
    out.push('\n');
    out.push_str("    ");
    for x in 0..w {
        out.push(char::from_digit((x % 10) as u32, 10).unwrap_or(' '));
    }
    out.push('\n');
    for y in 0..h {
        let row: String = (0..w)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect();
        out.push_str(&format!("{y:>3} {row}\n"));
    }
    let _ = std::fs::remove_dir_all(&dir);
    out
}

pub fn run<P: Mpv>(project: Project, path: PathBuf, player: P) -> anyhow::Result<()> {
    let mut terminal = ratatui::init(); // raw mode + alternate screen + panic hook
    let history = History::load(&History::sidecar_path(&path)).unwrap_or_default();
    let mut app = App::new(project, path);
    app.history = history;
    // The shared register: what `nle yank` (or an earlier session) left.
    app.register = load_register(&app.path);
    // Open where `nle seek` (or the last session) left the playhead.
    app.playhead = secs(&app.project.playhead);
    // Build the monitor off-thread before the first frame: a cold proxy can
    // take seconds and the status row will show it.
    app.preview_enabled = true;
    start_preview(&mut app);
    // Media title (MPRIS "Now playing") follows the clip under the playhead.
    let mut last_media_title: Option<String> = None;
    // When the playhead was last seen moving, for the idle flush below.
    let mut playhead_moving: Option<Instant> = None;

    loop {
        let st = player.state();
        app.sync(&st);

        // A shell may have written the shared files (`nle split`,
        // `nle seek`, `nle yank`) — adopt it before anything of our own,
        // so the next save can't clobber it. Modes hold indices into the
        // current state, so the reload waits for Normal.
        if watch_foreign(&mut app) {
            match reload_project(&mut app, &player, true) {
                Ok(()) => app.set_message("adopted changes from the shell"),
                Err(e) => app.set_message(&format!("reload failed: {e}")),
            }
        }

        if app.reload_requested {
            app.reload_requested = false;
            match reload_project(&mut app, &player, false) {
                Ok(()) => app.set_message("timeline reloaded"),
                Err(e) => app.set_message(&format!("reload failed: {e}")),
            }
        }

        // A finished worker reports in (export, or an importer that just
        // rewrote the project).
        poll_job(&mut app, &player);
        poll_preview(&mut app, &player);

        // The stored playhead follows the transport: after a moment of
        // stillness, write where the cursor landed (`nle seek` reads it).
        // Diffing here catches every mover — keys, mpv playback, undo —
        // without instrumenting each handler.
        if playhead_moved(&app) {
            playhead_moving = Some(Instant::now());
        } else if let Some(since) = playhead_moving
            && since.elapsed() >= PLAYHEAD_FLUSH_IDLE
        {
            playhead_moving = None; // one shot; a failure retries on the next move
            match persist_playhead(&mut app) {
                Ok(true) => {}
                // A shell write landed mid-flight: adopt it next tick.
                Ok(false) => app.reload_pending = true,
                Err(e) => app.set_message(&format!("save failed: {e}")),
            }
        }

        // Keep the desktop's "Now playing" in sync with the clip playing.
        if st.file_loaded {
            let current = clip_under(&app.project, app.playhead, app.active())
                .map(|c| {
                    app.project
                        .asset(c.asset)
                        .and_then(|a| a.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| "clip".to_string())
                })
                .unwrap_or_else(|| "gap".to_string());
            let title = format!("{current} — nle");
            if last_media_title.as_deref() != Some(title.as_str()) {
                player.set_media_title(&title);
                last_media_title = Some(title);
            }
        }

        if app.quit {
            break;
        }

        terminal.draw(|f| render(f, &app))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            handle_key(&mut app, &player, key);
        }

        if st.dead {
            app.exit_message = Some("mpv monitor closed — exiting".to_string());
            break;
        }
    }

    // Quitting right after a move keeps the cursor where it was left.
    if playhead_moving.is_some()
        && let Err(e) = persist_playhead(&mut app)
    {
        crate::log::line(&format!("playhead save failed on exit: {e}"));
    }

    ratatui::restore();
    if let Some(msg) = app.exit_message {
        crate::say!("{msg}");
    }
    Ok(())
}

/// Whether the live playhead has moved off the stored one — compared in
/// **frames**, because the two floats are computed by different paths and
/// can differ in the last ulp (a float compare would re-trigger the flush
/// forever).
fn playhead_moved(app: &App) -> bool {
    let fps = app.project.fps.as_ref();
    frame_of(fps, app.playhead) != frame_of(fps, secs(&app.project.playhead))
}

/// Write the session playhead into the project and save it — the stored
/// cursor `nle seek` reads. Called after edits, by `:seek`, and by the
/// transport's idle flush.
///
/// `Ok(true)` = written; `Ok(false)` = a shell got there first, so nothing
/// was written (the caller adopts the file instead — we never clobber).
fn persist_playhead(app: &mut App) -> anyhow::Result<bool> {
    if app.external_change() {
        return Ok(false);
    }
    app.project.set_playhead(app.playhead);
    app.project.save(&app.path)?;
    app.note_project();
    Ok(true)
}

/// Decide what to do about a foreign write to the shared files: adopt it
/// now (Normal mode), or hold it until the mode ends — modes hold indices
/// into the current state. Returns whether a reload should run now.
fn watch_foreign(app: &mut App) -> bool {
    let foreign = app.external_change();
    if foreign && !matches!(app.mode, Mode::Normal) {
        app.reload_pending = true;
        return false;
    }
    if !foreign && !app.reload_pending {
        return false;
    }
    // Normal mode with something to adopt (just arrived, or held earlier).
    app.reload_pending = false;
    true
}

/// Reload the project (and its undo history) from disk and rebuild the
/// preview. A **user** reload (`R`) keeps the live playhead; adopting a
/// foreign write (`nle seek`, an edit in another window) also adopts that
/// file's playhead.
fn reload_project(app: &mut App, _player: &dyn Mpv, adopt_playhead: bool) -> anyhow::Result<()> {
    let project = Project::load(&app.path)?;
    app.project = project;
    app.active_track = app
        .active_track
        .min(app.project.timeline.tracks.len().saturating_sub(1));
    app.history = History::load(&History::sidecar_path(&app.path)).unwrap_or_default();
    // The register is shared too: a shell may have yanked since we read it.
    app.register = load_register(&app.path);
    if adopt_playhead {
        // A shell may have moved it (`nle seek`); adopt that cursor.
        app.playhead = secs(&app.project.playhead);
    } else {
        // A manual reload keeps the cursor; just clamp it into the new
        // timeline if an edit shortened it.
        app.playhead = app.playhead.clamp(0.0, app.last_frame().max(0.0));
    }
    // Everything on disk is ours again.
    app.note_seen();
    app.reload_pending = false;
    // The monitor rebuilds off-thread (a cold proxy could take seconds).
    start_preview(app);
    Ok(())
}

// ------------------------------------------------------------ keys

/// Snapshot the pool's selected asset and preview clock for the next open.
fn remember_pool(app: &mut App) {
    let (sel, pos) = match &app.mode {
        Mode::Pool(p) => (p.sel, p.pos),
        _ => return,
    };
    app.last_pool_sel = sel;
    app.last_pool_pos = pos;
    app.last_pool_asset = app.project.media.get(sel).map(|a| a.id);
}

/// Pool-state accessors. `Mode::Pool` is `Copy`, so these take/return owned
/// values and each borrow ends at the statement. That is what lets pool
/// handlers stay `&mut App` while also touching `project`/`history` — a helper
/// returning `&mut PoolState` would borrow all of `App` and collide.
fn is_pool(app: &App) -> bool {
    matches!(app.mode, Mode::Pool(_))
}

fn pool_sel(app: &App) -> Option<usize> {
    match &app.mode {
        Mode::Pool(p) => Some(p.sel),
        _ => None,
    }
}

fn set_pool_sel(app: &mut App, sel: usize) {
    if let Mode::Pool(p) = &mut app.mode {
        p.sel = sel;
    }
}

fn pool_pos(app: &App) -> f64 {
    match &app.mode {
        Mode::Pool(p) => p.pos,
        _ => 0.0,
    }
}

fn set_pool_pos(app: &mut App, pos: f64) {
    if let Mode::Pool(p) = &mut app.mode {
        p.pos = pos;
    }
}

fn pool_resume(app: &App) -> f64 {
    match &app.mode {
        Mode::Pool(p) => p.resume,
        _ => 0.0,
    }
}

fn is_fx(app: &App) -> bool {
    matches!(app.mode, Mode::Fx(_))
}

/// A copy of the panel state: the handlers mutate `app` (and call
/// `run_action`), so a `&mut FxState` borrow would collide with the rest.
fn fx_state(app: &App) -> Option<&FxState> {
    match &app.mode {
        Mode::Fx(st) => Some(st),
        _ => None,
    }
}

fn fx_state_mut(app: &mut App) -> Option<&mut FxState> {
    match &mut app.mode {
        Mode::Fx(st) => Some(st),
        _ => None,
    }
}

fn set_fx_sel(app: &mut App, sel: usize) {
    if let Some(st) = fx_state_mut(app) {
        st.sel = sel;
    }
}

fn set_fx_form(app: &mut App, form: Option<FxForm>) {
    if let Some(st) = fx_state_mut(app) {
        st.form = form;
    }
}

/// Keep the cursor on a row after an edit shrank the list.
fn clamp_fx_sel(app: &mut App) {
    let Some(st) = fx_state(app) else { return };
    let n = fx_clip(app).map_or(0, |c| c.effects.len());
    let sel = st.sel.min(n.saturating_sub(1));
    set_fx_sel(app, sel);
}

/// The clip with `id` on any track — clip ids are unique across the timeline.
fn clip_by_id(project: &Project, id: u32) -> Option<&Clip> {
    project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .find_map(|seg| match seg {
            TimelineSegment::Clip(c) if c.id == id => Some(c),
            _ => None,
        })
}

fn fx_clip(app: &App) -> Option<&Clip> {
    let st = fx_state(app)?;
    clip_by_id(&app.project, st.clip)
}

pub fn handle_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    // Ctrl-C quits even in raw mode (no SIGINT is generated). One guard for
    // every mode: the panels that grew later (command, pool, transition, fx)
    // each missed their own copy and the key went dead there.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        return;
    }
    if matches!(app.mode, Mode::Command(_)) {
        let buf = match std::mem::replace(&mut app.mode, Mode::Normal) {
            Mode::Command(buf) => buf,
            _ => unreachable!("checked above"),
        };
        command_key(app, player, buf, key);
    } else if matches!(app.mode, Mode::Extend(_)) {
        extend_key(app, player, key);
    } else if is_pool(app) {
        pool_key(app, player, key);
    } else if matches!(app.mode, Mode::Transition(_)) {
        transition_form_key(app, player, key);
    } else if is_fx(app) {
        fx_key(app, player, key);
    } else {
        normal_key(app, player, key);
    }
}

/// Whether `code` completes the pending `gg` motion. Pure so the pool and
/// normal modes share one rule — and it is testable without a live mpv.
fn completes_gg(pending: Pending, code: KeyCode) -> bool {
    matches!((pending, code), (Pending::G, KeyCode::Char('g')))
}

/// Media-pool panel keys: move the list (loading each asset for preview),
/// insert/remove, and seek/play the preview.
fn pool_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    // Counts build for the seek keys (bare `0` = start of the file).
    if let KeyCode::Char(c @ '0'..='9') = key.code {
        if c == '0' && app.count.is_none() {
            pool_seek_abs(app, player, 0.0);
            return;
        }
        let d = c.to_digit(10).unwrap_or(0);
        app.count = Some(app.count.unwrap_or(0) * 10 + d);
        return;
    }
    let had_count = app.count.is_some();
    let mult = app.count.take().unwrap_or(1);

    // Second key of the `gg` sequence (mirrors normal mode). This must run
    // BEFORE the match below sets the pending state, or a lone `g` would
    // complete itself.
    if completes_gg(app.pending, key.code) {
        app.pending = Pending::None;
        pool_seek_abs(app, player, 0.0);
        return;
    }

    let n = app.project.media.len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('m') => close_pool(app, player),
        // List motion (j/k), list ends ({ }).
        KeyCode::Char('j') | KeyCode::Down => pool_move(app, player, 1, n),
        KeyCode::Char('k') | KeyCode::Up => pool_move(app, player, -1, n),
        KeyCode::Char('{') => pool_select(app, player, 0),
        KeyCode::Char('}') => pool_select(app, player, n.saturating_sub(1)),
        // Preview ends (file, not list): gg / 0 = start, G / $ = end.
        KeyCode::Char('g') => app.pending = Pending::G,
        KeyCode::Char('G') | KeyCode::Char('$') => pool_seek_end(app, player),
        KeyCode::Enter | KeyCode::Char('P') => insert_from_pool(app, player),
        KeyCode::Char('d') => remove_from_pool(app, player),
        // Import a file into the pool: opens the command line pre-filled.
        KeyCode::Char('i') => {
            remember_pool(app);
            app.command_from_pool = true;
            app.mode = Mode::Command("import ".to_string());
        }
        KeyCode::Char('u') => do_undo(app, player),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => do_redo(app, player),
        // Preview transport (asset seconds).
        KeyCode::Char(' ') => pool_toggle_play(app, player),
        KeyCode::Char('h') => pool_seek_rel(app, player, -(mult as f64)),
        KeyCode::Char('l') => pool_seek_rel(app, player, mult as f64),
        KeyCode::Char('H') => pool_seek_rel(app, player, -5.0 * mult as f64),
        KeyCode::Char('L') => pool_seek_rel(app, player, 5.0 * mult as f64),
        KeyCode::Char('f') => pool_step(app, player, 1),
        KeyCode::Char('F') => pool_step(app, player, -1),
        // Track change is available in the pool so `enter` inserts where you
        // mean (the monitor keeps the asset preview; see `pool_set_track`).
        KeyCode::Char('t') => {
            let n = app.project.timeline.tracks.len();
            let idx = if had_count {
                (mult as usize).saturating_sub(1)
            } else {
                (app.active() + 1) % n.max(1)
            };
            pool_set_track(app, idx);
        }
        KeyCode::Char('T') => {
            let n = app.project.timeline.tracks.len();
            let idx = if had_count {
                (mult as usize).saturating_sub(1)
            } else {
                (app.active() + n.max(1) - 1) % n.max(1)
            };
            pool_set_track(app, idx);
        }
        _ => {}
    }

    // `gg` completes to the start of the file (mirrors normal mode).
    // Only `g` carries a pending sequence; any other key clears it. (Digits
    // return above, so they never reach here.)
    if !matches!(key.code, KeyCode::Char('g')) {
        app.pending = Pending::None;
    }
}

/// Seek the pool preview to the selected asset's last frame.
fn pool_seek_end(app: &mut App, player: &dyn Mpv) {
    let d = app
        .project
        .media
        .get(pool_sel(app).unwrap_or(0))
        .and_then(|a| a.duration())
        .map(|d| secs(&d))
        .unwrap_or(0.0);
    pool_seek_abs(app, player, pool_last_frame(app, d));
}

/// Play/pause the pool preview; at the end of the file, restart from 0
/// (mpv `--keep-open` holds at EOF, where a plain unpause would stick).
fn pool_toggle_play(app: &mut App, player: &dyn Mpv) {
    if !app.loaded {
        return;
    }
    if app.eof {
        set_pool_pos(app, 0.0);
        player.seek_absolute(0.0);
        player.set_pause(false);
    } else {
        player.toggle_pause();
    }
}

/// Move the list selection by `delta` and load the new asset.
fn pool_move(app: &mut App, player: &dyn Mpv, delta: i64, n: usize) {
    if n == 0 {
        return;
    }
    let cur = pool_sel(app).unwrap_or(0).min(n - 1) as i64;
    pool_select(app, player, (cur + delta).clamp(0, n as i64 - 1) as usize);
}

/// Select a list row and swap the monitor to that asset.
fn pool_select(app: &mut App, player: &dyn Mpv, sel: usize) {
    if pool_sel(app) == Some(sel) && app.loaded {
        return;
    }
    set_pool_sel(app, sel);
    load_pool_preview(app, player);
}

/// Load the selected asset into the monitor as a raw file from its start.
fn load_pool_preview(app: &mut App, player: &dyn Mpv) {
    load_pool_preview_at(app, player, 0.0);
}

/// Load the selected asset at `start` (clamped to its frames). Used by the
/// reopen snapshot as well as a fresh selection.
fn load_pool_preview_at(app: &mut App, player: &dyn Mpv, start: f64) {
    let Some(sel) = pool_sel(app) else { return };
    let Some(asset) = app.project.media.get(sel) else {
        return;
    };
    let path = asset.path.clone();
    let dur = asset.duration().map(|d| secs(&d)).unwrap_or(0.0);
    let last = pool_last_frame(app, dur);
    let start = if start.is_finite() {
        start.clamp(0.0, last)
    } else {
        0.0
    };
    set_pool_pos(app, start);
    app.pool_on_monitor = true;
    app.pool_handoff = None;
    app.pending_pause = None;
    app.eof = false; // a freshly loaded file is not at EOF
    player.load(&path.display().to_string(), Some(start));
}

/// Open the pool: remember the timeline playhead, preview the last row.
fn open_pool(app: &mut App, player: &dyn Mpv) {
    let len = app.project.media.len();
    // Prefer the remembered asset id: an import/removal may have moved rows.
    let sel = app
        .last_pool_asset
        .and_then(|id| app.project.media.iter().position(|a| a.id == id))
        .unwrap_or_else(|| app.last_pool_sel.min(len.saturating_sub(1)));
    let same_asset = app.project.media.get(sel).map(|a| a.id) == app.last_pool_asset;
    let start = if same_asset { app.last_pool_pos } else { 0.0 };
    app.mode = Mode::Pool(PoolState {
        sel,
        pos: start,
        resume: app.playhead,
    });
    load_pool_preview_at(app, player, start);
    app.set_message("media");
}

/// Close the pool: reload the project timeline at the remembered playhead.
/// A `:` line opened from the pool (`i`) leaves the monitor on the pooled
/// file: put the timeline back on it, as `close_pool` does.
fn leave_pool_command(app: &mut App, player: &dyn Mpv) {
    if !app.command_from_pool {
        return;
    }
    app.command_from_pool = false;
    app.pending_pause = Some(true);
    player.set_pause(true);
    app.reload_requested = true;
}

fn close_pool(app: &mut App, player: &dyn Mpv) {
    close_pool_at(app, player, pool_resume(app));
}

/// Close the pool and put the playhead at `at`: the pre-pool position on a
/// manual close, or the frame an insert landed on. Always hands the timeline
/// back **paused**: mpv is often playing at EOF, and an auto-playing EDL
/// would let the cut magnet move the landing.
fn close_pool_at(app: &mut App, player: &dyn Mpv, at: f64) {
    remember_pool(app);
    app.mode = Mode::Normal;
    app.playhead = at.clamp(0.0, app.last_frame());
    app.pending_pause = Some(true);
    // Set it now so the subsequent async `loadfile` starts paused (mpv keeps
    // the pause property; the pending value re-applies it after the load).
    player.set_pause(true);
    // The monitor was on the pooled asset; rebuild the timeline EDL (from the
    // cache unless something changed) and reload when it is ready.
    start_preview(app);
    app.set_message("timeline");
}

/// Seek the pool preview to an absolute asset time (clamped to duration).
fn pool_seek_abs(app: &mut App, player: &dyn Mpv, t: f64) {
    if !app.loaded {
        return;
    }
    let d = app
        .project
        .media
        .get(pool_sel(app).unwrap_or(0))
        .and_then(|a| a.duration())
        .map(|d| secs(&d))
        .unwrap_or(f64::INFINITY);
    // The last *frame* sits one frame before the duration; seeking to the
    // exact end lands past the final frame (mpv holds there and reports a
    // couple frames short). Clamp to the last frame's timestamp.
    let last = pool_last_frame(app, d);
    let target = t.clamp(0.0, last);
    set_pool_pos(app, target);
    player.seek_absolute(target);
}

/// Timestamp of the selected asset's last frame (`duration − 1 frame`).
fn pool_last_frame(app: &App, dur: f64) -> f64 {
    let fps = app
        .project
        .media
        .get(pool_sel(app).unwrap_or(0))
        .and_then(|a| a.fps())
        .map(|f| secs(&f))
        .unwrap_or(30.0);
    (dur - 1.0 / fps).max(0.0)
}

fn pool_seek_rel(app: &mut App, player: &dyn Mpv, delta: f64) {
    pool_seek_abs(app, player, pool_pos(app) + delta);
}

/// Frame-step the pool preview: move on the asset's frame grid and seek
/// frame-exactly (same model-authoritative approach as normal mode).
fn pool_step(app: &mut App, player: &dyn Mpv, dir: i64) {
    if !app.loaded {
        return;
    }
    let (target, _fps) = pool_step_target(app, dir);
    set_pool_pos(app, target);
    // Nudge a hair into the frame so a frame-exact seek lands ON it, not
    // the previous frame (same trick normal mode uses).
    player.seek_absolute(target + 0.004);
}

/// The frame-grid target of a pool step (`dir` frames) and the asset fps.
/// Pure: snaps to the grid so repeated steps don't drift; clamps to
/// `[0, last frame]`.
fn pool_step_target(app: &App, dir: i64) -> (f64, f64) {
    let sel = pool_sel(app).unwrap_or(0);
    let fps = app
        .project
        .media
        .get(sel)
        .and_then(|a| a.fps())
        .map(|f| secs(&f))
        .unwrap_or(30.0);
    let dur = app
        .project
        .media
        .get(sel)
        .and_then(|a| a.duration())
        .map(|d| secs(&d))
        .unwrap_or(f64::INFINITY);
    let last = pool_last_frame(app, dur);
    let cur = (pool_pos(app) * fps).round() as i64;
    let idx = (cur + dir).clamp(0, (last * fps).round() as i64);
    (idx as f64 / fps, fps)
}

/// Undo the last edit, restoring the playhead to where it was made.
fn do_undo(app: &mut App, player: &dyn Mpv) {
    run_action(app, player, &Action::Undo);
}

/// Redo the last undone edit.
fn do_redo(app: &mut App, player: &dyn Mpv) {
    run_action(app, player, &Action::Redo);
}

/// Insert a clip from the selected pool asset at the playhead (ripple).
fn insert_from_pool(app: &mut App, player: &dyn Mpv) {
    let Some(asset) = app.project.media.get(pool_sel(app).unwrap_or(0)) else {
        app.set_message("no asset selected");
        return;
    };
    let dur = asset.duration();
    // Use a whole-frame duration: an asset like blue.mp4 (6.006 s) trimmed
    // to its exact duration would leave a sub-frame stump if a later edit
    // (a split, a paste) lands near the end — floor to the frame grid so
    // inserted clips are clean.
    let whole = match (dur, app.project.fps.as_ref()) {
        (Some(d), Some(fps)) => {
            let frames = (secs(&d) * secs(fps)).floor().max(1.0);
            Seconds::new(frames as i64, 1) / *fps
        }
        (Some(d), None) => d,
        (None, _) => {
            app.set_message("asset has no known duration");
            return;
        }
    };
    let snapshot = TimelineSegment::Clip(Clip {
        id: 0,
        asset: asset.id,
        asset_in: Seconds::from_integer(0),
        asset_out: whole,
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let inserted = run_action(
        app,
        player,
        &Action::Paste {
            track: app.active(),
            at: app.playhead,
            ripple: true,
            segment: Some(snapshot),
        },
    );
    if inserted {
        // Inserting is the pool's job: leave it so the monitor returns to the
        // timeline, keeping the frame the insert landed on.
        let landed = app.playhead;
        close_pool_at(app, player, landed);
    }
}

/// Remove the selected asset from the pool, undoably (the guard refuses if
/// it's in use), then refresh the pool preview.
fn remove_from_pool(app: &mut App, player: &dyn Mpv) {
    let Some(asset) = app.project.media.get(pool_sel(app).unwrap_or(0)) else {
        app.set_message("no asset selected");
        return;
    };
    run_action(app, player, &Action::AssetRm { id: asset.id });
}

/// The `:` commands, shown by `:help` and in the Command-mode cheat-sheet.
/// The edit commands are the CLI's, flag for flag (`--at` defaults to the
/// playhead here).
const COMMAND_HELP: &str = "commands — :trim-in|out · :split · :gap · :transition · :effect · :delete · :yank · :paste · :extend · :speed · :undo · :redo · :seek · :track · :media · :cache · :import · :add · :show · :purge · :detect · :edl-import · :otio-import · :export · :w · :q";

/// `:import PATH…` — probe files and add them to the media pool. An
/// out-of-band pool write, so it re-baselines the undo sidecar (a stale
/// `Change::Assets` entry would otherwise refuse to apply).
fn import_command(app: &mut App, cmd: &str) -> String {
    let rest = cmd.strip_prefix("import").unwrap_or("").trim();
    if rest.is_empty() {
        return "usage: :import PATH [PATH…]".to_string();
    }
    let playhead = frame_of(app.project.fps.as_ref(), app.playhead);
    let mut new = 0usize;
    let mut existing = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for arg in rest.split_whitespace() {
        match PathBuf::from(arg).canonicalize() {
            Ok(path) => {
                if app.project.asset_by_path(&path).is_some() {
                    existing += 1;
                    continue;
                }
                match nle_ffmpeg::probe(&path) {
                    Ok(report) => {
                        app.history
                            .add_asset(&mut app.project, path, report, playhead);
                        new += 1;
                    }
                    Err(e) => failed.push(format!("{arg}: {e}")),
                }
            }
            Err(_) => failed.push(format!("{arg}: not found")),
        }
    }
    if new > 0 {
        // Recorded pool additions (`u` removes them again). The timeline is
        // untouched, so no preview rebuild is needed.
        if let Err(e) = app.project.save(&app.path) {
            return format!("imported {new} — save failed: {e}");
        }
        if let Err(e) = app.history.save(&History::sidecar_path(&app.path)) {
            return format!("imported {new} — history save failed: {e}");
        }
        app.note_seen();
    }
    let mut msg = format!("imported {new} new, {existing} already pooled");
    if !failed.is_empty() {
        msg.push_str(&format!(" — failed: {}", failed.join("; ")));
    }
    msg
}

// ------------------------------------------------------------ io commands

/// The front-end's own (non-edit) commands: queries, imports, and the two
/// long renders. Everything the *model* can do is an [`Action`] above;
/// these need processes (ffprobe/ffmpeg) or print state. Same flags as
/// the CLI, via the shared tokenizer.
fn io_command(app: &mut App, player: &dyn Mpv, cmd: &str) -> Option<String> {
    let (verb, mut f) = match action::split_args(cmd) {
        Ok(v) => v,
        Err(e) => return Some(e.to_string()),
    };
    match verb.as_str() {
        "import" => Some(import_command(app, cmd)),
        "show" => Some(show_command(app)),
        "media" => Some(media_ls_command(app)),
        "track" => Some(track_ls_command(app)),
        "cache" => cache_command(&mut f),
        "reload" => {
            app.reload_requested = true;
            None
        }
        "add" => add_command(app, player, &mut f),
        "seek" => seek_command(app, player, &mut f),
        "export" => export_command(app, &mut f),
        "detect" => detect_command(app, &mut f),
        "edl-import" => edl_import_command(app, &mut f),
        "otio-import" => otio_import_command(app, &mut f),
        other => Some(format!("unknown command: {other}  (try :help)")),
    }
}

/// A repeatable option value, parsed: `--at T`-style flags the io
/// commands accept. `None` = absent.
fn flag_time(f: &mut action::Flags, name: &str) -> Result<Option<f64>, String> {
    match f.take(name) {
        Some(v) => nle_core::parse_signed_time(&v)
            .map(|t| Some(secs(&t)))
            .map_err(|e| format!("invalid --{name} {v:?} ({e})")),
        None => Ok(None),
    }
}

fn flag_usize(f: &mut action::Flags, name: &str) -> Result<Option<usize>, String> {
    match f.take(name) {
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| format!("--{name} expects a number, got {v:?}")),
        None => Ok(None),
    }
}

/// `:add FILE [--at T] [--in T --out T] [--track N]` — pool the file and
/// ripple-insert it (the CLI's `nle add`; here `--at` defaults to the
/// playhead, like every other command).
fn add_command(app: &mut App, player: &dyn Mpv, f: &mut action::Flags) -> Option<String> {
    let file = match f.positional() {
        Some(p) => PathBuf::from(p),
        None => return Some("usage: :add FILE [--at T] [--in T --out T] [--track N]".to_string()),
    };
    let mut track = app.active();
    let mut at = app.playhead;
    let mut in_point = None;
    let mut out_point = None;
    match (|| -> Result<(), String> {
        if let Some(n) = flag_usize(f, "track")? {
            track = crate::cli_index(n, "--track")?;
        }
        at = flag_time(f, "at")?.unwrap_or(at);
        in_point = f.take("in");
        out_point = f.take("out");
        f.finish(":add FILE [--at T] [--in T --out T] [--track N]")
            .map_err(|e| e.to_string())
    })() {
        Ok(()) => {}
        Err(e) => return Some(e),
    }
    let playhead = frame_of(app.project.fps.as_ref(), app.playhead);
    let Some(kind) = app.project.timeline.track(track).map(|t| t.kind) else {
        return Some(format!("no track #{}", track + 1));
    };
    let (scratch, imported) = match crate::stage_clip(
        &mut app.project,
        &mut app.history,
        &file,
        in_point.as_deref(),
        out_point.as_deref(),
        playhead,
        kind,
    ) {
        Ok(v) => v,
        Err(e) => return Some(format!("add failed: {e}")),
    };
    if imported {
        // A recorded pool addition lands on disk before the edit does, so
        // undo peels the insert and then the import.
        if let Err(e) = app.project.save(&app.path) {
            return Some(format!("save failed: {e}"));
        }
        if let Err(e) = app.history.save(&History::sidecar_path(&app.path)) {
            return Some(format!("history save failed: {e}"));
        }
        app.note_seen();
    }
    run_action(
        app,
        player,
        &Action::Paste {
            track,
            at,
            ripple: true,
            segment: Some(scratch),
        },
    );
    None
}

/// `:seek T` — move the playhead (and the monitor) to T. The stored
/// playhead follows immediately, so a shell and the TUI share one cursor
/// (`nle seek` reads the same slot).
fn seek_command(app: &mut App, player: &dyn Mpv, f: &mut action::Flags) -> Option<String> {
    let at = match f.positional().or_else(|| f.take("at")) {
        Some(v) => v,
        None => return Some("usage: :seek T".to_string()),
    };
    if let Err(e) = f.finish(":seek T") {
        return Some(e.to_string());
    }
    let t = match nle_core::parse_signed_time(&at) {
        Ok(t) => secs(&t),
        Err(e) => return Some(format!("invalid time {at:?} ({e})")),
    };
    // Same clamp as every other seek: onto the timeline's last frame.
    app.seek_to(player, t.clamp(0.0, app.last_frame()));
    match persist_playhead(app) {
        Ok(true) => {}
        Ok(false) => {
            app.reload_pending = true;
            return Some("not saved: nle.json changed on disk — reloading".to_string());
        }
        Err(e) => return Some(format!("save failed: {e}")),
    }
    Some(format!(
        "playhead → {}",
        fmt_timecode(app.playhead, app.project.fps.as_ref())
    ))
}

/// `:export -o FILE [--encoder cpu|vaapi|qsv] [--crf N] [--preset P]
/// [--bitrate B]` — render in a worker; the monitor keeps playing and the
/// notice lands when ffmpeg finishes.
fn export_command(app: &mut App, f: &mut action::Flags) -> Option<String> {
    const USAGE: &str =
        "usage: :export -o FILE [--encoder cpu|vaapi|qsv] [--crf N] [--preset P] [--bitrate B]";
    let output = match f.take("out").or_else(|| f.positional()) {
        Some(o) => PathBuf::from(o),
        None => return Some(USAGE.to_string()),
    };
    let encoder = match f.take("encoder").as_deref() {
        None | Some("cpu") => crate::Encoder::Cpu,
        Some("vaapi") => crate::Encoder::Vaapi,
        Some("qsv") => crate::Encoder::Qsv,
        Some(other) => return Some(format!("--encoder expects cpu|vaapi|qsv, got {other:?}")),
    };
    let crf = f.take("crf").and_then(|s| s.parse().ok()).unwrap_or(18);
    let preset = f.take("preset").unwrap_or_else(|| "fast".to_string());
    let bitrate = f.take("bitrate").unwrap_or_else(|| "8M".to_string());
    if let Err(e) = f.finish(USAGE) {
        return Some(e.to_string());
    }
    let notice = format!("exporting to {}…", output.display());
    let Some(job) = start_job(app, "export", false) else {
        return Some("a job is already running — wait for it".to_string());
    };
    let project_path = app.path.clone();
    std::thread::spawn(move || {
        let result = crate::run_export(&project_path, &output, encoder, crf, &preset, &bitrate)
            .map_err(|e| e.to_string());
        let _ = job.send(result);
    });
    Some(notice)
}

/// `:detect FILE [--threshold T] [--min-shot S] [--black-min S]
/// [--no-gaps] [--fps R] [--video-only] [--list] [--append] [--track N]
/// [--force]` — the CLI's `detect-edits`, in a worker (it can chew
/// through minutes of footage). The project reloads when it lands.
fn detect_command(app: &mut App, f: &mut action::Flags) -> Option<String> {
    const USAGE: &str = "usage: :detect FILE [--append] [--list] [--threshold T] [--force]";
    let file = match f.positional() {
        Some(p) => PathBuf::from(p),
        None => return Some(USAGE.to_string()),
    };
    let threshold = f
        .take("threshold")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.4);
    let min_shot = f.take("min-shot").unwrap_or_else(|| "0.5".to_string());
    let black_min = f.take("black-min").unwrap_or_else(|| "0.1".to_string());
    let no_gaps = f.take_bool("no-gaps");
    let fps = f.take("fps");
    let video_only = f.take_bool("video-only");
    let list = f.take_bool("list");
    let append = f.take_bool("append");
    let track = flag_usize(f, "track").ok().flatten();
    let force = f.take_bool("force");
    if let Err(e) = f.finish(USAGE) {
        return Some(e.to_string());
    }
    let reload = !list;
    let notice = format!("detecting cuts in {}…", file.display());
    let Some(job) = start_job(app, "detect-edits", reload) else {
        return Some("a job is already running — wait for it".to_string());
    };
    let project_path = app.path.clone();
    std::thread::spawn(move || {
        let result = crate::detect_edits_work(crate::DetectArgs {
            project: &project_path,
            file: &file,
            threshold,
            min_shot: &min_shot,
            black_min: &black_min,
            no_gaps,
            fps: fps.as_deref(),
            video_only,
            list,
            append,
            track,
            force,
        })
        .map_err(|e| e.to_string())
        .map(|r| {
            for w in &r.warnings {
                crate::log::line(w);
            }
            r.summary
        });
        let _ = job.send(result);
    });
    Some(notice)
}

/// `:edl-import FILE --media DIR [--fps R] [--src-offset TC] [--map
/// REEL=PATH] [--force]` — build a project from a CMX3600 EDL (the CLI's
/// `import-edl`); the open project reloads when it lands.
fn edl_import_command(app: &mut App, f: &mut action::Flags) -> Option<String> {
    const USAGE: &str = "usage: :edl-import FILE --media DIR [--fps R] [--src-offset TC] [--map REEL=PATH] [--force]";
    let file = match f.positional() {
        Some(p) => PathBuf::from(p),
        None => return Some(USAGE.to_string()),
    };
    let media = match f.take("media") {
        Some(d) => PathBuf::from(d),
        None => return Some(USAGE.to_string()),
    };
    let fps = f.take("fps");
    let src_offset = f
        .take("src-offset")
        .unwrap_or_else(|| "00:00:00:00".to_string());
    let map = f.take_all("map");
    let force = f.take_bool("force");
    if let Err(e) = f.finish(USAGE) {
        return Some(e.to_string());
    }
    let notice = format!("importing {}…", file.display());
    let Some(job) = start_job(app, "edl-import", true) else {
        return Some("a job is already running — wait for it".to_string());
    };
    let project_path = app.path.clone();
    std::thread::spawn(move || {
        let result = crate::import_edl_work(
            &project_path,
            &file,
            &media,
            fps.as_deref(),
            &src_offset,
            &map,
            force,
        )
        .map_err(|e| e.to_string())
        .map(|r| {
            for w in &r.warnings {
                crate::log::line(w);
            }
            r.summary
        });
        let _ = job.send(result);
    });
    Some(notice)
}

/// `:otio-import FILE [--media DIR] [--fps R] [--remap OLD=NEW]
/// [--force]` — build a project from an OpenTimelineIO timeline (the
/// CLI's `import-otio`); the open project reloads when it lands.
fn otio_import_command(app: &mut App, f: &mut action::Flags) -> Option<String> {
    const USAGE: &str =
        "usage: :otio-import FILE [--media DIR] [--fps R] [--remap OLD=NEW] [--force]";
    let file = match f.positional() {
        Some(p) => PathBuf::from(p),
        None => return Some(USAGE.to_string()),
    };
    let media = f.take("media").map(PathBuf::from);
    let fps = f.take("fps");
    let remap = f.take_all("remap");
    let force = f.take_bool("force");
    if let Err(e) = f.finish(USAGE) {
        return Some(e.to_string());
    }
    let notice = format!("importing {}…", file.display());
    let Some(job) = start_job(app, "otio-import", true) else {
        return Some("a job is already running — wait for it".to_string());
    };
    let project_path = app.path.clone();
    std::thread::spawn(move || {
        let result = crate::import_otio_work(
            &project_path,
            &file,
            media.as_deref(),
            fps.as_deref(),
            &remap,
            force,
        )
        .map_err(|e| e.to_string())
        .map(|r| {
            for w in &r.warnings {
                crate::log::line(w);
            }
            r.summary
        });
        let _ = job.send(result);
    });
    Some(notice)
}

/// Arm a background job (one at a time) and hand back its sender.
fn start_job(
    app: &mut App,
    label: &'static str,
    reload: bool,
) -> Option<std::sync::mpsc::Sender<Result<String, String>>> {
    if app.job.is_some() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    app.job = Some(Job { label, reload, rx });
    Some(tx)
}

/// Let a finished worker report in: swap its result into the message
/// line, and reload the project when the job rewrote it.
fn poll_job(app: &mut App, player: &dyn Mpv) {
    use std::sync::mpsc::TryRecvError;
    let Some((label, reload, res)) = app
        .job
        .as_ref()
        .map(|j| (j.label, j.reload, j.rx.try_recv()))
    else {
        return;
    };
    match res {
        Ok(Ok(msg)) => {
            app.job = None;
            if reload && let Err(e) = reload_project(app, player, true) {
                app.set_message(&format!("reload failed: {e}"));
            }
            app.set_message(&msg);
        }
        Ok(Err(e)) => {
            app.job = None;
            app.set_message(&format!("{label} failed: {e}"));
        }
        Err(TryRecvError::Disconnected) => {
            app.job = None;
            app.set_message(&format!("{label} failed: worker died"));
        }
        Err(TryRecvError::Empty) => {}
    }
}

/// Rebuild the monitor's EDL at the current playhead, off-thread: a proxy
/// render is seconds of ffmpeg, and blocking the key handler froze the UI.
/// A second edit while one runs just marks it dirty — the finished render
/// still warms the cache — and the newest build follows.
fn start_preview(app: &mut App) {
    if !app.preview_enabled {
        return;
    }
    if app.preview.is_some() {
        app.preview_dirty = true;
        return;
    }
    let project = app.project.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    app.preview = Some(PreviewJob {
        rx,
        started: Instant::now(),
    });
    app.preview_dirty = false;
    app.preview_progress = Some(PreviewProgress::default());
    std::thread::spawn(move || {
        let progress_tx = tx.clone();
        let mut report = move |done, total, frac| {
            let _ = progress_tx.send(PreviewMsg::Progress { done, total, frac });
        };
        let result =
            preview::build_composite_edl_with(&project, &mut report).map_err(|e| e.to_string());
        let _ = tx.send(PreviewMsg::Done(result));
    });
}

/// Drain the preview worker: progress feeds the status row, and a completed
/// (non-stale) build loads the monitor at wherever the playhead is now. While
/// the pool is open its asset keeps the monitor — `close_pool` rebuilds.
fn poll_preview(app: &mut App, player: &dyn Mpv) {
    use std::sync::mpsc::TryRecvError;
    loop {
        let Some(res) = app.preview.as_ref().map(|j| j.rx.try_recv()) else {
            return;
        };
        match res {
            Ok(PreviewMsg::Progress { done, total, frac }) => {
                app.preview_progress = Some(PreviewProgress { done, total, frac });
            }
            Ok(PreviewMsg::Done(result)) => {
                let stale = app.preview_dirty;
                let pool = is_pool(app);
                let extending = extend_state(app).is_some();
                app.preview = None;
                app.preview_progress = None;
                if stale {
                    app.preview_dirty = false;
                    if !pool && !extending {
                        start_preview(app);
                    }
                } else if !pool && !extending {
                    // Extend mode owns the monitor like the pool does: over
                    // the material preview it handed to mpv, whose clock is
                    // translated through `preview_off`. A timeline build
                    // finishing mid-session must not replace that picture
                    // (`exit_extend` reloads when extend leaves).
                    match result {
                        Ok(url) => {
                            // Keep ignoring the pooled clock until the EDL's
                            // own file reports the landing (see `sync`).
                            if app.pool_on_monitor {
                                app.pool_handoff = Some(app.playhead);
                            }
                            player.load(&url, Some(app.playhead));
                            if let Some(paused) = app.pending_pause.take() {
                                player.set_pause(paused);
                            }
                        }
                        Err(e) => preview_failed(app, &e.to_string()),
                    }
                }
                return;
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                app.preview = None;
                app.preview_progress = None;
                preview_failed(app, "worker died");
                return;
            }
        }
    }
}

/// Keep the timeline's notice: a monitor problem must not erase what
/// happened to the model (the same rule as the old inline rebuild).
/// A failed rebuild during a hand-off (pool close, extend exit/commit)
/// also gives the hand-off up: the timeline EDL will never land, and
/// `sync` gates on `pool_on_monitor`, so the playhead would ignore mpv
/// forever. The monitor keeps the old (paused) file; the next action's
/// rebuild fixes the picture — and this notice says what didn't.
/// In the pool the pooled asset legitimately owns the monitor: there the
/// flag stands.
fn preview_failed(app: &mut App, why: &str) {
    if !is_pool(app) {
        app.pool_on_monitor = false;
        app.pool_handoff = None;
    }
    let msg = if app.message.is_empty() {
        format!("preview failed: {why}")
    } else {
        format!("{} — preview failed: {why}", app.message)
    };
    app.set_message(&msg);
}

/// The `:` parser's defaults: the playhead, the active track, and the
/// clip under the playhead — so `:delete`, `:split`, `:speed` need no
/// `--at`/`--track`/`--clip` (the CLI passes every flag explicitly).
fn parse_defaults(app: &App) -> ParseDefaults {
    ParseDefaults {
        at: Some(app.playhead),
        track: app.active(),
        clip: clip_under(&app.project, app.playhead, app.active()).map(|c| c.id),
    }
}

fn command_key(app: &mut App, player: &dyn Mpv, buf: String, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let cmd = buf.trim().to_string();
            let quitting = matches!(cmd.as_str(), "q" | "q!" | "quit" | "wq");
            if let Some(msg) = run_command(app, player, &cmd) {
                app.set_message(&msg);
            }
            if !quitting {
                app.mode = Mode::Normal;
                leave_pool_command(app, player);
            }
            // else: keep the command text visible while exiting
        }
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            leave_pool_command(app, player);
        }
        KeyCode::Backspace => {
            let mut b = buf;
            b.pop();
            app.mode = Mode::Command(b)
        }
        KeyCode::Char(c) => app.mode = Mode::Command(buf + &c.to_string()),
        _ => app.mode = Mode::Command(buf),
    }
}

/// Run one `:cmd`, returning a notice for the commands that don't set one
/// themselves (built-ins and queries). Every edit is an [`Action`] — the
/// same values the keybinds and the CLI build — so the `:cmd` line and
/// `nle …` can't drift apart.
fn run_command(app: &mut App, player: &dyn Mpv, cmd: &str) -> Option<String> {
    match cmd {
        "" => None,
        "q" | "q!" | "quit" | "wq" => {
            app.quit = true;
            None
        }
        "w" | "write" => Some("edits save automatically — nothing to save".to_string()),
        "help" | "?" => Some(COMMAND_HELP.to_string()),
        _ => match action::parse(cmd, &parse_defaults(app)) {
            Ok(action) => {
                // A new track becomes the active one (where the next
                // edit goes) — `:track add` used to do this inline.
                let is_add = matches!(action, Action::TrackAdd { .. });
                if run_action(app, player, &action) && is_add {
                    app.active_track = app.project.timeline.tracks.len().saturating_sub(1);
                }
                None
            }
            // The verb is an edit, the arguments are not.
            Err(ParseError::Invalid(msg)) => Some(msg),
            // Not an edit action: the front-end's own (query/IO) commands.
            Err(ParseError::Unknown(_)) => io_command(app, player, cmd),
        },
    }
}

/// `:show` — the summary line; the full segment dump goes to the log.
fn show_command(app: &App) -> String {
    let report = crate::show_report(&app.project);
    crate::log::line(&report);
    report.lines().next().unwrap_or_default().to_string()
}

/// `:media ls` — pool summary; the per-asset listing goes to the log.
fn media_ls_command(app: &App) -> String {
    let used = app
        .project
        .media
        .iter()
        .filter(|a| !app.project.asset_usage(a.id).is_empty())
        .count();
    for a in &app.project.media {
        crate::log::line(&format!("asset #{} {}", a.id, a.path.display()));
    }
    format!("{} asset(s) · {used} in use", app.project.media.len())
}

/// `:track ls` — one line per track (the label carries the number).
fn track_ls_command(app: &App) -> String {
    app.project
        .timeline
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mute = if t.muted { " (muted)" } else { "" };
            format!(
                "{} {} seg{mute}",
                track_label(&app.project, i),
                t.segments.len()
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `:cache prune [--days N] [--dry-run]` — delete old proxy renders. Not an
/// edit: it touches no project state and has no undo.
fn cache_command(f: &mut action::Flags) -> Option<String> {
    const USAGE: &str = "usage: :cache prune [--days N] [--dry-run]";
    let Some(verb) = f.positional() else {
        return Some(USAGE.to_string());
    };
    if verb != "prune" {
        return Some(format!("unknown cache command: {verb} — {USAGE}"));
    }
    let days = match f.take("days") {
        Some(v) => match v.parse::<u64>() {
            Ok(d) if d > 0 => d,
            _ => return Some("--days expects a whole number of days (1 or more)".to_string()),
        },
        None => 30,
    };
    let dry_run = f.take_bool("dry-run");
    if let Err(e) = f.finish(USAGE) {
        return Some(e.to_string());
    }
    match crate::proxy::prune(Duration::from_secs(days.saturating_mul(86_400)), dry_run) {
        Ok(report) => Some(report.summary(dry_run)),
        Err(e) => Some(format!("prune failed: {e}")),
    }
}

/// The clip ending exactly at `t` — the left neighbour at a cut whose right
/// side is a gap (where `clip_under` finds no clip).
fn clip_ending_at(project: &Project, t: f64, ti: usize) -> Option<&Clip> {
    let track = project.timeline.track(ti)?;
    let mut acc = 0.0;
    for seg in &track.segments {
        acc += secs(&seg.duration());
        if (acc - t).abs() < 1e-6 {
            return match seg {
                TimelineSegment::Clip(c) => Some(c),
                _ => None,
            };
        }
    }
    None
}

/// `x` — open the effects panel on the clip under the playhead (or the clip
/// ending there, so `xx` works at a clip|gap cut). Effects are video filters,
/// so an audio clip gets the model's own refusal.
fn open_fx(app: &mut App) {
    let clip = clip_under(&app.project, app.playhead, app.active())
        .or_else(|| clip_ending_at(&app.project, app.playhead, app.active()))
        .cloned();
    let Some(clip) = clip else {
        app.set_message("move the playhead onto a clip to see its effects");
        return;
    };
    if app
        .project
        .timeline
        .track(app.active())
        .is_some_and(|t| t.kind != TrackKind::Video)
    {
        app.set_message(&crate::edit_message(&EditError::EffectsNeedVideo));
        return;
    }
    app.mode = Mode::Fx(FxState {
        clip: clip.id,
        sel: 0,
        form: None,
    });
    app.set_message("effects");
}

/// The blend at the playhead on the active track: id, start, kind, frames.
fn transition_target(app: &App) -> Option<(u32, f64, TransitionKind, i64)> {
    let fps = app.project.fps?;
    let (_, tr, start) = edit::transition_at(&app.project, app.active(), app.playhead)
        .ok()
        .flatten()?;
    let frames = (tr.dur * fps).round().to_integer();
    Some((tr.id, secs_f64(&start), tr.kind, frames))
}

/// Open the transition length form, optionally pre-filled with a count.
fn open_transition_form(app: &mut App, prefill: Option<i64>) {
    let Some((id, start, kind, current)) = transition_target(app) else {
        app.set_message("move the playhead onto a transition to change its length");
        return;
    };
    app.mode = Mode::Transition(TransitionForm {
        id,
        start,
        kind,
        frames: prefill.map(|n| n.to_string()).unwrap_or_default(),
        current,
    });
    app.set_message(&format!("transition #{id} — enter frames, enter applies"));
}

/// Transition form keys: digits edit the frame count; enter applies.
fn transition_form_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.set_message("timeline");
        }
        KeyCode::Enter => apply_transition_form(app, player),
        KeyCode::Backspace => {
            if let Mode::Transition(f) = &mut app.mode {
                f.frames.pop();
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Mode::Transition(f) = &mut app.mode {
                f.frames.clear();
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            if let Mode::Transition(f) = &mut app.mode {
                f.frames.push(c);
            }
        }
        _ => {}
    }
}

fn apply_transition_form(app: &mut App, player: &dyn Mpv) {
    let Some(form) = (match &app.mode {
        Mode::Transition(f) => Some(f.clone()),
        _ => None,
    }) else {
        return;
    };
    let text = form.frames.trim();
    let frames = if text.is_empty() {
        form.current
    } else {
        match text.parse::<i64>() {
            Ok(n) => n,
            Err(_) => {
                app.set_message("frames must be a whole number");
                return;
            }
        }
    };
    if frames < 1 {
        app.set_message("frames must be at least 1");
        return;
    }
    if frames == form.current {
        app.mode = Mode::Normal;
        app.set_message(&format!("transition #{} unchanged", form.id));
        return;
    }
    if run_action(
        app,
        player,
        &Action::Transition {
            track: app.active(),
            at: form.start,
            kind: form.kind,
            len: TransitionLen::Frames(frames),
        },
    ) {
        app.mode = Mode::Normal;
    }
}

/// Close the panel. Unlike the pool it hasn't moved the monitor, so there is
/// nothing to reload.
fn close_fx(app: &mut App) {
    app.mode = Mode::Normal;
    app.set_message("timeline");
}

/// The "add" form for an effect kind. Every input starts blank; the neutral
/// value is shown as "was N" beside it and is used if the field is left blank.
/// Each numeric field's first letter is its jump key.
fn form_for(kind: &'static str) -> FxForm {
    let field = |label, previous: Option<&str>, kind, key: Option<char>| FxField {
        label,
        value: String::new(),
        previous: previous.map(str::to_string),
        kind,
        key,
    };
    let fields = match kind {
        "crop" => vec![
            field("left %", Some("0"), FieldKind::Float, Some('l')),
            field("right %", Some("0"), FieldKind::Float, Some('r')),
            field("top %", Some("0"), FieldKind::Float, Some('t')),
            field("bottom %", Some("0"), FieldKind::Float, Some('b')),
        ],
        "scale" => vec![
            field("width %", Some("100"), FieldKind::Float, Some('w')),
            field("height %", Some("100"), FieldKind::Float, Some('h')),
            // A real field so tab/arrows reach it; `a` jumps here and space
            // toggles it.
            field("aspect lock", None, FieldKind::Toggle, Some('a')),
        ],
        "offset" => vec![
            field("x %", Some("0"), FieldKind::Float, Some('x')),
            field("y %", Some("0"), FieldKind::Float, Some('y')),
        ],
        "eq" => vec![
            field("brightness", Some("0"), FieldKind::Float, Some('b')),
            field("contrast", Some("1"), FieldKind::Float, Some('c')),
            field("saturation", Some("1"), FieldKind::Float, Some('s')),
        ],
        _ => Vec::new(),
    };
    FxForm {
        kind,
        fields,
        focus: 0,
        editing: None,
        // Scale is aspect-locked by default; the App's sticky preference
        // overrides this when the form is opened.
        lock: kind == "scale",
        aspect: 1.0,
    }
}

/// An "edit" form: same fields, but the inputs start blank and each shows the
/// applied value beside it. A blank field keeps that value on apply.
fn form_for_effect(effect: &Effect, index: usize) -> FxForm {
    let mut form = form_for(effect.kind());
    form.editing = Some(index);
    if let Effect::Scale { w, h } = effect
        && *h > 0.0
    {
        form.aspect = *w / *h;
    }
    let values: Vec<String> = match effect {
        Effect::Crop {
            left,
            right,
            top,
            bottom,
        } => [left, right, top, bottom]
            .into_iter()
            .map(|v| v.to_string())
            .collect(),
        Effect::Scale { w, h } => vec![w.to_string(), h.to_string()],
        Effect::Offset { x, y } => vec![x.to_string(), y.to_string()],
        Effect::Eq {
            brightness,
            contrast,
            saturation,
        } => vec![
            brightness.to_string(),
            contrast.to_string(),
            saturation.to_string(),
        ],
        Effect::Lut { path } => vec![path.clone()],
    };
    for (field, previous) in form.fields.iter_mut().zip(values) {
        field.value.clear();
        field.previous = Some(previous);
    }
    form
}

fn open_effect_form(app: &mut App, kind: &'static str) {
    let Some((clip, sel)) = fx_state(app).map(|s| (s.clip, s.sel)) else {
        return;
    };
    // One effect per kind: if this kind is already on the clip, its add key
    // opens that row for editing instead of stacking a duplicate.
    let index = clip_by_id(&app.project, clip).and_then(|c| {
        if c.effects.get(sel).is_some_and(|e| e.kind() == kind) {
            Some(sel)
        } else {
            c.effects.iter().position(|e| e.kind() == kind)
        }
    });
    if let Some(index) = index {
        edit_effect_at(app, clip, index);
        return;
    }
    let mut form = form_for(kind);
    if kind == "scale" {
        form.lock = app.fx_scale_lock;
    }
    set_fx_form(app, Some(form));
    app.set_message(&format!(
        "new {kind} — letters jump, tab moves, enter applies"
    ));
}

/// Enter on a listed effect opens it for editing, prefilled with its values.
fn open_edit_form(app: &mut App) {
    let Some((clip, sel)) = fx_state(app).map(|s| (s.clip, s.sel)) else {
        return;
    };
    edit_effect_at(app, clip, sel);
}

/// Open effect `index` of `clip` as an edit form: blank inputs, old values
/// shown beside them.
fn edit_effect_at(app: &mut App, clip: u32, index: usize) {
    let Some(effect) = clip_by_id(&app.project, clip)
        .and_then(|c| c.effects.get(index))
        .cloned()
    else {
        app.set_message("no effect to edit");
        return;
    };
    if matches!(effect, Effect::Lut { .. }) {
        app.set_message("LUT effects are CLI-only for now — use :effect lut");
        return;
    }
    let kind = effect.kind();
    let mut form = form_for_effect(&effect, index);
    if kind == "scale" {
        form.lock = app.fx_scale_lock;
    }
    set_fx_form(app, Some(form));
    app.set_message(&format!(
        "edit {kind} — blank keeps the old value, enter applies"
    ));
}

/// Does a typed character belong in the focused field?
fn field_accepts(kind: FieldKind, c: char) -> bool {
    match kind {
        FieldKind::Float => c.is_ascii_digit() || c == '.' || c == '-',
        FieldKind::Path => !c.is_control(),
        FieldKind::Toggle => false,
    }
}

/// Edit the focused field's string; the borrow ends inside so callers can
/// keep touching `app`.
fn fx_edit_field(app: &mut App, f: impl FnOnce(&mut String)) {
    let Some(st) = fx_state_mut(app) else { return };
    let Some(form) = st.form.as_mut() else { return };
    let Some(field) = form.fields.get_mut(form.focus) else {
        return;
    };
    f(&mut field.value);
}

fn fx_move_focus(app: &mut App, delta: isize) {
    let Some(st) = fx_state_mut(app) else { return };
    let Some(form) = st.form.as_mut() else { return };
    let last = form.fields.len().saturating_sub(1);
    form.focus = form.focus.saturating_add_signed(delta).min(last);
}

/// Focus one field by index (a letter mnemonic).
fn fx_focus(app: &mut App, index: usize) {
    let Some(st) = fx_state_mut(app) else { return };
    let Some(form) = st.form.as_mut() else { return };
    if index < form.fields.len() {
        form.focus = index;
    }
}

/// Flip the scale form's aspect lock (its toggle field is focused).
fn fx_toggle_lock(app: &mut App) {
    let lock = {
        let Some(st) = fx_state_mut(app) else { return };
        let Some(form) = st.form.as_mut() else { return };
        form.lock = !form.lock;
        form.lock
    };
    app.fx_scale_lock = lock;
}

/// With the scale form's lock on, mirror the field being typed into the other
/// axis using the ratio captured when the form opened.
fn fx_sync_aspect(app: &mut App, edited: usize) {
    let Some(st) = fx_state_mut(app) else { return };
    let Some(form) = st.form.as_mut() else { return };
    if !form.lock || form.kind != "scale" || form.fields.len() < 2 || form.aspect <= 0.0 {
        return;
    }
    if edited >= 2 {
        return; // the lock toggle itself
    }
    let Some(value) = form
        .fields
        .get(edited)
        .and_then(|f| f.value.trim().parse::<f64>().ok())
    else {
        return;
    };
    if value <= 0.0 {
        return;
    }
    let synced = if edited == 0 {
        value / form.aspect
    } else {
        value * form.aspect
    };
    if let Some(other) = form.fields.get_mut(1 - edited) {
        other.value = fmt_percent(synced);
    }
}

/// A percentage for a mirrored field: two decimals, no trailing zeros.
fn fmt_percent(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Turn the form's strings into the typed effect and apply it. The values go
/// through `effect_from_args` — the CLI's parser — so the required fields and
/// neutral defaults can't drift between the two front-ends.
fn apply_fx_form(app: &mut App, player: &dyn Mpv) {
    let Some(clip) = fx_state(app).map(|s| s.clip) else {
        return;
    };
    let Some(form) = fx_state(app).and_then(|s| s.form.clone()) else {
        return;
    };
    let effect = match form_effect(&form) {
        Ok(effect) => effect,
        Err(msg) => {
            app.set_message(&msg);
            return;
        }
    };
    // An edit that changes nothing is not a history entry: just close.
    if let Some(i) = form.editing
        && clip_by_id(&app.project, clip).and_then(|c| c.effects.get(i)) == Some(&effect)
    {
        set_fx_form(app, None);
        app.mode = Mode::Normal;
        app.set_message("effect unchanged");
        return;
    }
    let applied = run_action(
        app,
        player,
        &Action::AddEffect {
            track: app.active(),
            clip,
            effect,
            index: form.editing,
        },
    );
    // Applying is the end of the gesture: leave the panel, keeping the
    // action's notice ("effect on clip #N"). A refused value never reaches
    // here, so the form stays open only while there is something to fix.
    if applied {
        set_fx_form(app, None);
        app.mode = Mode::Normal;
    }
}

fn form_effect(form: &FxForm) -> Result<Effect, String> {
    let value = |i: usize| form.fields.get(i).map_or("", FxField::effective);
    let opt_float = |i: usize, name: &str| -> Result<Option<f64>, String> {
        let v = value(i);
        if v.is_empty() {
            return Ok(None);
        }
        v.parse()
            .map(Some)
            .map_err(|_| format!("{name} must be a number"))
    };
    let args = match form.kind {
        "crop" => EffectArgs {
            left: opt_float(0, "left")?,
            right: opt_float(1, "right")?,
            top: opt_float(2, "top")?,
            bottom: opt_float(3, "bottom")?,
            ..Default::default()
        },
        "scale" => EffectArgs {
            w: opt_float(0, "width")?,
            h: opt_float(1, "height")?,
            ..Default::default()
        },
        "offset" => EffectArgs {
            x: opt_float(0, "x")?,
            y: opt_float(1, "y")?,
            ..Default::default()
        },
        "eq" => EffectArgs {
            brightness: opt_float(0, "brightness")?,
            contrast: opt_float(1, "contrast")?,
            saturation: opt_float(2, "saturation")?,
            ..Default::default()
        },
        other => return Err(format!("unknown effect {other:?}")),
    };
    effect_from_args(form.kind, args).map_err(|e| e.to_string())
}

/// Keys while the add form is open: type into the focused field, tab/enter to
/// move, enter on the last applies, esc cancels.
fn fx_form_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    let Some(form) = fx_state(app).and_then(|s| s.form.clone()) else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            set_fx_form(app, None);
            app.set_message("effects");
        }
        KeyCode::Tab | KeyCode::Down => fx_move_focus(app, 1),
        KeyCode::BackTab | KeyCode::Up => fx_move_focus(app, -1),
        // Enter is the commit: it validates every field and applies. Only tab
        // (or the arrows) moves between fields.
        KeyCode::Enter => apply_fx_form(app, player),
        KeyCode::Backspace => {
            fx_edit_field(app, |v| {
                v.pop();
            });
            fx_sync_aspect(app, form.focus);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            fx_edit_field(app, String::clear);
        }
        // Space flips the focused toggle (the scale form's lock).
        KeyCode::Char(' ')
            if form
                .fields
                .get(form.focus)
                .is_some_and(|f| f.kind == FieldKind::Toggle) =>
        {
            fx_toggle_lock(app);
        }
        KeyCode::Char(c) => {
            let focused = form.fields.get(form.focus);
            // A focused toggle has no text: its own jump key flips it.
            if focused.is_some_and(|f| f.kind == FieldKind::Toggle)
                && focused.is_some_and(|f| f.key == Some(c.to_ascii_lowercase()))
            {
                fx_toggle_lock(app);
                return;
            }
            // In a numeric form the field labels double as jump keys. A path
            // field must receive every letter as text, so it is exempt.
            if focused.is_some_and(|f| f.kind != FieldKind::Path)
                && let Some(i) = form
                    .fields
                    .iter()
                    .position(|f| f.key == Some(c.to_ascii_lowercase()))
            {
                fx_focus(app, i);
                return;
            }
            if focused.is_some_and(|f| field_accepts(f.kind, c)) {
                fx_edit_field(app, |v| v.push(c));
                fx_sync_aspect(app, form.focus);
            }
        }
        _ => {}
    }
}

/// Add a cross-dissolve at the playhead (`X`, or `xx` from Normal — the first
/// `x` opens the panel and the second lands here). A count is the length in
/// frames; without one it is a second.
/// Add a cross-dissolve at the playhead (`X`, or `xx` from Normal — the first
/// `x` opens the panel and the second lands here). Returns whether it landed.
fn add_transition(app: &mut App, player: &dyn Mpv, frames: Option<i64>) -> bool {
    let len = match frames {
        Some(n) => TransitionLen::Frames(n),
        None => TransitionLen::Seconds(1.0),
    };
    run_action(
        app,
        player,
        &Action::Transition {
            track: app.active(),
            at: app.playhead,
            kind: TransitionKind::Dissolve,
            len,
        },
    )
}

/// Effects panel keys: list motion, drop/clear, add (structured form), undo.
fn fx_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    if fx_state(app).is_some_and(|st| st.form.is_some()) {
        fx_form_key(app, player, key);
        return;
    }
    let Some((clip, sel)) = fx_state(app).map(|s| (s.clip, s.sel)) else {
        return;
    };
    let Some(c) = clip_by_id(&app.project, clip) else {
        // The clip vanished (an undo of the insert that made it): leave.
        close_fx(app);
        return;
    };
    let n = c.effects.len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => close_fx(app),
        KeyCode::Char('j') | KeyCode::Down => {
            set_fx_sel(app, (sel + 1).min(n.saturating_sub(1)));
        }
        KeyCode::Char('k') | KeyCode::Up => set_fx_sel(app, sel.saturating_sub(1)),
        // The rows are numbered 1..n: a digit opens that effect directly.
        KeyCode::Char(digit @ '1'..='9') => {
            let row = digit.to_digit(10).unwrap_or(0) as usize;
            if row <= n {
                set_fx_sel(app, row - 1);
                open_edit_form(app);
            } else {
                app.set_message(&format!("no effect #{row}"));
            }
        }
        // Selecting an applied effect opens it for editing (blank keeps the
        // current value); `d`/`D` remain the way to remove.
        KeyCode::Enter => open_edit_form(app),
        KeyCode::Char('d') => {
            if n == 0 {
                app.set_message("no effects to drop");
                return;
            }
            run_action(
                app,
                player,
                &Action::RemoveEffect {
                    track: app.active(),
                    clip,
                    index: Some(sel.min(n - 1)),
                },
            );
            clamp_fx_sel(app);
        }
        KeyCode::Char('D') => {
            if n == 0 {
                app.set_message("no effects to clear");
                return;
            }
            run_action(
                app,
                player,
                &Action::ClearEffects {
                    track: app.active(),
                    clip,
                },
            );
            set_fx_sel(app, 0);
        }
        // Adding asks for the values in labelled fields instead of leaving a
        // `:effect …` line to complete by hand.
        KeyCode::Char('c') => open_effect_form(app, "crop"),
        KeyCode::Char('s') => open_effect_form(app, "scale"),
        KeyCode::Char('e') => open_effect_form(app, "eq"),
        KeyCode::Char('o') => open_effect_form(app, "offset"),
        // `xx`: the first `x` opened the panel; the second applies the
        // transition and exits the panel (a failure keeps it open to retry).
        KeyCode::Char('x') => {
            if transition_target(app).is_some() {
                open_transition_form(app, None);
            } else if add_transition(app, player, None) {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Char('u') => {
            do_undo(app, player);
            clamp_fx_sel(app);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            do_redo(app, player);
            clamp_fx_sel(app);
        }
        _ => {}
    }
}

fn normal_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    let code = key.code;

    // Vim counts: digits build a multiplier; bare `0` jumps to the start.
    if let KeyCode::Char(c @ '0'..='9') = code {
        if c == '0' && app.count.is_none() {
            app.seek_to(player, 0.0);
            return;
        }
        let d = c.to_digit(10).unwrap_or(0);
        app.count = Some(app.count.unwrap_or(0) * 10 + d);
        return;
    }
    let had_count = app.count.is_some();
    let mult = app.count.take().unwrap_or(1);
    let eps = half_frame(&app.project);

    // Second key of a two-key sequence.
    match (&app.pending, code) {
        (Pending::G, KeyCode::Char('g')) => {
            app.pending = Pending::None;
            app.seek_to(player, 0.0);
            return;
        }
        (Pending::D, KeyCode::Char('d')) => {
            app.pending = Pending::None;
            edit_delete(app, player);
            return;
        }
        (Pending::Y, KeyCode::Char('y')) => {
            app.pending = Pending::None;
            edit_yank(app, player);
            return;
        }
        _ => {}
    }

    match code {
        KeyCode::Char(' ') => {
            if app.guard_loaded() {
                if app.eof {
                    // At the end: play restarts from the beginning.
                    player.seek_absolute(0.0);
                    player.set_pause(false);
                } else {
                    player.toggle_pause();
                }
            }
        }
        KeyCode::Char('h') => app.seek_rel(player, -(mult as f64)),
        KeyCode::Char('l') => app.seek_rel(player, mult as f64),
        KeyCode::Char('H') => app.seek_rel(player, -5.0 * mult as f64),
        KeyCode::Char('L') => app.seek_rel(player, 5.0 * mult as f64),
        KeyCode::Char('f') => {
            if app.guard_loaded() {
                for _ in 0..mult {
                    app.step_playhead(1);
                }
                // Model-authoritative: move the monitor to the frame we
                // display, on the PROJECT grid. mpv's own `frame_step` walks
                // the source's grid instead, so a 24/59.94 clip would drift
                // the marker and the monitor apart (and jump the playhead on
                // the next play).
                app.seek_to(player, app.playhead);
            }
        }
        KeyCode::Char('F') => {
            if app.guard_loaded() {
                for _ in 0..mult {
                    app.step_playhead(-1);
                }
                app.seek_to(player, app.playhead);
            }
        }
        KeyCode::Char('j') => {
            match next_cut(&cut_points(&app.project, app.active()), app.playhead, eps) {
                Some(t) => app.seek_to(player, t),
                None => app.set_message("no cut after the playhead"),
            }
        }
        KeyCode::Char('k') => {
            match prev_cut(&cut_points(&app.project, app.active()), app.playhead, eps) {
                Some(t) => app.seek_to(player, t),
                None => app.set_message("no cut before the playhead"),
            }
        }
        KeyCode::Char('w') => {
            match next_clip_start(&app.project, app.playhead, eps, app.active()) {
                Some(t) => app.seek_to(player, t),
                None => app.set_message("no clip after the playhead"),
            }
        }
        KeyCode::Char('b') => {
            match prev_clip_start(&app.project, app.playhead, eps, app.active()) {
                Some(t) => app.seek_to(player, t),
                None => app.set_message("no clip before the playhead"),
            }
        }
        KeyCode::Char('n') => match next_gap_start(&app.project, app.playhead, eps, app.active()) {
            Some(t) => app.seek_to(player, t),
            None => app.set_message("no gap after the playhead"),
        },
        KeyCode::Char('N') => match prev_gap_start(&app.project, app.playhead, eps, app.active()) {
            Some(t) => app.seek_to(player, t),
            None => app.set_message("no gap before the playhead"),
        },
        KeyCode::Char('g') => app.pending = Pending::G,
        KeyCode::Char('d') => app.pending = Pending::D,
        KeyCode::Char('y') => app.pending = Pending::Y,
        // ---- editing: uniform playhead-based ops (counts don't multiply) ----
        KeyCode::Char('i') => {
            run_action(
                app,
                player,
                &Action::TrimIn {
                    track: app.active(),
                    at: app.playhead,
                    ripple: false,
                },
            );
        }
        KeyCode::Char('I') => {
            run_action(
                app,
                player,
                &Action::TrimIn {
                    track: app.active(),
                    at: app.playhead,
                    ripple: true,
                },
            );
        }
        KeyCode::Char('o') => {
            run_action(
                app,
                player,
                &Action::TrimOut {
                    track: app.active(),
                    at: app.playhead,
                    ripple: false,
                },
            );
        }
        KeyCode::Char('O') => {
            run_action(
                app,
                player,
                &Action::TrimOut {
                    track: app.active(),
                    at: app.playhead,
                    ripple: true,
                },
            );
        }
        KeyCode::Char('s') => {
            run_action(
                app,
                player,
                &Action::Split {
                    track: app.active(),
                    at: app.playhead,
                },
            );
        }
        KeyCode::Char('_') => {
            run_action(
                app,
                player,
                &Action::InsertGap {
                    track: app.active(),
                    at: app.playhead,
                    len: GapLen::Frames(mult as i64),
                },
            );
        }
        KeyCode::Char('D') => {
            edit_yank(app, player); // D yanks too
            run_action(
                app,
                player,
                &Action::Delete {
                    track: app.active(),
                    target: Target::At(app.playhead),
                    ripple: true,
                },
            );
        }
        KeyCode::Char('p') => {
            run_action(
                app,
                player,
                &Action::Paste {
                    track: app.active(),
                    at: app.playhead,
                    ripple: false,
                    segment: None,
                },
            );
        }
        KeyCode::Char('P') => {
            run_action(
                app,
                player,
                &Action::Paste {
                    track: app.active(),
                    at: app.playhead,
                    ripple: true,
                    segment: None,
                },
            );
        }
        KeyCode::Char('u') => do_undo(app, player),
        KeyCode::Char('m') => open_pool(app, player),
        // `x` — the effects panel for the clip under the playhead, or the
        // length form when the playhead is inside a transition (a blend owns
        // no clip's effects).
        KeyCode::Char('x') => {
            if transition_target(app).is_some() {
                open_transition_form(app, None);
            } else {
                open_fx(app);
            }
        }
        // `X` — cross-dissolve at the boundary; a count is the duration in
        // frames (default 1 s). On an existing blend the same key opens its
        // length form, with the count pre-filled.
        KeyCode::Char('X') => {
            if transition_target(app).is_some() {
                open_transition_form(app, had_count.then_some(mult as i64));
            } else {
                add_transition(app, player, had_count.then_some(mult as i64));
            }
        }
        // `t`/`T` cycle the active track; `<N>t` jumps to track N (1-based).
        KeyCode::Char('t') => {
            let n = app.project.timeline.tracks.len();
            let idx = if had_count {
                (mult as usize).saturating_sub(1)
            } else {
                (app.active() + 1) % n.max(1)
            };
            set_active_track(app, idx);
        }
        KeyCode::Char('T') => {
            let n = app.project.timeline.tracks.len();
            let idx = if had_count {
                (mult as usize).saturating_sub(1)
            } else {
                (app.active() + n.max(1) - 1) % n.max(1)
            };
            set_active_track(app, idx);
        }
        KeyCode::Char('e') => enter_extend(app, player, false),
        KeyCode::Char('E') => enter_extend(app, player, true),
        KeyCode::Char('[') => enter_extend_side(app, player, Side::Right),
        KeyCode::Char(']') => enter_extend_side(app, player, Side::Left),
        KeyCode::Char('<') | KeyCode::Char(',') => seek_clip_end(app, player, true),
        KeyCode::Char('>') | KeyCode::Char('.') => seek_clip_end(app, player, false),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => do_redo(app, player),
        // ---- navigation/transport (unchanged) ----
        KeyCode::Char('G') | KeyCode::Char('$') => app.seek_to(player, app.total()),
        KeyCode::Char('R') => app.reload_requested = true,
        KeyCode::Char('q') => app.quit = true,
        // `r` opens the speed prompt for the clip under the playhead; the
        // command line is pre-filled so it reads as `:speed ` (or the
        // transition rate form when the playhead is inside a blend).
        KeyCode::Char('r') => {
            app.mode = Mode::Command(if transition_target(app).is_some() {
                "transition --percent ".to_string()
            } else {
                "speed ".to_string()
            });
        }
        KeyCode::Char(':') => app.mode = Mode::Command(String::new()),
        _ => {}
    }

    // Any key other than a pending-sequence starter (or count digits)
    // cancels a pending two-key sequence.
    if !matches!(
        code,
        KeyCode::Char('g') | KeyCode::Char('d') | KeyCode::Char('y') | KeyCode::Char('0'..='9')
    ) {
        app.pending = Pending::None;
    }
}

// ------------------------------------------------------------ extend mode

/// Plan an extend-mode session: armed boundary, committable range,
/// material span for display, and preview parameters. Pure — the caller
/// realizes it with mpv IPC. Positions below are timeline seconds;
/// `hi == None` means unbounded (gap extension past the end).
struct ExtendPlan {
    track: usize,
    armed: Armed,
    edge_pos: f64,
    lo: f64,
    hi: Option<f64>,
    mat: Option<(f64, f64)>,
    preview: ExtendPreviewSpec,
}

enum ExtendPreviewSpec {
    /// Asset path + asset-relative start/length + timeline origin.
    Clip {
        path: std::path::PathBuf,
        src_start: Seconds,
        length: Seconds,
        origin: f64,
    },
    Black {
        origin: f64,
    },
}

fn plan_extend(
    project: &Project,
    playhead: f64,
    ripple: bool,
    ti: usize,
) -> Result<ExtendPlan, String> {
    let _ = ripple;
    let (armed, edge) = resolve_armed(project, playhead, ti)?;
    plan_for_armed(project, armed, edge, ti)
}

/// Build the plan for a known armed boundary at `edge`. Split from
/// `plan_extend` so extend mode can re-arm to the other side of a shared
/// cut without re-resolving from a playhead.
fn plan_for_armed(
    project: &Project,
    armed: Armed,
    edge: f64,
    ti: usize,
) -> Result<ExtendPlan, String> {
    let track = project
        .timeline
        .track(ti)
        .ok_or_else(|| "empty timeline".to_string())?;

    match armed {
        Armed::Clip { id, head } => {
            let (s, e) = clip_span(project, id, ti).ok_or_else(|| "clip vanished".to_string())?;
            let asset_id = track
                .segments
                .iter()
                .filter_map(|seg| match seg {
                    TimelineSegment::Clip(c) if c.id == id => Some(c),
                    _ => None,
                })
                .next()
                .ok_or_else(|| "clip vanished".to_string())?;
            let asset = project
                .asset(asset_id.asset)
                .ok_or_else(|| format!("asset #{} is missing", asset_id.asset))?;
            let ain = secs(&asset_id.asset_in);
            let aout = secs(&asset_id.asset_out);
            let idx = track
                .clip_index(id)
                .ok_or_else(|| "clip vanished".to_string())?;
            let dur_opt = asset.duration().map(|d| secs(&d));
            // Unknown asset duration: no extension room, trim/delete only,
            // and no material strip to show.
            let (lo, hi, mat) = if head {
                // Head material reaches back to source zero. A segment at
                // the timeline start (s == 0) can extend PAST it: the
                // material is prepended (the segment keeps its start and
                // grows, followers shift right), so its lower bound goes
                // negative by the head room. Anywhere further right is
                // capped at 0 — nothing exists left of the origin.
                let lo = if s <= 0.0 { -ain } else { (s - ain).max(0.0) };
                (lo, Some(e), dur_opt.map(|d| (s - ain, s + (d - ain))))
            } else {
                // Tail material reaches forward to the asset end.
                (
                    s,
                    Some(dur_opt.map(|d| e + (d - aout)).unwrap_or(e)),
                    dur_opt.map(|d| (e - aout, e + (d - aout))),
                )
            };
            // At a blend's far end the edge's material is the transition
            // side's outer source, not the clip's visible in/out: red's tail
            // shows `a.src_out`, blue's head `b.src_in`. Without this the
            // monitor sat one blend-duration off the armed edge.
            let src_at_edge =
                blend_edge_source(track, idx, edge, head).unwrap_or(if head { ain } else { aout });
            // Preview: the whole asset file, so every reachable position
            // shows real frames. mpv 0 maps to the timeline position of
            // source zero (edge minus the edge's source point). Without a
            // known duration, map just the clip's current span.
            let preview = match dur_opt {
                Some(d) => {
                    // Trim the preview to the reachable side so mpv stops
                    // at the segment's edge instead of playing the whole
                    // asset: a head reaches source [0, aout], a tail
                    // [ain, d].
                    let length = if head { aout } else { d };
                    ExtendPreviewSpec::Clip {
                        path: asset.path.clone(),
                        src_start: Seconds::from_integer(0),
                        length: Seconds::new((length * 1e6).round() as i64, 1_000_000),
                        origin: edge - src_at_edge,
                    }
                }
                None => ExtendPreviewSpec::Clip {
                    path: asset.path.clone(),
                    src_start: Seconds::new((ain * 1e6).round() as i64, 1_000_000),
                    length: Seconds::new(((aout - ain) * 1e6).round() as i64, 1_000_000),
                    origin: s,
                },
            };
            Ok(ExtendPlan {
                track: ti,
                armed,
                edge_pos: edge,
                lo,
                hi,
                mat,
                preview,
            })
        }
        Armed::GapTail { index } => {
            let g = match track.segments.get(index) {
                Some(TimelineSegment::Gap(g)) => g,
                _ => return Err("gap vanished".to_string()),
            };
            let gap_start = secs(&track.start_of(index));
            Ok(ExtendPlan {
                track: ti,
                armed,
                edge_pos: gap_start + secs(&g.duration),
                lo: gap_start,
                hi: None,
                mat: None,
                preview: ExtendPreviewSpec::Black { origin: gap_start },
            })
        }
        Armed::GapHead { index } => {
            let g = match track.segments.get(index) {
                Some(TimelineSegment::Gap(g)) => g,
                _ => return Err("gap vanished".to_string()),
            };
            let gap_start = secs(&track.start_of(index));
            // Gaps have no material, so the head reaches back without
            // bound: extending it left inserts empty space (growing the
            // gap), even past time 0. The gap's own end bounds shrinking.
            Ok(ExtendPlan {
                track: ti,
                armed,
                edge_pos: gap_start,
                lo: f64::NEG_INFINITY,
                hi: Some(gap_start + secs(&g.duration)),
                mat: None,
                preview: ExtendPreviewSpec::Black { origin: 0.0 },
            })
        }
    }
}

// ------------------------------------------------------------ edit plumbing

/// The clip the playhead sits inside, if any.
fn clip_under(project: &Project, t: f64, ti: usize) -> Option<&Clip> {
    let track = project.timeline.track(ti)?;
    track.clip_at(grid_time(project, t))
}

/// The clip under `t` and the position of its head (start) or tail edge.
/// Shared by the `<` / `>` handling in both modes.
fn clip_edge_under(project: &Project, t: f64, head: bool, ti: usize) -> Option<(u32, f64)> {
    let c = clip_under(project, t, ti)?;
    let (s, e) = clip_span(project, c.id, ti)?;
    Some((c.id, if head { s } else { e }))
}

/// `<` / `>` (and `,` / `.`) in Normal mode: move the playhead to the
/// head or tail of the clip it is over.
fn seek_clip_end(app: &mut App, player: &dyn Mpv, head: bool) {
    let Some((id, edge)) = clip_edge_under(&app.project, app.playhead, head, app.active()) else {
        app.set_message("playhead is in a gap — clip start/end needs a clip");
        return;
    };
    app.seek_to(player, edge);
    app.set_message(&format!(
        "clip #{id} {}",
        if head { "start" } else { "end" }
    ));
}

/// The clip and source frame under the playhead — the media anchor an
/// edit should preserve. `None` over a gap (or past the end).
#[derive(Debug, Clone, Copy, PartialEq)]
struct MediaAnchor {
    id: u32,
    source: Seconds,
    /// The clip's timeline start before the edit — where a ripple delete
    /// leaves the seam between its neighbours.
    start: f64,
    /// The playhead before the edit — the offset a plain delete keeps
    /// inside the gap it leaves.
    at: f64,
}

/// A position anchor for a gap or transition — neither is media (a gap has no
/// id, a transition is opaque), so the playhead can't be re-found by frame.
/// It is kept where it was while the segment survives; a *removal* ripples and
/// leaves the seam (`start`) between the neighbours.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GapAnchor {
    /// The seam a removal leaves: a gap's start, a transition's end.
    start: f64,
    /// The playhead before the edit — kept while a gap or transition is still
    /// under it (an insert, a trim, an effect elsewhere), so those don't slide
    /// the playhead to the seam.
    at: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Anchor {
    Media(MediaAnchor),
    Gap(GapAnchor),
}

/// The gap or transition whose span *ends* exactly at `t`. `index_at` gives
/// the boundary to the follower, but for anchoring the segment the playhead
/// sits *on* is what matters: a blend's far edge is still the blend, and a
/// trim that moves that edge to the playhead must keep the playhead put.
fn gap_ending_at(track: &Track, t: Seconds) -> Option<usize> {
    let mut acc = Seconds::from_integer(0);
    for (i, s) in track.segments.iter().enumerate() {
        acc += s.duration();
        if acc == t && matches!(s, TimelineSegment::Gap(_) | TimelineSegment::Transition(_)) {
            return Some(i);
        }
    }
    None
}

/// Anchor the playhead's position before an edit: the clip + source frame
/// over a clip, or the gap's start over a gap.
fn edit_anchor(project: &Project, playhead: f64, ti: usize) -> Option<Anchor> {
    let track = project.timeline.track(ti)?;
    let t = grid_time(project, playhead);
    let idx = gap_ending_at(track, t).or_else(|| track.index_at(t))?;
    let at = secs(&t);
    match &track.segments[idx] {
        TimelineSegment::Clip(c) => Some(Anchor::Media(MediaAnchor {
            id: c.id,
            // Timeline → source scales by the clip's speed (a 2× clip's
            // timeline second is two source seconds).
            source: c.source_at(t - track.start_of(idx)),
            start: secs(&track.start_of(idx)),
            at,
        })),
        TimelineSegment::Gap(_) => Some(Anchor::Gap(GapAnchor {
            start: secs(&track.start_of(idx)),
            at,
        })),
        // A transition is removed by handing its trimmed spans back, so the
        // neighbours meet at its *end*; a non-removing edit keeps the playhead.
        TimelineSegment::Transition(tr) => Some(Anchor::Gap(GapAnchor {
            start: secs(&(track.start_of(idx) + tr.dur)),
            at,
        })),
    }
}

#[cfg(test)]
fn clip_anchor(project: &Project, playhead: f64) -> Option<MediaAnchor> {
    match edit_anchor(project, playhead, 0) {
        Some(Anchor::Media(a)) => Some(a),
        _ => None,
    }
}
/// The timeline position of `source` in clip `id`, clamped to the clip's
/// current extent. `None` when the clip is gone.
fn resolve_anchor(project: &Project, id: u32, source: Seconds, ti: usize) -> Option<f64> {
    let track = project.timeline.track(ti)?;
    let mut acc = Seconds::from_integer(0);
    for seg in &track.segments {
        if let TimelineSegment::Clip(c) = seg
            && c.id == id
        {
            let pos = if source < c.asset_in {
                acc
            } else if source >= c.asset_out {
                acc + c.duration()
            } else {
                // Source → timeline divides by speed: the inverse of the
                // mapping `edit_anchor` used to record the media frame.
                acc + c.timeline_offset_of(source)
            };
            return Some(secs(&pos));
        }
        acc += seg.duration();
    }
    None
}

/// After an edit, put the playhead back on the media frame it was on
/// (`anchor`), so a trim that moves material under it keeps the same
/// frame rather than the same timecode.
fn reanchor_playhead(app: &mut App, anchor: Option<Anchor>) {
    let Some(anchor) = anchor else { return };
    let pos = match anchor {
        Anchor::Media(a) => resolve_anchor(&app.project, a.id, a.source, app.active())
            .unwrap_or_else(|| fallback_position(&app.project, &a, app.active())),
        // A gap or transition still under the playhead: an edit that doesn't
        // remove it keeps the playhead put. A removal leaves the seam between
        // the neighbours (the segment under `at` is then a clip).
        Anchor::Gap(g) => {
            let ti = app.active();
            let idx = app.project.timeline.track(ti).and_then(|track| {
                let t = grid_time(&app.project, g.at);
                gap_ending_at(track, t).or_else(|| track.index_at(t))
            });
            match idx {
                Some(i)
                    if !matches!(
                        app.project.timeline.tracks[ti].segments[i],
                        TimelineSegment::Clip(_)
                    ) =>
                {
                    g.at
                }
                _ => g.start,
            }
        }
    };
    let total = app.last_frame();
    app.playhead = pos.clamp(0.0, total);
}

/// Does this action move the active track's media? If not, the playhead stays
/// where it is — an anchor taken against the old active track would otherwise
/// resolve against the wrong one (`track rm` re-indexes).
fn reanchors(action: &Action) -> bool {
    !matches!(
        action,
        Action::TrackAdd { .. }
            | Action::TrackMute { .. }
            | Action::TrackRm { .. }
            | Action::AssetRm { .. }
    )
}

/// Where the playhead lands when the anchored clip is gone. A plain
/// delete replaces it with a gap of the same length, so keep the same
/// offset (`at`); a ripple delete removes it and closes the gap, so land
/// on the cut the neighbours now share (`start`).
fn fallback_position(project: &Project, a: &MediaAnchor, ti: usize) -> f64 {
    if let Some(track) = project.timeline.track(ti)
        && let Some(idx) = track.index_at(grid_time(project, a.at))
        && matches!(track.segments[idx], TimelineSegment::Gap(_))
    {
        return a.at;
    }
    a.start
}

/// The playhead's position on the project's frame grid, as Seconds.
fn grid_time(project: &Project, t: f64) -> Seconds {
    nle_core::grid_time(project.fps.as_ref(), t)
}

/// The trim keys' routing, as the key handlers call it (tests drive the
/// shared [`edit::trim_at_on`] through this shim).
#[cfg(test)]
fn trim_key(
    project: &Project,
    ph: f64,
    out: bool,
    ripple: bool,
    ti: usize,
) -> Result<Batch, EditError> {
    edit::trim_at_on(project, ti, ph, out, ripple)
}

/// Run one catalog action — the single path every edit takes, keybind or
/// `:cmd`: apply through the undo history, reanchor the playhead, persist,
/// refresh the monitor. Returns whether it applied (the action set its own
/// notice either way).
/// What a delete can take away: clip/transition ids and the segment count.
/// `dd` on a clip leaves a gap (the count stays), so ids are needed; deleting
/// a gap keeps the ids, so the count is needed too.
struct TimelineStamp {
    ids: HashSet<u32>,
    segments: usize,
}

fn timeline_stamp(project: &Project) -> TimelineStamp {
    let mut stamp = TimelineStamp {
        ids: HashSet::new(),
        segments: 0,
    };
    for track in &project.timeline.tracks {
        for seg in &track.segments {
            stamp.segments += 1;
            match seg {
                TimelineSegment::Clip(c) => {
                    stamp.ids.insert(c.id);
                }
                TimelineSegment::Transition(t) => {
                    stamp.ids.insert(t.id);
                }
                TimelineSegment::Gap(_) => {}
            }
        }
    }
    stamp
}

/// Did an edit remove material? A clip/transition id vanished, or the segment
/// count fell. Trims that only shorten, splits, and inserts don't.
fn removed_material(before: &TimelineStamp, after: &TimelineStamp) -> bool {
    after.segments < before.segments || before.ids.iter().any(|id| !after.ids.contains(id))
}

fn run_action(app: &mut App, player: &dyn Mpv, action: &Action) -> bool {
    // What the timeline holds before the edit — a removal opens the crocodile's
    // mouth in the status row (the chomp is transient, not a message).
    let stamp = timeline_stamp(&app.project);
    // Keep the playhead on the frame it was showing, not the same
    // timecode (a trim can move material under it). Undo/redo carry their
    // own restore position; an extend commit is made from the armed edge,
    // and lands on the edge it moved to.
    let restore = matches!(action, Action::Undo | Action::Redo);
    let in_extend = matches!(app.mode, Mode::Extend(_));
    let anchor = (!in_extend && !restore)
        .then(|| edit_anchor(&app.project, app.playhead, app.active()))
        .flatten();
    let from = match &app.mode {
        Mode::Extend(st) => st.edge_pos,
        _ => app.playhead,
    };
    match nle_core::run_action(
        &mut app.project,
        &mut app.history,
        &mut app.register,
        action,
        from,
    ) {
        Ok(outcome) => {
            match outcome.playhead {
                Some(frame) => {
                    // Clamp: a redo can restore a position into a shorter
                    // timeline.
                    app.playhead =
                        frame_to_secs(app.project.fps.as_ref(), frame).clamp(0.0, app.last_frame());
                }
                // A no-op (a yank) must not move the playhead: the anchor
                // maps a transition to its *end*, so reanchoring a yank out
                // of a blend would slide the playhead — and `dd` yanks first.
                None if outcome.changed && reanchors(action) => reanchor_playhead(app, anchor),
                // A track or pool change doesn't move the active track's
                // media, so leave the playhead where it is (just clamped): a
                // `track rm` re-indexes, and an anchor taken against the old
                // active would resolve against the wrong track.
                None if outcome.changed => {
                    app.playhead = app.playhead.clamp(0.0, app.last_frame());
                }
                None => {}
            }
            if outcome.changed && removed_material(&stamp, &timeline_stamp(&app.project)) {
                app.chomp_until = Some(Instant::now() + Duration::from_millis(CHOMP_MS));
            }
            app.set_message(&outcome.desc);
            // The register is shared with the CLI (`nle yank`/`nle paste`).
            if save_register(&app.path, app.register.as_ref()).is_ok() {
                app.note_register();
            }
            if outcome.changed {
                persist_and_refresh(app, player);
            }
            true
        }
        Err(e) => {
            app.set_message(&crate::edit_message(&e));
            false
        }
    }
}

/// Atomic save + view refresh. Called after every successful edit (and
/// undo/redo) — no manual save state to get wrong. The undo history sidecar
/// is written alongside the project, then the monitor is reloaded for
/// whichever view is showing: the pool's raw file when the pool is open, or
/// the timeline EDL (with the playhead baked into the load) otherwise.
fn persist_and_refresh(app: &mut App, player: &dyn Mpv) {
    // The stored playhead follows the session — it is what `nle seek`
    // reads and where the next TUI session opens.
    match persist_playhead(app) {
        Ok(true) => {}
        Ok(false) => {
            // A shell wrote the project first: adopt theirs rather than
            // clobber it, and say so — this edit was not saved.
            if let Err(e) = reload_project(app, player, true) {
                app.set_message(&format!("reload failed: {e}"));
                return;
            }
            let prev = app.message.clone();
            app.set_message(&format!(
                "{prev} — not saved: nle.json changed on disk (reloaded)"
            ));
            return;
        }
        Err(e) => {
            app.set_message(&format!("save failed: {e}"));
            return;
        }
    }
    if let Err(e) = app.history.save(&History::sidecar_path(&app.path)) {
        app.set_message(&format!("history save failed: {e}"));
    } else {
        app.note_history();
    }
    if is_pool(app) {
        // A remove/undo changed the list; keep the selection in range and
        // reload the (possibly different) asset. Doing this here is why an
        // undo while the pool is open no longer kicks the monitor to the
        // timeline.
        let sel = pool_sel(app)
            .unwrap_or(0)
            .min(app.project.media.len().saturating_sub(1));
        set_pool_sel(app, sel);
        crate::log::line(&format!("load pool preview #{sel}"));
        // An undo/edit shouldn't rewind the preview; clamp the old clock to
        // the (possibly different) selected asset.
        let pos = pool_pos(app);
        load_pool_preview_at(app, player, pos);
    } else {
        // Proxy renders are seconds of ffmpeg: build the EDL off-thread and
        // let the status row show progress. A newer edit supersedes this one.
        start_preview(app);
    }
}

fn edit_delete(app: &mut App, player: &dyn Mpv) {
    edit_yank(app, player); // dd yanks first (vim: dd fills the register)
    run_action(
        app,
        player,
        &Action::Delete {
            track: app.active(),
            target: Target::At(app.playhead),
            ripple: false,
        },
    );
}

fn edit_yank(app: &mut App, player: &dyn Mpv) {
    run_action(
        app,
        player,
        &Action::Yank {
            track: app.active(),
            target: Target::At(app.playhead),
        },
    );
}

// ------------------------------------------------------------ extend mode

fn extend_state(app: &App) -> Option<ExtendState> {
    match &app.mode {
        Mode::Extend(st) => Some(st.clone()),
        _ => None,
    }
}

fn clamp_extend(st: &ExtendState, t: f64) -> f64 {
    t.clamp(st.lo, st.hi.unwrap_or(f64::INFINITY))
}

/// Clamp `t` into the extend range AND land it on the project's frame
/// grid. The material-end bound is off-grid (it derives from the asset's
/// duration), but the commit ops snap to frames internally — leaving the
/// playhead unsnapped would park it a sub-frame away from the cut the
/// commit actually creates (red indicator, "no clip ends here"). Snapping
/// can step just outside the range, so pull back to the nearest in-range
/// frame.
fn snap_extend_target(fps: Option<&Seconds>, st: &ExtendState, t: f64) -> f64 {
    // No floor at 0: a timeline-start segment can extend left of it
    // (material prepended), so `st.lo` may be negative.
    let clamped = clamp_extend(st, t);
    let Some(fps) = fps else { return clamped };
    let f = secs(fps);
    let mut target = (clamped * f).round() / f;
    if let Some(hi) = st.hi
        && target > hi
    {
        target = (hi * f).floor() / f;
    }
    if target < st.lo {
        target = (st.lo * f).ceil() / f;
    }
    target
}

fn armed_desc(armed: &Armed) -> String {
    match armed {
        Armed::Clip { id, head } => {
            format!("{} of clip #{id}", if *head { "head" } else { "tail" })
        }
        Armed::GapTail { .. } => "gap end".to_string(),
        Armed::GapHead { .. } => "gap start".to_string(),
    }
}

/// Enter extend mode: resolve the boundary, build the material preview,
/// and hand mpv over to it. Stays in Normal on any failure.
fn enter_extend(app: &mut App, player: &dyn Mpv, ripple: bool) {
    let plan = match plan_extend(&app.project, app.playhead, ripple, app.active()) {
        Err(msg) => {
            app.set_message(&msg);
            return;
        }
        Ok(p) => p,
    };
    // `resolve_armed` hops past a blend beside the cut; say so, or the playhead
    // moving to the blend's far end reads as a jump.
    let hopped = (plan.edge_pos - app.playhead).abs() > 1e-6
        && at_blend_edge(&app.project, app.active(), app.playhead);
    // Land on the armed edge: a gap on the right presents its tail, which
    // sits away from the cut `e` was pressed at.
    app.playhead = plan.edge_pos;
    apply_extend_plan(app, player, plan, ripple);
    if hopped {
        app.message
            .push_str(" — the blend is part of the clip — armed its far end");
    }
}

/// Hand the monitor over to an extend plan's material preview and enter
/// the mode. Shared by entry and by swapping sides at a shared cut.
fn apply_extend_plan(app: &mut App, player: &dyn Mpv, plan: ExtendPlan, ripple: bool) {
    let (url, origin) = match &plan.preview {
        ExtendPreviewSpec::Clip {
            path,
            src_start,
            length,
            origin,
        } => {
            let pv = preview::build_clip_preview(path, *src_start, *length, *origin);
            (pv.url, pv.origin)
        }
        ExtendPreviewSpec::Black { origin } => {
            // Bounded ranges (leading gap) cover just the gap; unbounded
            // ones (trailing gap) reach an hour out via repeated chunks.
            let length = match plan.hi {
                Some(hi) => Seconds::new(((hi - origin) * 1e6).round() as i64, 1_000_000),
                None => Seconds::from_integer(preview::EXTEND_PREVIEW_SECS),
            };
            match preview::build_black_preview(&app.project, *origin, length) {
                Err(e) => {
                    app.set_message(&format!("preview failed: {e}"));
                    return;
                }
                Ok(pv) => (pv.url, pv.origin),
            }
        }
    };
    player.load(&url, Some((app.playhead - origin).max(0.0)));
    // The material preview is not the pooled file; its clock maps back
    // through `preview_off` (see `sync`).
    app.pool_on_monitor = false;
    app.pool_handoff = None;
    let flag = if ripple { " · RIPPLE" } else { "" };
    app.set_message(&format!(
        "extend {}{flag} — navigate, e to commit",
        armed_desc(&plan.armed)
    ));
    app.mode = Mode::Extend(ExtendState {
        track: plan.track,
        armed: plan.armed,
        edge_pos: plan.edge_pos,
        ripple,
        lo: plan.lo,
        hi: plan.hi,
        mat: plan.mat,
        preview_off: origin,
    });
}

/// Which clip boundary of a cut to work on: the clip that ENDS there
/// (its tail) or the clip that STARTS there (its head). At a clip|clip
/// cut both exist; against a gap only one side has a clip.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

/// The clip boundary on `side` of the cut at `t`, with its exact edge
/// position. `None` when no clip sits on that side (a gap, or nothing).
fn armed_on_side(project: &Project, t: f64, side: Side, ti: usize) -> Option<(Armed, f64)> {
    let track = project.timeline.track(ti)?;
    let eps = 1e-6;
    let mut acc = 0.0;
    for (i, seg) in track.segments.iter().enumerate() {
        let start = acc;
        acc += secs(&seg.duration());
        let end = acc;
        match seg {
            // A blend's far edge is the neighbour's real edge: the clip
            // before it ends there (`]`), the clip after it starts there
            // (`[`). At its near edge the same bracket arms the same clip
            // and `blend_far_end` hops across afterwards.
            TimelineSegment::Transition(_) => match side {
                Side::Left if (end - t).abs() < eps => {
                    if let Some(TimelineSegment::Clip(c)) =
                        i.checked_sub(1).and_then(|p| track.segments.get(p))
                    {
                        return Some((
                            Armed::Clip {
                                id: c.id,
                                head: false,
                            },
                            t,
                        ));
                    }
                }
                Side::Right if (start - t).abs() < eps => {
                    if let Some(TimelineSegment::Clip(c)) = track.segments.get(i + 1) {
                        return Some((
                            Armed::Clip {
                                id: c.id,
                                head: true,
                            },
                            t,
                        ));
                    }
                }
                _ => {}
            },
            TimelineSegment::Clip(c) => match side {
                Side::Left if (end - t).abs() < eps => {
                    return Some((
                        Armed::Clip {
                            id: c.id,
                            head: false,
                        },
                        end,
                    ));
                }
                Side::Right if (start - t).abs() < eps => {
                    return Some((
                        Armed::Clip {
                            id: c.id,
                            head: true,
                        },
                        start,
                    ));
                }
                _ => {}
            },
            // A gap presents the edge at the cut: its tail where it ends
            // (left side) or its head where it starts (right side).
            TimelineSegment::Gap(_) => match side {
                Side::Left if (end - t).abs() < eps => {
                    return Some((Armed::GapTail { index: i }, end));
                }
                Side::Right if (start - t).abs() < eps => {
                    return Some((Armed::GapHead { index: i }, start));
                }
                _ => {}
            },
        }
    }
    None
}

/// Is `t` a blend's edge — the cut between a clip and a transition?
fn at_blend_edge(project: &Project, ti: usize, t: f64) -> bool {
    let Some(track) = project.timeline.track(ti) else {
        return false;
    };
    let mut acc = 0.0;
    for seg in &track.segments {
        if let TimelineSegment::Transition(_) = seg
            && (acc - t).abs() < 1e-6
        {
            return true;
        }
        acc += secs(&seg.duration());
        if let TimelineSegment::Transition(_) = seg
            && (acc - t).abs() < 1e-6
        {
            return true;
        }
    }
    false
}

/// The far end of the blend beside a clip's armed edge, if one is there. A
/// transition is part of both neighbours, so entering extend mode at a
/// clip|transition cut hops past it to the clip's real edge.
fn blend_far_end(
    project: &Project,
    ti: usize,
    armed: &Armed,
    edge: f64,
) -> Option<(f64, &'static str)> {
    let track = project.timeline.track(ti)?;
    let Armed::Clip { id, head } = armed else {
        return None;
    };
    let idx = track.clip_index(*id)?;
    // The blend sits on the side the armed edge opens toward: a head reaches
    // left, a tail right.
    let ni = if *head { idx.checked_sub(1)? } else { idx + 1 };
    let TimelineSegment::Transition(tr) = track.segments.get(ni)? else {
        return None;
    };
    let (near, far) = (
        secs(&track.start_of(ni)),
        secs(&(track.start_of(ni) + tr.dur)),
    );
    // The edge the clip's edge touches, and the blend's far end past it.
    let (want, far_end) = if *head { (far, near) } else { (near, far) };
    ((edge - want).abs() < 1e-6)
        .then_some((far_end, "the blend is part of the clip — armed its far end"))
}

/// The material at a blend's far edge: the preceding clip's tail ends on
/// `a.src_out` (the blend's end), the following clip's head starts on
/// `b.src_in` (its start). `None` when the edge isn't a blend's far end or
/// that side is black.
fn blend_edge_source(track: &Track, clip_idx: usize, edge: f64, head: bool) -> Option<f64> {
    let ni = if head {
        clip_idx.checked_sub(1)?
    } else {
        clip_idx + 1
    };
    let TimelineSegment::Transition(tr) = track.segments.get(ni)? else {
        return None;
    };
    let near = secs(&track.start_of(ni));
    let far = near + secs(&tr.dur);
    // A tail's edge is the blend's end; a head's is its start.
    let (want, side) = if head { (near, &tr.b) } else { (far, &tr.a) };
    if (edge - want).abs() > 1e-6 {
        return None;
    }
    match side {
        TransitionSource::Clip {
            src_in, src_out, ..
        } => Some(secs(if head { src_in } else { src_out })),
        TransitionSource::Black => None,
    }
}

fn side_hint(side: Side) -> &'static str {
    // Name the *other* bracket: the hint fires on the key that just failed.
    match side {
        Side::Left => "no clip ends here — [ works the clip on the right",
        Side::Right => "no clip starts here — ] works the clip on the left",
    }
}

/// `[` / `]` at a cut: enter extend mode already armed on the clip on
/// that side — `[` opens right, so it works the clip starting (right)
/// there; `]` opens left, so the clip ending (left). `e`/`E` keep the
/// default (right at a shared cut).
fn enter_extend_side(app: &mut App, player: &dyn Mpv, side: Side) {
    let Some((armed, mut edge)) = armed_on_side(&app.project, app.playhead, side, app.active())
    else {
        app.set_message(side_hint(side));
        return;
    };
    // The blend beside the cut is part of the clip: arm its far end.
    let note = match blend_far_end(&app.project, app.active(), &armed, edge) {
        Some((far, note)) => {
            edge = far;
            Some(note)
        }
        None => None,
    };
    match plan_for_armed(&app.project, armed, edge, app.active()) {
        Ok(plan) => {
            // Land on the armed edge (a gap presents its tail, which can
            // be away from the cut we armed it from).
            app.playhead = edge;
            apply_extend_plan(app, player, plan, false);
            if let Some(note) = note {
                app.message.push_str(&format!(" — {note}"));
            }
        }
        Err(m) => app.set_message(&m),
    }
}

/// `[` / `]` inside extend mode: move to the clip on the other side of
/// the same cut, keeping the ripple flag and playhead.
fn set_extend_side(app: &mut App, player: &dyn Mpv, side: Side) {
    let Some(st) = extend_state(app) else { return };
    let Some((armed, mut edge)) = armed_on_side(&app.project, st.edge_pos, side, st.track) else {
        app.set_message(side_hint(side));
        return;
    };
    // Same hop as entering the mode: a blend beside the cut is part of the
    // clip, so its edge is the blend's far end.
    if let Some((far, _)) = blend_far_end(&app.project, st.track, &armed, edge) {
        edge = far;
    }
    if armed == st.armed {
        return;
    }
    match plan_for_armed(&app.project, armed, edge, st.track) {
        Ok(plan) => {
            app.playhead = edge;
            apply_extend_plan(app, player, plan, st.ripple);
        }
        Err(m) => app.set_message(&m),
    }
}

/// The armed edge on the requested end of the same segment (clip or gap),
/// and its position. `None` when the segment is gone.
fn flip_target(project: &Project, armed: Armed, head: bool, ti: usize) -> Option<(Armed, f64)> {
    match armed {
        Armed::Clip { id, .. } => clip_span(project, id, ti)
            .map(|(s, e)| (Armed::Clip { id, head }, if head { s } else { e })),
        Armed::GapHead { index } | Armed::GapTail { index } => {
            gap_span(project, index, ti).map(|(s, e)| {
                if head {
                    (Armed::GapHead { index }, s)
                } else {
                    (Armed::GapTail { index }, e)
                }
            })
        }
    }
}

/// `<` / `>` (and `,` / `.`) inside extend mode: re-anchor the armed edge
/// to the head or tail of the segment being edited (clip or gap), keeping
/// the ripple flag.
fn flip_extend_edge(app: &mut App, player: &dyn Mpv, head: bool) {
    let Some(st) = extend_state(app) else { return };
    let Some((armed, edge)) = flip_target(&app.project, st.armed, head, st.track) else {
        app.set_message("segment is gone");
        return;
    };
    if st.armed == armed {
        return; // already on that end
    }
    match plan_for_armed(&app.project, armed, edge, st.track) {
        Ok(plan) => {
            // Anchor the playhead before handing the monitor over so the
            // material preview opens on the armed source point.
            app.playhead = edge;
            apply_extend_plan(app, player, plan, st.ripple);
        }
        Err(m) => app.set_message(&m),
    }
}

/// Leave extend mode: project timeline back on the monitor, playhead at
/// `pos`. The rebuild rides the preview worker like every other monitor
/// hand-over — a cold proxy is seconds of ffmpeg on the key thread, which
/// froze the whole TUI — so the exit parked below lets `poll_preview` land
/// the EDL. Pause first: an auto-playing EDL streams past the landing and
/// the position check in `sync` would never clear (the same reason
/// `close_pool_at` pauses).
fn exit_extend(app: &mut App, player: &dyn Mpv, pos: f64, msg: &str) {
    app.playhead = pos;
    app.mode = Mode::Normal;
    // The material preview is still (or just now) the monitor's file; its
    // clock must not drive the playhead until the timeline EDL reports its
    // landing. The poll raises `pool_handoff` when it loads.
    app.pool_on_monitor = true;
    app.pool_handoff = None;
    app.pending_pause = Some(true);
    player.set_pause(true);
    start_preview(app);
    app.set_message(msg);
}

/// Map a commit kind to its edit op for the playhead under `app` — the
/// extend commit's routing, as a test-facing shim over [`commit_batch`].
#[cfg(test)]
fn run_commit_action(
    app: &App,
    armed: &Armed,
    kind: CommitKind,
    ripple: bool,
) -> Result<Batch, EditError> {
    commit_batch(
        &app.project,
        app.playhead,
        armed,
        kind,
        ripple,
        app.active(),
    )
}

/// The timeline length if the current extend position were committed.
/// Runs the real op on a scratch copy so every route (trim, absorb,
/// ripple, gap) projects honestly; falls back to the current length when
/// the commit would refuse or leave an invalid timeline.
fn projected_total(app: &App, st: &ExtendState) -> f64 {
    let fallback = app.total();
    let Ok(kind) = plan_commit(&app.project, &st.armed, st.edge_pos, app.playhead, st.track) else {
        return fallback;
    };
    if kind == CommitKind::Noop {
        return fallback;
    }
    let Ok(batch) = commit_batch(
        &app.project,
        app.playhead,
        &st.armed,
        kind,
        st.ripple,
        st.track,
    ) else {
        return fallback;
    };
    let mut probe = app.project.clone();
    for op in &batch.ops {
        if op.apply(&mut probe).is_err() || probe.validate().is_err() {
            return fallback;
        }
    }
    secs(&probe.timeline.end())
}

/// Map an armed boundary to the catalog's addressing — extend mode
/// resolved it interactively, and the action resolves it back the same
/// way (gap edges by segment index, clip edges by id).
fn target_of_armed(armed: &Armed) -> (Target, Edge) {
    match armed {
        Armed::Clip { id, head } => (Target::Clip(*id), if *head { Edge::In } else { Edge::Out }),
        Armed::GapHead { index } => (Target::Segment(*index), Edge::In),
        Armed::GapTail { index } => (Target::Segment(*index), Edge::Out),
    }
}

/// Commit: run the extend as one undo step and leave the mode. Failures
/// keep the mode (and its preview) so the position isn't lost.
fn commit_extend(app: &mut App, player: &dyn Mpv) {
    let Some(st) = extend_state(app) else { return };
    let kind = match plan_commit(&app.project, &st.armed, st.edge_pos, app.playhead, st.track) {
        Err(msg) => {
            app.set_message(&msg);
            return;
        }
        Ok(k) => k,
    };
    if kind == CommitKind::Noop {
        exit_extend(app, player, st.edge_pos, "edge already there");
        return;
    }
    let (target, edge) = target_of_armed(&st.armed);
    if run_action(
        app,
        player,
        &Action::Extend {
            track: st.track,
            target,
            edge,
            to: app.playhead,
            ripple: st.ripple,
        },
    ) {
        app.mode = Mode::Normal;
        // The monitor still shows the material preview while the post-edit
        // rebuild renders, and translating its clock back needs extend
        // mode's `preview_off`. Raise the hand-off so `sync` ignores mpv
        // until the timeline EDL lands, and pause like `close_pool_at`:
        // an auto-playing EDL reports past the landing and the check would
        // never clear. Resume is one press of `space`.
        app.pool_on_monitor = true;
        app.pool_handoff = None;
        app.pending_pause = Some(true);
        player.set_pause(true);
    }
}

/// Cancel: snap the playhead back to the armed edge, project timeline
/// back on the monitor.
fn cancel_extend(app: &mut App, player: &dyn Mpv) {
    let Some(st) = extend_state(app) else { return };
    exit_extend(app, player, st.edge_pos, "extend cancelled");
}

/// Mode-aware absolute seek: clamped to the committable range, translated
/// through the preview offset for mpv.
fn seek_to_mode(app: &mut App, player: &dyn Mpv, st: &ExtendState, t: f64) {
    if !app.guard_loaded() {
        return;
    }
    let target = snap_extend_target(app.project.fps.as_ref(), st, t);
    app.playhead = target;
    // mpv can't seek before its file start; the gap head can push the
    // mapped position negative, so clamp (the preview is black anyway).
    player.seek_absolute((target - st.preview_off + 0.004).max(0.0));
}

fn seek_rel_mode(app: &mut App, player: &dyn Mpv, st: &ExtendState, delta: f64) {
    let target = app.playhead + delta;
    seek_to_mode(app, player, st, target);
}

/// Frame index a frame-step from `playhead` lands on, clamped to the armed
/// range. `lo` may be negative (head material prepended past time 0) — do
/// not re-floor it here.
fn extend_step_index(
    lo: f64,
    hi: Option<f64>,
    fps: Seconds,
    playhead: f64,
    dir: i64,
    mult: u32,
) -> i64 {
    let f = secs(&fps);
    let lo_i = (lo * f).round() as i64;
    let hi_i = hi.map(|h| (h * f).round() as i64).unwrap_or(i64::MAX);
    ((playhead * f).round() as i64 + dir * mult as i64).clamp(lo_i, hi_i)
}

/// Mode-aware frame step: the frame index is clamped to the range, and
/// mpv only steps when the model actually moved (no divergence).
fn step_mode(app: &mut App, player: &dyn Mpv, st: &ExtendState, dir: i64, mult: u32) {
    if !app.guard_loaded() {
        return;
    }
    let Some(fps) = app.project.fps.as_ref() else {
        return;
    };
    let idx = extend_step_index(st.lo, st.hi, *fps, app.playhead, dir, mult);
    let next = idx as f64 / secs(fps);
    if next != app.playhead {
        app.playhead = next;
        // Seek mpv to the model frame on the project grid — see the f/F note.
        app.seek_to(player, next);
    }
}

fn extend_inert(app: &mut App) {
    app.set_message("exit extend mode first (Esc)");
}

fn extend_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    let code = key.code;

    // Counts still build for nav keys; bare `0` seeks (clamped below).
    if let KeyCode::Char(c @ '0'..='9') = code {
        if c == '0' && app.count.is_none() {
            if let Some(st) = extend_state(app) {
                seek_to_mode(app, player, &st, 0.0);
            }
            return;
        }
        let d = c.to_digit(10).unwrap_or(0);
        app.count = Some(app.count.unwrap_or(0) * 10 + d);
        return;
    }
    let mult = app.count.take().unwrap_or(1);
    let eps = half_frame(&app.project);

    // `gg` still works (lands on the range start via clamp).
    if let (Pending::G, KeyCode::Char('g')) = (&app.pending, code) {
        app.pending = Pending::None;
        if let Some(st) = extend_state(app) {
            seek_to_mode(app, player, &st, 0.0);
        }
        return;
    }

    let Some(st) = extend_state(app) else { return };
    match code {
        KeyCode::Char(' ') => {
            if app.guard_loaded() {
                player.toggle_pause();
            }
        }
        KeyCode::Char('h') => seek_rel_mode(app, player, &st, -(mult as f64)),
        KeyCode::Char('l') => seek_rel_mode(app, player, &st, mult as f64),
        KeyCode::Char('H') => seek_rel_mode(app, player, &st, -5.0 * mult as f64),
        KeyCode::Char('L') => seek_rel_mode(app, player, &st, 5.0 * mult as f64),
        KeyCode::Char('f') => step_mode(app, player, &st, 1, mult),
        KeyCode::Char('F') => step_mode(app, player, &st, -1, mult),
        KeyCode::Char('j') => {
            match next_cut(&cut_points(&app.project, app.active()), app.playhead, eps) {
                Some(t) => seek_to_mode(app, player, &st, t),
                None => seek_to_mode(app, player, &st, app.total()),
            }
        }
        KeyCode::Char('k') => {
            match prev_cut(&cut_points(&app.project, app.active()), app.playhead, eps) {
                Some(t) => seek_to_mode(app, player, &st, t),
                None => seek_to_mode(app, player, &st, 0.0),
            }
        }
        KeyCode::Char('w') => {
            match next_clip_start(&app.project, app.playhead, eps, app.active()) {
                Some(t) => seek_to_mode(app, player, &st, t),
                None => seek_to_mode(app, player, &st, app.total()),
            }
        }
        KeyCode::Char('b') => {
            match prev_clip_start(&app.project, app.playhead, eps, app.active()) {
                Some(t) => seek_to_mode(app, player, &st, t),
                None => seek_to_mode(app, player, &st, 0.0),
            }
        }
        KeyCode::Char('n') => match next_gap_start(&app.project, app.playhead, eps, app.active()) {
            Some(t) => seek_to_mode(app, player, &st, t),
            None => seek_to_mode(app, player, &st, app.total()),
        },
        KeyCode::Char('N') => match prev_gap_start(&app.project, app.playhead, eps, app.active()) {
            Some(t) => seek_to_mode(app, player, &st, t),
            None => seek_to_mode(app, player, &st, 0.0),
        },
        KeyCode::Char('g') => app.pending = Pending::G,
        KeyCode::Char('G') | KeyCode::Char('$') => seek_to_mode(app, player, &st, app.total()),
        KeyCode::Char('e') => commit_extend(app, player),
        KeyCode::Char('[') => set_extend_side(app, player, Side::Right),
        KeyCode::Char(']') => set_extend_side(app, player, Side::Left),
        KeyCode::Char('<') | KeyCode::Char(',') => flip_extend_edge(app, player, true),
        KeyCode::Char('>') | KeyCode::Char('.') => flip_extend_edge(app, player, false),
        KeyCode::Char('E') => {
            let ripple = match &mut app.mode {
                Mode::Extend(inner) => {
                    inner.ripple = !inner.ripple;
                    inner.ripple
                }
                _ => return,
            };
            app.set_message(if ripple {
                "extend: ripple ON"
            } else {
                "extend: preserve"
            });
        }
        KeyCode::Esc => cancel_extend(app, player),
        KeyCode::Char('q') => cancel_extend(app, player),
        KeyCode::Char(':')
        | KeyCode::Char('r')
        | KeyCode::Char('u')
        | KeyCode::Char('i')
        | KeyCode::Char('I')
        | KeyCode::Char('o')
        | KeyCode::Char('O')
        | KeyCode::Char('s')
        | KeyCode::Char('p')
        | KeyCode::Char('P')
        | KeyCode::Char('d')
        | KeyCode::Char('y') => extend_inert(app),
        _ => {}
    }

    if !matches!(code, KeyCode::Char('g') | KeyCode::Char('0'..='9')) {
        app.pending = Pending::None;
    }
}

// ------------------------------------------------------------ rendering

fn render(f: &mut Frame, app: &App) {
    // Size the timeline to show every track, then give what's left to the
    // info panel and the bottom panels. The bottom panels are dropped before
    // the timeline is allowed to hide a track. While the pool is open the
    // panel is that asset's one-clip strip, so one row + marker is enough and
    // the freed rows go to the pool list below.
    let track_count = if pool_strip_active(app) {
        1
    } else {
        app.project.timeline.tracks.len()
    };
    let (timeline_h, keys, msg_h) = panel_heights(track_count, f.area().height);
    // The add form carries its own key hints and needs the rows for its
    // fields; drop the bottom cheat-sheet while it is open.
    let keys = if is_fx(app) && fx_state(app).is_some_and(|st| st.form.is_some()) {
        0
    } else {
        keys
    };
    let [status, timeline, info, keys_area, msg_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(timeline_h),
        Constraint::Min(0),
        Constraint::Length(keys),
        Constraint::Length(msg_h),
    ])
    .areas(f.area());

    render_status(f, status, app);
    render_timeline(f, timeline, app);
    if is_pool(app) {
        render_pool(f, info, app);
    } else if is_fx(app) {
        render_fx(f, info, app);
    } else if matches!(app.mode, Mode::Transition(_)) {
        render_transition_form(f, info, app);
    } else {
        render_info(f, info, app);
    }
    if keys > 0 {
        render_hints(f, keys_area, app);
    }
    if msg_h > 0 {
        render_message(f, msg_area, app);
    }
}

/// Rows for the timeline panel and the two bottom panels: the timeline gets
/// one row per track plus the marker row and two borders, and the keys/message
/// panels are dropped before a track has to be hidden. Returns
/// `(timeline_h, keys_h, msg_h)`. Pure so the sizing is testable.
fn panel_heights(track_count: usize, height: u16) -> (u16, u16, u16) {
    let track_rows = track_count.max(1) as u16;
    // Two borders + one row per track + the marker row.
    let want_tl = track_rows.saturating_add(3);
    // The status row is always present; keep a few rows for the info panel.
    let min_info = 3u16;
    let max_tl = height.saturating_sub(1 + min_info).max(4);
    let timeline_h = want_tl.min(max_tl).max(4);
    // Bottom panels only when they fit without squeezing the tracks.
    let leftover = height.saturating_sub(1 + timeline_h + min_info);
    let msg_h = if leftover >= 3 { 3 } else { 0 };
    let keys_h = if leftover >= 3 + 8 { 8 } else { 0 };
    (timeline_h, keys_h, msg_h)
}

/// The keys cheat-sheet (mode-specific; see `hints_for`).
fn render_hints(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" keys ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines: Vec<Line> = hints_for(app)
        .into_iter()
        .map(|h| Line::from(Span::styled(h, Style::default().fg(Color::DarkGray))))
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The latest message/feedback line, in its own panel.
fn render_message(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" message ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let (msg, style) = if !app.message.is_empty() {
        (app.message.clone(), Style::default().fg(Color::Yellow))
    } else {
        ("—".to_string(), Style::default().fg(Color::DarkGray))
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(msg, style))).wrap(Wrap { trim: false }),
        inner,
    );
}

/// The mode-specific key cheat-sheet (single source of truth for hints).
fn hints_for(app: &App) -> Vec<&'static str> {
    if let Mode::Command(_) = app.mode {
        return vec![
            "type a command · enter run · esc cancel · flags: --at T --track N --ripple",
            "edits   :split :trim-in|out :delete :yank :paste :gap :extend :speed",
            "history :undo :redo · tracks :track add video|audio · :track rm N",
            "media   :import PATH… · :add FILE · :media ls · :media rm N",
            "more    :show :seek :purge :detect :edl-import :otio-import :export",
        ];
    }
    if matches!(app.mode, Mode::Extend(_)) {
        return vec![
            "extend mode — move the armed edge, then commit",
            "h/l ±1s · H/L ±5s · f/F frame · j/k cuts · w/b clips · n/N gaps · gg/G/0",
            "e commit · E ripple on/off · [ ] side · < > (, .) end · Esc cancel",
        ];
    }
    if is_pool(app) {
        return vec![
            "media pool — the monitor previews the selected file",
            "j/k assets · { } ends · h/l ±1s · H/L ±5s · f/F frame · gg/0 start · G/$ end",
            "space play · enter insert · i import · d remove · u undo · m/esc close",
            "tracks     t/T target · <N>t jump · insert goes to the active track",
        ];
    }
    if matches!(app.mode, Mode::Transition(_)) {
        return vec![
            "transition length — frames, then enter (blank keeps it)",
            "digits edit · ctrl-u clears · esc cancels",
        ];
    }
    if is_fx(app) {
        if let Some(form) = fx_state(app).and_then(|st| st.form.as_ref()) {
            return vec![
                form_hint(form.kind),
                "letters jump · tab/↑↓ field · space toggles lock · enter applies",
                "ctrl-u clears · esc cancels",
            ];
        }
        return vec![
            "effects — rows apply in order at export; the monitor re-renders",
            "j/k select · 1-9 edit · d drop · D clear all · u undo · Ctrl-r redo",
            "c/s/e/o add/edit crop/scale/eq/offset · x transition · q close",
        ];
    }
    vec![
        "transport  space play · h/l ±1s · H/L ±5s · f/F frame · gg/G/0/$ ends",
        "motion     j/k cuts · w/b clips · n/N gaps · < > (, .) clip start/end",
        "edit       i/o trim · I/O ripple · s split · _ gap · dd/D del · yy/p/P",
        "           r speed · X transition · e/E extend · [ ] side · m pool",
        "effects    x panel · c/s/e/o add/edit · d/D drop · u undo · Ctrl-r redo",
        "tracks     t/T cycle · <N>t jump · :track add · : cmd · Esc · q quit",
    ]
}

/// The pool takes over the info pane: asset list on the left, probe
/// metadata for the selection on the right.
fn render_pool(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{List, ListState, Paragraph, Wrap};
    let [list_area, meta_area] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(0)]).areas(area);

    // Left: the asset list. The title names the track `enter` inserts into.
    let target = track_label(&app.project, app.active());
    let list_block = Block::bordered().title(format!(" media pool → {target} "));
    let list_inner = list_block.inner(list_area);
    f.render_widget(list_block, list_area);
    let items: Vec<ratatui::widgets::ListItem> = app
        .project
        .media
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let used = app.project.asset_usage(a.id);
            let mark = if used.is_empty() { "○" } else { "●" };
            let style = if i == pool_sel(app).unwrap_or(0) {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            ratatui::widgets::ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} #{:<3} ", a.id), style),
                Span::styled(asset_line(a), style),
            ]))
        })
        .collect();
    // The list scrolls to keep the selected row visible: `ListState` owns the
    // viewport offset, while the selection itself lives in the mode
    // (`pool_sel`), so the widget state is rebuilt each frame, not persisted.
    let mut list_state = ListState::default();
    list_state.select(pool_sel(app));
    f.render_stateful_widget(List::new(items), list_inner, &mut list_state);

    // Right: probe metadata for the selected asset.
    let meta_block = Block::bordered().title(" metadata ");
    let meta_inner = meta_block.inner(meta_area);
    f.render_widget(meta_block, meta_area);
    let lines = match app.project.media.get(pool_sel(app).unwrap_or(0)) {
        Some(a) => asset_metadata(a),
        None => vec!["(no asset selected)".to_string()],
    };
    let para = Paragraph::new(
        lines
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::Gray))))
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false });
    f.render_widget(para, meta_inner);
}

/// Plain-text summary for the pool row (path basename + duration).
fn asset_line(a: &nle_core::Asset) -> String {
    let name = a
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| a.path.display().to_string());
    let dur = a
        .duration()
        .map(|d| format!("{}s", secs(&d)))
        .unwrap_or_default();
    format!("{name} {dur}")
}

/// Human-readable probe metadata for the metadata pane (all from the
/// stored `RawProbe` — the file is never re-probed).
fn asset_metadata(a: &nle_core::Asset) -> Vec<String> {
    let p = &a.probe;
    let mut out = vec![
        format!("path   {}", a.path.display()),
        format!("id     #{}", a.id),
    ];
    let fmt = &p.format;
    out.push(format!("format {}", fmt.format_name));
    if let Some(d) = a.duration() {
        out.push(format!("dur    {}s", secs(&d)));
    }
    if let Some(sz) = fmt.size.as_deref() {
        out.push(format!("size   {sz} bytes"));
    }
    if let Some(b) = fmt.bit_rate.as_deref() {
        out.push(format!("bitrate {b}"));
    }
    if let Some((w, h)) = a.video_size() {
        out.push(format!("size   {w}×{h}"));
    }
    if let Some(f) = a.fps() {
        out.push(format!("fps    {}/{}", f.numer(), f.denom()));
    }
    out.push(String::new());
    for s in &p.streams {
        let kind = s.codec_type.as_deref().unwrap_or("?");
        let codec = s.codec_name.as_deref().unwrap_or("?");
        let mut line = format!("stream {} {kind}/{codec}", s.index);
        if let Some(w) = s.width
            && let Some(h) = s.height
        {
            line += &format!(" {w}×{h}");
        }
        if let Some(r) = &s.r_frame_rate {
            line += &format!(" {r}fps");
        }
        if let Some(pf) = &s.pix_fmt {
            line += &format!(" {pf}");
        }
        if let Some(sr) = s.sample_rate.as_deref() {
            line += &format!(" {sr}Hz");
        }
        if let Some(ch) = s.channels {
            line += &format!(" {ch}ch");
        }
        out.push(line);
    }
    out
}

/// The status-bar mark: a crocodile. `=` is the closed mouth; a delete opens
/// it to `>` for a moment (`CHOMP_MS`), then it closes again.
const CROC: &str = r#"=";::;>--"#;
const CHOMP: &str = r#">";::;>--"#;
const CHOMP_MS: u64 = 250;
/// How long a preview build runs before the status row announces it. A warm
/// cache (or a no-proxy timeline) finishes in milliseconds; without this every
/// edit flashed `building preview…` for a frame.
const PREVIEW_NOTICE_DELAY: Duration = Duration::from_millis(200);

fn logo(app: &App) -> &'static str {
    match app.chomp_until {
        Some(until) if Instant::now() < until => CHOMP,
        _ => CROC,
    }
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let name = app
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| app.path.display().to_string());

    let icon = if app.dead {
        "monitor closed"
    } else if !app.loaded {
        "… loading"
    } else if app.paused {
        // Two ASCII cells each. `⏸` has an emoji presentation in some
        // terminals (a wide white square) while `▶` stays narrow, so the
        // text after the icon drifted by one cell. These can't.
        "||"
    } else {
        "|>"
    };
    let mode = match &app.mode {
        Mode::Normal => "NORMAL".to_string(),
        Mode::Command(buf) => format!(":{buf}▏"),
        Mode::Extend(st) => {
            if st.ripple {
                "EXTEND·RIPPLE".to_string()
            } else {
                "EXTEND".to_string()
            }
        }
        Mode::Pool(_) => "POOL".to_string(),
        Mode::Fx(_) => "EFFECTS".to_string(),
        Mode::Transition(_) => "TRANSITION".to_string(),
    };
    let count = app.count.map(|c| format!("{c}")).unwrap_or_default();

    let style = Style::default().bg(Color::Blue).fg(Color::White);
    // In extend mode show where the timeline WILL end, so the pending
    // commit's effect on length is visible before pressing e.
    let (total, projected) = match &app.mode {
        Mode::Extend(st) => {
            let p = projected_total(app, st);
            (p, (p - app.total()).abs() > 1e-9)
        }
        _ => (app.total(), false),
    };
    let total_style = if projected {
        style.add_modifier(Modifier::BOLD).fg(Color::Yellow)
    } else {
        style
    };

    // In the pool the monitor and the timeline row are the selected asset,
    // so the clock reads the asset position/duration and the mode tag says
    // POOL. The project cursor is hidden until the pool closes.
    if is_pool(app) {
        let sel = pool_sel(app).unwrap_or(0);
        let (pos, dur) = match app.project.media.get(sel) {
            Some(a) => (pool_pos(app), a.duration().map(|d| secs(&d)).unwrap_or(0.0)),
            None => (0.0, 0.0),
        };
        let fs = app.project.media.get(sel).and_then(|a| a.fps());
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", logo(app)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {name} · "), style),
            Span::styled(
                fmt_timecode(pos, fs.as_ref()),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" / {} · ", fmt_timecode(dur, fs.as_ref())), style),
            Span::styled(format!("{icon} · POOL{count}"), style),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let mut spans = vec![
        Span::styled(
            format!(" {} ", logo(app)),
            style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {name} · "), style),
        Span::styled(
            fmt_timecode(app.playhead, app.project.fps.as_ref()),
            style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {} · ", fmt_timecode(total, app.project.fps.as_ref())),
            total_style,
        ),
        Span::styled(format!("{icon} · {mode}{count}"), style),
    ];
    if let Some(p) = app.preview_progress
        && app
            .preview
            .as_ref()
            .is_some_and(|j| j.started.elapsed() >= PREVIEW_NOTICE_DELAY)
    {
        spans.push(Span::styled(
            format!(" · {}", preview_indicator(p, app.preview_dirty)),
            style.add_modifier(Modifier::BOLD).fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A 10-frame spinner for the status row. Time-based, so it animates with the
/// loop's ~50 ms tick without extra app state.
fn spinner() -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let i = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() / 100) as usize % FRAMES.len())
        .unwrap_or(0);
    FRAMES[i]
}

/// The status-row preview indicator: `⠋ proxies 2/5 · 43% · queued`. The
/// fraction is (finished + this render's progress) / total.
fn preview_indicator(p: PreviewProgress, dirty: bool) -> String {
    let done = p.done.min(p.total);
    let pct = if p.total > 0 {
        (((p.done as f64 + p.frac) / p.total as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u32
    } else {
        0
    };
    let mut s = if p.total > 0 {
        format!("{} proxies {done}/{} · {pct}%", spinner(), p.total)
    } else {
        format!("{} building preview…", spinner())
    };
    if dirty {
        s.push_str(" · queued");
    }
    s
}

/// Make track `idx` active (clamped) and return its label. Leaves the monitor
/// alone — the pool keeps its asset preview; normal mode reloads on top.
fn select_active_track(app: &mut App, idx: usize) -> Option<String> {
    let n = app.project.timeline.tracks.len();
    if n == 0 {
        return None;
    }
    app.active_track = idx.min(n - 1);
    Some(track_label(&app.project, app.active_track))
}

/// Make track `idx` active and announce it. The monitor plays the composite
/// of all video tracks, so switching tracks does not reload it.
fn set_active_track(app: &mut App, idx: usize) {
    if let Some(label) = select_active_track(app, idx) {
        app.set_message(&format!("track {label}"));
    }
}

/// Switch the active track from the pool: retarget where `enter` inserts and
/// announce it, but leave the monitor on the asset preview.
fn pool_set_track(app: &mut App, idx: usize) {
    if let Some(label) = select_active_track(app, idx) {
        app.set_message(&format!("insert track {label}"));
    }
}

fn render_timeline(f: &mut Frame, area: Rect, app: &App) {
    // While the pool is open the monitor is off the project, so the timeline
    // row is the selected asset's own strip — playing, paused or ended. The
    // project (and its insert point) returns on close.
    if pool_strip_active(app)
        && let Mode::Pool(st) = &app.mode
    {
        render_pool_strip(f, area, app, st);
        return;
    }
    let block = Block::bordered().title(" timeline ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 6 || inner.height < 2 {
        return;
    }

    let tracks = &app.project.timeline.tracks;
    let n = tracks.len();
    // A label gutter only when there is more than one track to tell apart.
    let label_w: u16 = if n > 1 { 4 } else { 0 };
    let bar_w = inner.width.saturating_sub(label_w);
    if bar_w == 0 {
        return;
    }

    // In extend mode the row rescales past the timeline end so the
    // projected range stays visible while navigating outward.
    let in_extend = matches!(app.mode, Mode::Extend(_));
    let total = if in_extend {
        app.total().max(app.playhead).max(0.001)
    } else {
        app.total().max(0.001)
    };

    // One row per visible track; the marker row takes the row below them.
    let capacity = inner.height.saturating_sub(1) as usize;
    let (start, count) = track_window(n, app.active(), capacity.max(1));

    for (row, ti) in (start..start + count).enumerate() {
        let y = inner.y + row as u16;
        if label_w > 0 {
            let active = ti == app.active();
            let style = if active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let label = format!("{:>3} ", track_label(&app.project, ti));
            let gutter = Rect {
                x: inner.x,
                y,
                width: label_w,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(label, style))),
                gutter,
            );
        }
        let bar = Rect {
            x: inner.x + label_w,
            y,
            width: bar_w,
            height: 1,
        };
        draw_track_row(
            f,
            bar,
            app,
            &tracks[ti],
            total,
            in_extend && ti == app.active(),
        );
    }

    let marker_y = inner.y + count as u16;
    let bar = Rect {
        x: inner.x + label_w,
        y: marker_y,
        width: bar_w,
        height: 1,
    };
    draw_marker_row(f, bar, app, total, in_extend);
}

/// While the pool is open the timeline panel and the monitor both show the
/// selected asset — playing, paused or ended. The project returns (at its
/// insert cursor) only when the pool is closed.
fn pool_strip_active(app: &App) -> bool {
    is_pool(app)
}

/// The pool previews a raw file, so the monitor is not on the project
/// timeline. Replace the timeline row with the selected asset's own strip (a
/// single clip `0..duration`) and put the marker at the preview clock.
/// `panel_heights` already sized this panel to one track + marker.
fn render_pool_strip(f: &mut Frame, area: Rect, app: &App, st: &PoolState) {
    let Some(asset) = app.project.media.get(st.sel) else {
        f.render_widget(Block::bordered().title(" preview "), area);
        return;
    };
    let block = Block::bordered().title(format!(" preview — {} ", asset_line(asset)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 6 || inner.height < 2 {
        return;
    }
    let Some(dur) = asset.duration() else {
        // No known duration means no scale to draw against: name only.
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                asset_line(asset),
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    };
    let total = secs(&dur).max(0.001);
    // Reuse the project row renderer so the name truncates and the edges
    // quantize through the same `marker_col` as the playhead marker.
    let track = Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![TimelineSegment::Clip(Clip::new(
            0,
            asset.id,
            Seconds::from_integer(0),
            dur,
        ))],
    };
    let row = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    draw_track_row(f, row, app, &track, total, false);
    let marker = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: 1,
    };
    draw_pool_marker_row(f, marker, total, st.pos, app.eof);
}

/// A bare marker row for the pool strip: red `▏`, `▕` at the file's end.
fn draw_pool_marker_row(f: &mut Frame, bar: Rect, total: f64, pos: f64, eof: bool) {
    if bar.width == 0 {
        return;
    }
    let ph = marker_col(pos, total, bar.width) as usize;
    let mut line = Line::default();
    if ph > 0 {
        line.push_span(Span::raw(" ".repeat(ph)));
    }
    let ch = if eof { '▕' } else { '▏' };
    line.push_span(Span::styled(
        ch.to_string(),
        Style::default().fg(Color::Red),
    ));
    f.render_widget(Paragraph::new(line), bar);
}

/// Which tracks to show: all of them when they fit, else a window of
/// `capacity` centred on the active track.
fn track_window(n: usize, active: usize, capacity: usize) -> (usize, usize) {
    if n <= capacity {
        return (0, n);
    }
    let start = active.saturating_sub(capacity / 2).min(n - capacity);
    (start, capacity)
}

/// One track's segments as (duration, label, style) parts, plus a trailing
/// gap for any span short of the timeline end — a track shorter than the
/// timeline reads as gap rather than blank.
/// One row's parts: `(duration, label, style, permeable_left)`. The flag is
/// true when the boundary on this part's left is a blend — a transition is
/// part of both neighbours, so that divider is drawn permeable rather than
/// hard.
fn track_parts(project: &Project, track: &Track, total: f64) -> Vec<(f64, String, Style, bool)> {
    let gap_style = Style::default().bg(Color::DarkGray).fg(Color::Black);
    let is_trans = |s: &TimelineSegment| matches!(s, TimelineSegment::Transition(_));
    let mut parts: Vec<(f64, String, Style, bool)> = track
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let soft = is_trans(s) || i > 0 && track.segments.get(i - 1).is_some_and(is_trans);
            match s {
                TimelineSegment::Clip(c) => {
                    let name = project
                        .asset(c.asset)
                        .and_then(|a| a.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| format!("#{}", c.id));
                    // `*` marks a clip with effects — a suffix would be cut
                    // off on a narrow cell, a prefix survives.
                    let name = if c.effects.is_empty() {
                        name
                    } else {
                        format!("*{name}")
                    };
                    (
                        secs(&c.duration()),
                        name,
                        Style::default().bg(Color::Cyan).fg(Color::Black),
                        soft,
                    )
                }
                // A transition gets its own band; the real dissolve is a
                // proxy-render thing (M5), so it's labelled, not blended.
                TimelineSegment::Transition(t) => (
                    secs(&t.dur),
                    "trans".to_string(),
                    Style::default().bg(Color::Magenta).fg(Color::Black),
                    soft,
                ),
                TimelineSegment::Gap(g) => (secs(&g.duration), "gap".to_string(), gap_style, soft),
            }
        })
        .collect();
    let tail = total - secs(&track.end());
    if tail > 1e-9 {
        let soft = track.segments.last().is_some_and(is_trans);
        parts.push((tail, "gap".to_string(), gap_style, soft));
    }
    parts
}

/// Draw one track's segment row. `notch` overlays the extend-mode playhead
/// marker (only the armed track gets it). Boundaries come from the SAME
/// quantizer as the marker row (`marker_col` of the cumulative time), so
/// drawn clip edges and the playhead/cut indicator can never disagree.
fn draw_track_row(f: &mut Frame, bar: Rect, app: &App, track: &Track, total: f64, notch: bool) {
    let parts = track_parts(&app.project, track, total);
    let mut cum: f64 = 0.0;
    let mut cells: Vec<(u16, String, Style)> = Vec::new();
    for (i, (dur, label, style, soft)) in parts.iter().enumerate() {
        let start_x = if i == 0 {
            0
        } else {
            marker_col(cum, total, bar.width)
        };
        cum += dur;
        let end_x = if i == parts.len() - 1 {
            bar.width
        } else {
            marker_col(cum, total, bar.width)
        };
        let width = end_x.saturating_sub(start_x);
        if width == 0 {
            continue; // rounds away — no cell, no divider
        }
        let text = if i == 0 || start_x == 0 {
            truncate_label(label, width)
        } else {
            // A blend is part of both neighbours: its boundary is a bare
            // colour change, not a bar. (A dotted box glyph would read as
            // permeable but sits mid-cell, so it never lines up with the
            // left-aligned `▏` dividers or the playhead marker.)
            let bar = if *soft { ' ' } else { '▏' };
            format!("{bar}{}", truncate_label(label, width.saturating_sub(1)))
        };
        cells.push((start_x, text, *style));
    }

    let mut spans: Vec<Span> = Vec::new();
    if notch {
        // Split the containing part's text around the playhead marker.
        let ph = marker_col(app.playhead, total, bar.width);
        let edge = match &app.mode {
            Mode::Extend(st) => st.edge_pos,
            _ => app.playhead,
        };
        let at_edge = (app.playhead - edge).abs() < 1e-9;
        let notch_style = Style::default().fg(if at_edge { Color::White } else { Color::Yellow });
        for (x0, text, style) in &cells {
            let chars: Vec<char> = text.chars().collect();
            let off = (ph as usize).saturating_sub(*x0 as usize);
            if ph >= *x0 && off < chars.len() {
                let before: String = chars[..off].iter().collect();
                let after: String = chars[off + 1..].iter().collect();
                if !before.is_empty() {
                    spans.push(Span::styled(before, *style));
                }
                spans.push(Span::styled("▏".to_string(), notch_style));
                if !after.is_empty() {
                    spans.push(Span::styled(after, *style));
                }
            } else {
                spans.push(Span::styled(text.clone(), *style));
            }
        }
    } else {
        for (_, text, style) in &cells {
            spans.push(Span::styled(text.clone(), *style));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), bar);
}

/// Draw the playhead marker row: red ▏ normally, WHITE exactly on a cut
/// frame, ▕ at EOF. In extend mode it also carries the workable span
/// (yellow) and the material extent (dim), with a yellow playhead.
fn draw_marker_row(f: &mut Frame, bar: Rect, app: &App, total: f64, in_extend: bool) {
    let ph = marker_col(app.playhead, total, bar.width);
    let at_end = app.eof;
    let mut marker_cells: Vec<(char, Style)> = vec![(' ', Style::default()); bar.width as usize];
    if let Mode::Extend(st) = &app.mode {
        let white_here = (app.playhead - st.edge_pos).abs() < 1e-9;
        let color = if white_here {
            Color::White
        } else {
            Color::Yellow
        };
        if let Some((m0, m1)) = st.mat {
            paint_run(
                &mut marker_cells,
                total,
                bar.width,
                m0,
                m1,
                '·',
                Style::default().fg(Color::DarkGray),
            );
        }
        let hi_edge = st.hi.unwrap_or(f64::INFINITY);
        paint_run(
            &mut marker_cells,
            total,
            bar.width,
            st.lo,
            hi_edge,
            '─',
            Style::default().fg(Color::Yellow),
        );
        marker_cells[ph as usize] = (if at_end { '▕' } else { '▏' }, Style::default().fg(color));
    } else {
        let _ = in_extend;
        let color = if on_boundary(&app.project, app.playhead, app.active()) {
            Color::White
        } else {
            Color::Red
        };
        marker_cells[ph as usize] = (if at_end { '▕' } else { '▏' }, Style::default().fg(color));
    }
    let mut marker: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in marker_cells {
        if run_style.as_ref() != Some(&style) {
            if let Some(s) = run_style {
                marker.push(Span::styled(std::mem::take(&mut run), s));
            }
            run_style = Some(style);
        }
        run.push(ch);
    }
    if let Some(s) = run_style {
        marker.push(Span::styled(run, s));
    }
    f.render_widget(Paragraph::new(Line::from(marker)), bar);
}
/// Paint `glyph` with `style` over the cells spanning `[a, b)` in
/// timeline seconds (`b` may exceed the row — it clamps).
fn paint_run(
    cells: &mut [(char, Style)],
    total: f64,
    width: u16,
    a: f64,
    b: f64,
    glyph: char,
    style: Style,
) {
    if width == 0 || b <= a {
        return;
    }
    let col_a = marker_col(a.max(0.0), total, width) as usize;
    let mut col_b = marker_col(b, total, width) as usize;
    if b >= total - 1e-9 {
        col_b = cells.len();
    }
    for cell in cells.iter_mut().take(col_b).skip(col_a) {
        *cell = (glyph, style);
    }
}

/// The effects panel takes over the info pane: one row per effect, in the
/// order they apply at export. Adding asks for the values in a labelled form
/// (`render_fx_form`); applying runs the same `AddEffect` as the CLI.
fn render_fx(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{List, ListItem, ListState, Paragraph};
    let st = fx_state(app);
    let clip = fx_clip(app);
    let form = st.and_then(|s| s.form.as_ref());
    let title = match (clip, form) {
        (Some(c), Some(f)) => {
            let verb = if f.editing.is_some() { "edit" } else { "new" };
            format!(" {verb} {} → clip #{} ", f.kind, c.id)
        }
        (Some(c), None) => format!(" effects → clip #{} ", c.id),
        (None, _) => " effects ".to_string(),
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(clip) = clip else {
        f.render_widget(
            Paragraph::new("the clip is gone — esc to close")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    if let Some(form) = form {
        render_fx_form(f, inner, form);
        return;
    }
    if clip.effects.is_empty() {
        f.render_widget(
            Paragraph::new("(no effects)   c crop · s scale · e eq · o offset")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let sel = st.map(|s| s.sel);
    let items: Vec<ListItem> = clip
        .effects
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let style = if sel == Some(i) {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>2}  ", i + 1), style),
                Span::styled(format!("{:<6}", e.kind()), style),
                Span::styled(e.details(), style),
            ]))
        })
        .collect();
    let mut list_state = ListState::default();
    list_state.select(sel);
    f.render_stateful_widget(List::new(items), inner, &mut list_state);
}

/// The add form: one labelled field per parameter, the focused value
/// highlighted, and the navigation line underneath.
/// What the form's fields expect: units, ranges, and neutral values.
fn form_hint(kind: &str) -> &'static str {
    match kind {
        "crop" => {
            "edge inset % (0 = frame edge, 100 = opposite edge); \
             the cut area shows below, else black"
        }
        "scale" => {
            "percent of the picture at this point; 100 keeps the size \
             (below 100 reveals the track below)"
        }
        "offset" => {
            "percent of the distance to the edge; 0 keeps the position, \
             ±100 is fully off-screen"
        }
        "eq" => "neutral values: brightness 0 · contrast 1 · saturation 1",
        _ => "",
    }
}

fn render_fx_form(f: &mut Frame, area: Rect, form: &FxForm) {
    let mut lines: Vec<Line> = form
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let value = &field.value;
            let shown = if field.kind == FieldKind::Toggle {
                if form.lock {
                    "x".to_string()
                } else {
                    " ".to_string()
                }
            } else if i == form.focus {
                format!("{value}▏")
            } else if value.is_empty() {
                "…".to_string()
            } else {
                value.clone()
            };
            let style = if i == form.focus {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let label_style = Style::default().fg(Color::DarkGray);
            let mut spans = Vec::new();
            // The first letter is the jump key; underline it so the mnemonic
            // is visible without a separate key legend.
            if field.key.is_some() {
                let mut chars = field.label.chars();
                if let Some(first) = chars.next() {
                    spans.push(Span::styled(
                        first.to_string(),
                        label_style.add_modifier(Modifier::UNDERLINED),
                    ));
                    spans.push(Span::styled(chars.as_str().to_string(), label_style));
                }
            } else {
                spans.push(Span::styled(field.label.to_string(), label_style));
            }
            let label_len = field.label.chars().count();
            spans.push(Span::raw(" ".repeat(12usize.saturating_sub(label_len))));
            spans.push(Span::styled(format!("[{shown}]"), style));
            if let Some(previous) = &field.previous {
                spans.push(Span::styled(
                    format!("  was {previous}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(form_hint(form.kind)).style(Style::default().fg(Color::DarkGray)));
    lines.push(
        Line::from("letters jump · tab/↑↓ field · space toggles lock · enter applies · ctrl-u clears · esc cancels")
            .style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn render_transition_form(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Transition(form) = &app.mode else {
        return;
    };
    let block = Block::bordered().title(format!(" transition #{} ", form.id));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let shown = if form.frames.is_empty() {
        "▏".to_string()
    } else {
        format!("{}▏", form.frames)
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{:<11}", "frames"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("[{shown}]"),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::styled(
                format!("  was {}", form.current),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        Line::from("enter applies · esc cancels").style(Style::default().fg(Color::DarkGray)),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_info(f: &mut Frame, area: Rect, app: &App) {
    let t = app.playhead;

    let first_line = {
        let label = track_label(&app.project, app.active());
        let body = match &app.mode {
            Mode::Extend(st) => extend_status(app, st),
            _ => clip_line(app, t),
        };
        format!("[{label}] {body}")
    };
    fn clip_line(app: &App, t: f64) -> String {
        clip_under(&app.project, t, app.active())
            .map(|c| {
                let asset = app
                    .project
                    .asset(c.asset)
                    .map(|a| a.path.display().to_string())
                    .unwrap_or_else(|| "?".into());
                let fps = app.project.fps.as_ref();
                let (seg_start, seg_end) = {
                    let track = app.track().unwrap();
                    let si = track.index_at(grid_time(&app.project, t)).unwrap();
                    let s = track.start_of(si);
                    (s, s + track.segments[si].duration())
                };
                let clip_fps = app
                    .project
                    .asset(c.asset)
                    .and_then(|a| a.fps())
                    .map(|f| format!(" · {:.2}fps", secs(&f)))
                    .unwrap_or_default();
                format!(
                    "clip #{}  {}   {} – {} · len {}{clip_fps}   (src {} – {})",
                    c.id,
                    asset,
                    fmt_timecode(secs(&seg_start), fps),
                    fmt_timecode(secs(&seg_end), fps),
                    fmt_timecode(secs(&c.duration()), fps),
                    fmt_timecode(secs(&c.asset_in), fps),
                    fmt_timecode(secs(&c.asset_out), fps)
                )
            })
            .unwrap_or_else(|| match gap_under(&app.project, t, app.active()) {
                Some((gs, ge)) => {
                    let fps = app.project.fps.as_ref();
                    format!(
                        "— gap —  {} – {} · len {}",
                        fmt_timecode(secs(&gs), fps),
                        fmt_timecode(secs(&ge), fps),
                        fmt_timecode(secs(&(ge - gs)), fps)
                    )
                }
                None => "— gap —".to_string(),
            })
    }

    /// First info line in extend mode: armed edge, committable range,
    /// available material, and the mode keys.
    fn extend_status(app: &App, st: &ExtendState) -> String {
        let fps = app.project.fps.as_ref();
        let hi = st
            .hi
            .map(|h| fmt_timecode(h, fps))
            .unwrap_or_else(|| "…".to_string());
        let range = format!("{}–{}", fmt_timecode(st.lo, fps), hi);
        let flag = if st.ripple { " · RIPPLE" } else { "" };
        match &st.armed {
            Armed::Clip { id, head } => {
                let mat = match material_remaining(app, *id, *head) {
                    Some(s) => format!(" · material {s:.1}s left"),
                    None => String::new(),
                };
                format!(
                    "EXTEND {} of clip #{} · edge {} · range {range}{mat}{flag}",
                    if *head { "head" } else { "tail" },
                    id,
                    fmt_timecode(st.edge_pos, fps),
                )
            }
            Armed::GapTail { .. } => format!("EXTEND gap end · range {range} · free growth{flag}"),
            Armed::GapHead { .. } => {
                format!("EXTEND gap start · range {range} · overwrites left{flag}")
            }
        }
    }

    /// Source seconds still unused on the extend side of a clip edge:
    /// before the in-point (head) or past the out-point (tail). `None`
    /// when the asset duration is unknown.
    fn material_remaining(app: &App, id: u32, head: bool) -> Option<f64> {
        let track = app.track()?;
        let clip = track
            .segments
            .iter()
            .filter_map(|seg| match seg {
                TimelineSegment::Clip(c) if c.id == id => Some(c),
                _ => None,
            })
            .next()?;
        let asset = app.project.asset(clip.asset)?;
        let dur = asset.duration()?;
        Some(if head {
            secs(&clip.asset_in)
        } else {
            (secs(&dur) - secs(&clip.asset_out)).max(0.0)
        })
    }

    let stats = format!(
        "{} clip(s) · {} · {} fps · undo depth {}",
        app.track()
            .map(|t| {
                t.segments
                    .iter()
                    .filter(|s| matches!(s, TimelineSegment::Clip(_)))
                    .count()
            })
            .unwrap_or(0),
        fmt_timecode(app.total(), app.project.fps.as_ref()),
        app.project
            .fps
            .as_ref()
            .map(|f| format!("{}/{}", f.numer(), f.denom()))
            .unwrap_or_else(|| "?".into()),
        app.history.depth()
    );

    let mut lines = vec![
        Line::from(first_line),
        Line::from(""),
        Line::from(Span::styled(stats, Style::default().fg(Color::Gray))),
    ];
    if let Some(err) = &app.mpv_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("mpv: {err}"),
            Style::default().fg(Color::Red),
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" info ")),
        area,
    );
}

/// True when the (frame-snapped) playhead sits exactly on an edit
/// boundary: an interior cut, the timeline's start, or its end (a clip
/// end / trailing-gap start counts; the timeline's own edges count too).
/// Both sides come from the same rational→f64 conversions, so exact
/// comparison is meaningful; the epsilon only guards against drift.
fn on_boundary(project: &Project, t: f64, ti: usize) -> bool {
    // The cut list already includes the timeline's start (0) and end —
    // every boundary instant in one place.
    cut_points(project, ti)
        .iter()
        .any(|&c| (c - t).abs() < 1e-9)
}

/// Column of a time position on the timeline row. Used for BOTH the
/// playhead marker and the boundary ticks — one scale, or they drift
/// apart (the playhead used (w-1) once and sat a column off the end).
fn marker_col(t: f64, total: f64, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    (((t / total.max(1e-9)) * width as f64).round() as u16).min(width - 1)
}

fn truncate_label(label: &str, width: u16) -> String {
    let w = width as usize;
    if w == 0 {
        return String::new();
    }
    let chars: Vec<char> = label.chars().collect();
    if chars.len() >= w {
        chars[..w].iter().collect()
    } else {
        format!("{:^width$}", label, width = w)
    }
}

#[cfg(test)]
mod tests;
