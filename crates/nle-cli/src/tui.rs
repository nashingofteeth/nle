//! The terminal dashboard: timeline visualization, vim keys, live playhead.
//!
//! Input model: a small vim. Counts work (`3l` = seek 3s), `gg`/`G` jump to
//! ends, `j`/`k` walk cut points, `w`/`b` walk clip starts, `:` opens a
//! command line. Playback keys are sent to mpv; state flows back through
//! the reader thread and is rendered on the next tick (~20fps redraw).

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use nle_core::edit;
use nle_core::timeline::{Segment as TimelineSegment, Track, TrackKind};
use nle_core::{
    Armed, Batch, Clip, CommitKind, EditError, History, Project, Seconds, clip_span, commit_batch,
    frame_of, frame_to_secs, gap_span, plan_commit, resolve_armed, track_label,
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

/// Which two-key sequence is waiting for its second keystroke (`gg`, `dd`, `yy`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Pending {
    None,
    G,
    D,
    Y,
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
    /// Yanked clip, for `p`/`P`.
    pub clipboard: Option<TimelineSegment>,
    /// Undo/redo stack of edit batches (session-only).
    pub history: History,
    pub reload_requested: bool,
    pub quit: bool,
    pub message: String,
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
    /// Remembered pool row so reopening the panel returns there. A sticky UI
    /// preference, not panel state (whether the panel is open is `Mode::Pool`).
    last_pool_sel: usize,
    /// Which track edits and navigation act on. Clamped via `active()`.
    pub active_track: usize,
}

impl App {
    pub fn new(project: Project, path: PathBuf) -> Self {
        Self {
            project,
            path,
            mode: Mode::Normal,
            playhead: 0.0,
            count: None,
            pending: Pending::None,
            clipboard: None,
            history: History::default(),
            reload_requested: false,
            quit: false,
            message: String::new(),
            paused: true,
            loaded: false,
            dead: false,
            eof: false,
            mpv_error: None,
            exit_message: None,
            last_pool_sel: 0,
            active_track: 0,
        }
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

    pub fn sync(&mut self, st: &PlaybackState) {
        // Authority split: while PLAYING, mpv's streamed position drives
        // the playhead (only it knows the live position). While PAUSED,
        // the MODEL owns it — the playhead only moves through our own
        // seeks and frame-steps, so mpv's drift-flavored reports (EDL
        // boundaries sit off the project grid) can never fight the
        // canonical positions that cuts, the white indicator, and edit
        // keys depend on.
        //
        // In extend mode mpv plays the material preview, whose position 0
        // is the range start: translate back to timeline coordinates.
        if !st.paused
            && st.file_loaded
            && let Some(t) = st.time_pos
        {
            if is_pool(self) {
                // The monitor is a raw pooled file: position is asset
                // seconds, no EDL translation.
                set_pool_pos(self, t);
            } else {
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
                    _ => snap_to_cut(snapped, &self.project, self.active()),
                };
            }
        }
        self.paused = st.paused;
        self.loaded = st.file_loaded;
        self.dead = st.dead;
        self.eof = st.eof;
        // Pool at EOF: pin the clock to the last frame so it doesn't read
        // a couple frames short of the file's probe duration.
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
            self.playhead = (self.playhead + delta).clamp(0.0, self.total());
        }
    }

    fn seek_to(&mut self, player: &dyn Mpv, t: f64) {
        if self.guard_loaded() {
            let target = t.max(0.0);
            // Model position is authoritative when paused: set it now.
            self.playhead = target;
            // Nudge a few ms INTO the target instant: EDL segment
            // boundaries sit a hair after the cut (source frame rounding),
            // so an exact seek would display the PREVIOUS segment's last
            // frame (gap/black) — the clip's color only appears after one
            // manual frame-step. 4ms clears the drift seen on this
            // machine's timelines.
            player.seek_absolute(target + 0.004);
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

pub fn run<P: Mpv>(project: Project, path: PathBuf, player: P) -> anyhow::Result<()> {
    let mut terminal = ratatui::init(); // raw mode + alternate screen + panic hook
    let history = History::load(&History::sidecar_path(&path)).unwrap_or_default();
    let mut app = App::new(project, path);
    app.history = history;
    if let Some(notice) = preview::speed_notice(&app.project) {
        app.set_message(&notice);
    }
    // Media title (MPRIS "Now playing") follows the clip under the playhead.
    let mut last_media_title: Option<String> = None;

    loop {
        let st = player.state();
        app.sync(&st);

        if app.reload_requested {
            app.reload_requested = false;
            match reload_project(&mut app, &player) {
                Ok(()) => {
                    let msg = preview::speed_notice(&app.project)
                        .unwrap_or_else(|| "timeline reloaded".to_string());
                    app.set_message(&msg);
                }
                Err(e) => app.set_message(&format!("reload failed: {e}")),
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

    ratatui::restore();
    if let Some(msg) = app.exit_message {
        println!("{msg}");
    }
    Ok(())
}

/// Reload the project (and its undo history) from disk and rebuild the
/// preview at the current playhead. Lets you edit `nle.json` in another
/// window (e.g. vim) and watch it here.
fn reload_project(app: &mut App, player: &dyn Mpv) -> anyhow::Result<()> {
    let project = Project::load(&app.path)?;
    app.project = project;
    app.active_track = app
        .active_track
        .min(app.project.timeline.tracks.len().saturating_sub(1));
    app.history = History::load(&History::sidecar_path(&app.path)).unwrap_or_default();
    let edl = preview::build_composite_edl(&app.project)?;
    player.load(&edl, Some(app.playhead));
    Ok(())
}

// ------------------------------------------------------------ keys

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

pub fn handle_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
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
        KeyCode::Char('i') => app.mode = Mode::Command("import ".to_string()),
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

/// Load the selected asset into the monitor as a raw file.
fn load_pool_preview(app: &mut App, player: &dyn Mpv) {
    let Some(sel) = pool_sel(app) else { return };
    let Some(path) = app.project.media.get(sel).map(|a| a.path.clone()) else {
        return;
    };
    set_pool_pos(app, 0.0);
    player.load(&path.display().to_string(), Some(0.0));
}

/// Open the pool: remember the timeline playhead, preview the last row.
fn open_pool(app: &mut App, player: &dyn Mpv) {
    let sel = app
        .last_pool_sel
        .min(app.project.media.len().saturating_sub(1));
    app.mode = Mode::Pool(PoolState {
        sel,
        pos: 0.0,
        resume: app.playhead,
    });
    load_pool_preview(app, player);
    app.set_message("media");
}

/// Close the pool: reload the project timeline at the remembered playhead.
fn close_pool(app: &mut App, player: &dyn Mpv) {
    let resume = pool_resume(app);
    if let Some(sel) = pool_sel(app) {
        app.last_pool_sel = sel; // capture on exit so reopening returns here
    }
    app.mode = Mode::Normal;
    if let Ok(edl) = preview::build_composite_edl(&app.project) {
        player.load(&edl, Some(resume.max(0.0)));
    }
    app.playhead = resume;
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
    match app.history.undo(&mut app.project) {
        Ok(outcome) => {
            app.playhead = frame_to_secs(app.project.fps.as_ref(), outcome.playhead);
            app.set_message(&format!("undo: {}", outcome.desc));
            persist_and_refresh(app, player);
        }
        Err(e) => app.set_message(&e.to_string()),
    }
}

/// Redo the last undone edit.
fn do_redo(app: &mut App, player: &dyn Mpv) {
    match app.history.redo(&mut app.project) {
        Ok(outcome) => {
            app.playhead = frame_to_secs(app.project.fps.as_ref(), outcome.playhead);
            app.set_message(&format!("redo: {}", outcome.desc));
            persist_and_refresh(app, player);
        }
        Err(e) => app.set_message(&e.to_string()),
    }
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
    });
    let _ = run_edit(
        app,
        player,
        edit::paste_ripple_on(&app.project, app.active(), &snapshot, app.playhead),
    );
}

/// Remove the selected asset from the pool, undoably (the guard refuses if
/// it's in use), then refresh the pool preview.
fn remove_from_pool(app: &mut App, player: &dyn Mpv) {
    let Some(asset) = app.project.media.get(pool_sel(app).unwrap_or(0)) else {
        app.set_message("no asset selected");
        return;
    };
    let id = asset.id;
    let playhead = frame_of(app.project.fps.as_ref(), app.playhead);
    match app.history.remove_asset(&mut app.project, id, playhead) {
        Ok(desc) => {
            app.set_message(&desc);
            persist_and_refresh(app, player);
        }
        Err(e) => app.set_message(&e.to_string()),
    }
}

/// `:track ...` — add / mute / remove tracks. Structural changes save
/// immediately. Removal resets the undo sidecar: track indices shift, so
/// any recorded `Change::Track` would point at the wrong track.
fn track_command(app: &mut App, cmd: &str) -> String {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let idx_arg = |parts: &[&str]| -> Option<usize> {
        parts
            .get(2)
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n.saturating_sub(1))
    };
    match parts.get(1).copied().unwrap_or("") {
        "add" => {
            let kind = match parts.get(2).copied() {
                Some("video") => TrackKind::Video,
                Some("audio") => TrackKind::Audio,
                _ => return "usage: :track add video|audio".to_string(),
            };
            let idx = app.project.timeline.add_track(kind);
            app.active_track = idx;
            let label = track_label(&app.project, idx);
            match app.project.save(&app.path) {
                Ok(()) => format!("added track {label}"),
                Err(e) => format!("added track {label} — save failed: {e}"),
            }
        }
        "mute" => {
            let idx = idx_arg(&parts).unwrap_or_else(|| app.active());
            let Some(track) = app.project.timeline.track_mut(idx) else {
                return format!("no track #{}", idx + 1);
            };
            track.muted = !track.muted;
            let muted = track.muted;
            let label = track_label(&app.project, idx);
            let _ = app.project.save(&app.path);
            format!("{label} {}", if muted { "muted" } else { "unmuted" })
        }
        "rm" | "remove" => {
            let idx = idx_arg(&parts).unwrap_or_else(|| app.active());
            let n = app.project.timeline.tracks.len();
            if idx >= n {
                return format!("no track #{}", idx + 1);
            }
            let is_video = app.project.timeline.tracks[idx].kind == TrackKind::Video;
            if is_video && app.project.timeline.video_tracks().count() <= 1 {
                return "can't remove the only video track".to_string();
            }
            // The track's clips go with it. Track indices shift under the
            // undo stack, so this re-baselines history (not undoable).
            let removed = app.project.timeline.tracks.remove(idx);
            let clips = removed
                .segments
                .iter()
                .filter(|s| matches!(s, TimelineSegment::Clip(_)))
                .count();
            app.history = History::default();
            let _ = std::fs::remove_file(History::sidecar_path(&app.path));
            app.active_track = app
                .active_track
                .min(app.project.timeline.tracks.len().saturating_sub(1));
            match app.project.save(&app.path) {
                Ok(()) if clips > 0 => format!("removed track ({clips} clip(s) deleted)"),
                Ok(()) => "removed track".to_string(),
                Err(e) => format!("removed track — save failed: {e}"),
            }
        }
        _ => "usage: :track add video|audio · :track mute [N] · :track rm N".to_string(),
    }
}

/// The `:` commands, shown by `:help` and in the Command-mode cheat-sheet.
const COMMAND_HELP: &str = "commands — :track · :speed · :purge · :import · :w · :q";

/// `:import PATH…` — probe files and add them to the media pool. An
/// out-of-band pool write, so it re-baselines the undo sidecar (a stale
/// `Change::Assets` entry would otherwise refuse to apply).
fn import_command(app: &mut App, cmd: &str) -> String {
    let rest = cmd.strip_prefix("import").unwrap_or("").trim();
    if rest.is_empty() {
        return "usage: :import PATH [PATH…]".to_string();
    }
    let mut new = 0usize;
    let mut existing = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for arg in rest.split_whitespace() {
        match PathBuf::from(arg).canonicalize() {
            Ok(path) => match nle_ffmpeg::probe(&path) {
                Ok(report) => {
                    if app.project.add_asset(path, report).1 {
                        new += 1;
                    } else {
                        existing += 1;
                    }
                }
                Err(e) => failed.push(format!("{arg}: {e}")),
            },
            Err(_) => failed.push(format!("{arg}: not found")),
        }
    }
    if new > 0 {
        app.history = History::default();
        let _ = std::fs::remove_file(History::sidecar_path(&app.path));
        let _ = app.project.save(&app.path);
    }
    let mut msg = format!("imported {new} new, {existing} already pooled");
    if !failed.is_empty() {
        msg.push_str(&format!(" — failed: {}", failed.join("; ")));
    }
    msg
}

fn command_key(app: &mut App, player: &dyn Mpv, buf: String, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let cmd = buf.trim().to_string();
            match cmd.as_str() {
                "q" | "q!" | "quit" | "wq" => {
                    app.quit = true;
                    return; // keep the command text visible while exiting
                }
                "w" | "write" => app.set_message("edits save automatically — nothing to save"),
                "" => {}
                other if other == "track" || other.starts_with("track ") => {
                    let msg = track_command(app, other);
                    app.set_message(&msg);
                }
                other if other == "speed" || other.starts_with("speed ") => {
                    let arg = other.strip_prefix("speed").unwrap_or("").trim();
                    speed_command(app, player, arg);
                }
                "purge" | "clean" => {
                    let msg = purge_command(app);
                    app.set_message(&msg);
                }
                other if other == "import" || other.starts_with("import ") => {
                    let msg = import_command(app, other);
                    app.set_message(&msg);
                }
                "help" | "?" => app.set_message(COMMAND_HELP),
                other => app.set_message(&format!("unknown command: {other}  (try :q)")),
            }
            app.mode = Mode::Normal;
        }
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Backspace => {
            let mut b = buf;
            b.pop();
            app.mode = Mode::Command(b)
        }
        KeyCode::Char(c) => app.mode = Mode::Command(buf + &c.to_string()),
        _ => app.mode = Mode::Command(buf),
    }
}

fn normal_key(app: &mut App, player: &dyn Mpv, key: KeyEvent) {
    let code = key.code;

    // Ctrl-C quits even in raw mode (no SIGINT is generated).
    if code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        return;
    }

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
            edit_yank(app);
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
            let _ = run_edit(
                app,
                player,
                trim_key(&app.project, app.playhead, false, false, app.active()),
            );
        }
        KeyCode::Char('I') => {
            let _ = run_edit(
                app,
                player,
                trim_key(&app.project, app.playhead, false, true, app.active()),
            );
        }
        KeyCode::Char('o') => {
            let _ = run_edit(
                app,
                player,
                trim_key(&app.project, app.playhead, true, false, app.active()),
            );
        }
        KeyCode::Char('O') => {
            let _ = run_edit(
                app,
                player,
                trim_key(&app.project, app.playhead, true, true, app.active()),
            );
        }
        KeyCode::Char('s') => {
            let _ = run_edit(
                app,
                player,
                edit::split_on(&app.project, app.active(), app.playhead),
            );
        }
        KeyCode::Char('_') => {
            let _ = run_edit(
                app,
                player,
                edit::insert_gap_on(&app.project, app.active(), app.playhead, mult as i64),
            );
        }
        KeyCode::Char('D') => {
            edit_yank(app); // D yanks too
            run_edit(
                app,
                player,
                edit::delete_ripple_on(&app.project, app.active(), app.playhead),
            );
        }
        KeyCode::Char('p') => {
            let Some(clipboard) = app.clipboard.clone() else {
                app.set_message("clipboard empty — yy first");
                return;
            };
            run_edit(
                app,
                player,
                edit::paste_on(&app.project, app.active(), &clipboard, app.playhead),
            );
        }
        KeyCode::Char('P') => {
            let Some(clipboard) = app.clipboard.clone() else {
                app.set_message("clipboard empty — yy first");
                return;
            };
            run_edit(
                app,
                player,
                edit::paste_ripple_on(&app.project, app.active(), &clipboard, app.playhead),
            );
        }
        KeyCode::Char('u') => do_undo(app, player),
        KeyCode::Char('m') => open_pool(app, player),
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
        KeyCode::Char('r') => app.reload_requested = true,
        KeyCode::Char('q') => app.quit = true,
        // `R` opens the speed prompt for the clip under the playhead; the
        // command line is pre-filled so it reads as `:speed `.
        KeyCode::Char('R') => app.mode = Mode::Command("speed ".to_string()),
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
            // Preview: the whole asset file, so every reachable position
            // shows real frames. mpv 0 maps to the timeline position of
            // source zero (edge minus the edge's source point). Without a
            // known duration, map just the clip's current span.
            let src_at_edge = if head { ain } else { aout };
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

/// A position anchor for a gap delete: a gap is not media (nothing to
/// re-find by id) — it can only be removed, which ripples — so the
/// playhead lands where it started, the cut between the neighbours.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GapAnchor {
    start: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Anchor {
    Media(MediaAnchor),
    Gap(GapAnchor),
}

/// Anchor the playhead's position before an edit: the clip + source frame
/// over a clip, or the gap's start over a gap.
fn edit_anchor(project: &Project, playhead: f64, ti: usize) -> Option<Anchor> {
    let track = project.timeline.track(ti)?;
    let t = grid_time(project, playhead);
    let idx = track.index_at(t)?;
    match &track.segments[idx] {
        TimelineSegment::Clip(c) => Some(Anchor::Media(MediaAnchor {
            id: c.id,
            // Timeline → source scales by the clip's speed (a 2× clip's
            // timeline second is two source seconds).
            source: c.source_at(t - track.start_of(idx)),
            start: secs(&track.start_of(idx)),
            at: secs(&t),
        })),
        TimelineSegment::Gap(_) => Some(Anchor::Gap(GapAnchor {
            start: secs(&track.start_of(idx)),
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
        // A gap is only ever deleted (rippling), so land on the seam it
        // left between its neighbours.
        Anchor::Gap(g) => g.start,
    };
    let total = app.total();
    app.playhead = pos.clamp(0.0, total);
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

/// `i`/`I`/`o`/`O` routing — the shared [`edit::trim_at_on`], renamed for
/// the key handlers' call sites.
fn trim_key(
    project: &Project,
    ph: f64,
    out: bool,
    ripple: bool,
    ti: usize,
) -> Result<Batch, EditError> {
    edit::trim_at_on(project, ti, ph, out, ripple)
}

/// Apply an edit batch, then save + rebuild the preview so the monitor
/// reflects the new timeline at the current playhead.
fn run_edit(app: &mut App, player: &dyn Mpv, result: Result<Batch, EditError>) -> bool {
    let batch = match result {
        Ok(b) => b,
        Err(e) => {
            app.set_message(&e.to_string());
            return false;
        }
    };
    let desc = batch.desc.clone();
    // Keep the playhead on the frame it was showing, not the same
    // timecode (a trim can move material under it). An extend commit is
    // the exception: land on the edge we extended to (the playhead is
    // already the target), not one frame short of a fresh tail.
    let in_extend = matches!(app.mode, Mode::Extend(_));
    let anchor = (!in_extend)
        .then(|| edit_anchor(&app.project, app.playhead, app.active()))
        .flatten();
    // Undo attribution: a normal edit is made from the playhead; an extend
    // commit is made from where extend mode was entered (the armed edge),
    // so undoing it returns there rather than to the commit target.
    let from = match &app.mode {
        Mode::Extend(st) => st.edge_pos,
        _ => app.playhead,
    };
    let at_frame = frame_of(app.project.fps.as_ref(), from);
    match app.history.apply(&mut app.project, batch, at_frame) {
        Ok(()) => {
            reanchor_playhead(app, anchor);
            app.set_message(&desc);
            persist_and_refresh(app, player);
            true
        }
        Err(e) => {
            app.set_message(&e.to_string());
            false
        }
    }
}

/// `:speed <rate>` — retime the clip under the playhead. The track ripples
/// to fit (nle's timeline is a sequence, so a new duration moves followers).
fn speed_command(app: &mut App, player: &dyn Mpv, arg: &str) {
    if arg.is_empty() {
        app.set_message("usage: :speed 0.5  (rate for the clip under the playhead)");
        return;
    }
    let speed = match crate::parse_speed_arg(arg) {
        Ok(s) => s,
        Err(e) => {
            app.set_message(&e.to_string());
            return;
        }
    };
    let ti = app.active();
    let Some(id) = clip_under(&app.project, app.playhead, ti).map(|c| c.id) else {
        app.set_message("no clip under the playhead");
        return;
    };
    let result = edit::set_speed_on(&app.project, ti, id, speed);
    run_edit(app, player, result);
}

/// `:purge` — drop sub-frame stumps and snap every track back onto the frame
/// grid. Repairs a project that accumulated sub-frame drift; the topology
/// changes, so it re-baselines the undo stack and reloads the monitor.
fn purge_command(app: &mut App) -> String {
    let Some(fps) = app.project.fps else {
        return "no frame rate set — add a clip first".to_string();
    };
    // Clamp inside the media first, then snap the grid / drop stumps.
    let notes = crate::clamp_project_to_media(&mut app.project);
    let mut dropped = 0usize;
    for track in &mut app.project.timeline.tracks {
        let (cleaned, n) = edit::purge_stumps(track, fps);
        track.segments = cleaned;
        dropped += n;
    }
    if let Err(e) = app.project.validate() {
        return format!("purge failed: {e}");
    }
    app.history = History::default();
    let _ = std::fs::remove_file(History::sidecar_path(&app.path));
    let _ = app.project.save(&app.path);
    app.reload_requested = true;
    format!(
        "purged {dropped} sub-frame segment(s), adjusted {} clip(s)",
        notes.len()
    )
}

/// Atomic save + view refresh. Called after every successful edit (and
/// undo/redo) — no manual save state to get wrong. The undo history sidecar
/// is written alongside the project, then the monitor is reloaded for
/// whichever view is showing: the pool's raw file when the pool is open, or
/// the timeline EDL (with the playhead baked into the load) otherwise.
fn persist_and_refresh(app: &mut App, player: &dyn Mpv) {
    if let Err(e) = app.project.save(&app.path) {
        app.set_message(&format!("save failed: {e}"));
        return;
    }
    if let Err(e) = app.history.save(&History::sidecar_path(&app.path)) {
        app.set_message(&format!("history save failed: {e}"));
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
        load_pool_preview(app, player);
    } else {
        match preview::build_composite_edl(&app.project) {
            Ok(edl) => {
                crate::log::line(&format!("load timeline EDL at {:.3}", app.playhead));
                player.load(&edl, Some(app.playhead));
            }
            Err(e) => app.set_message(&format!("preview rebuild failed: {e}")),
        }
    }
}

fn edit_delete(app: &mut App, player: &dyn Mpv) {
    edit_yank(app); // dd yanks first (vim: dd fills the register)
    run_edit(
        app,
        player,
        edit::delete_under_on(&app.project, app.active(), app.playhead),
    );
}

fn edit_yank(app: &mut App) {
    let Some(track) = app.track() else {
        app.set_message("nothing to yank — empty timeline");
        return;
    };
    let Some(si) = track.index_at(grid_time(&app.project, app.playhead)) else {
        app.set_message("nothing to yank — past the end");
        return;
    };
    let seg = track.segments[si].clone();
    let msg = match &seg {
        TimelineSegment::Clip(c) => format!("yanked clip #{}", c.id),
        TimelineSegment::Gap(g) => {
            let frames = app
                .project
                .fps
                .as_ref()
                .map(|f| (secs(&g.duration) * secs(f)).round() as i64);
            match frames {
                Some(n) => format!("yanked gap (f{n})"),
                None => "yanked gap".to_string(),
            }
        }
    };
    app.clipboard = Some(seg);
    app.set_message(&msg);
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
    // Land on the armed edge: a gap on the right presents its tail, which
    // sits away from the cut `e` was pressed at.
    app.playhead = plan.edge_pos;
    apply_extend_plan(app, player, plan, ripple);
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

fn side_hint(side: Side) -> &'static str {
    match side {
        Side::Left => "no clip ends here — ] works the clip on the left",
        Side::Right => "no clip starts here — [ works the clip on the right",
    }
}

/// `[` / `]` at a cut: enter extend mode already armed on the clip on
/// that side — `[` opens right, so it works the clip starting (right)
/// there; `]` opens left, so the clip ending (left). `e`/`E` keep the
/// default (right at a shared cut).
fn enter_extend_side(app: &mut App, player: &dyn Mpv, side: Side) {
    let Some((armed, edge)) = armed_on_side(&app.project, app.playhead, side, app.active()) else {
        app.set_message(side_hint(side));
        return;
    };
    match plan_for_armed(&app.project, armed, edge, app.active()) {
        Ok(plan) => {
            // Land on the armed edge (a gap presents its tail, which can
            // be away from the cut we armed it from).
            app.playhead = edge;
            apply_extend_plan(app, player, plan, false);
        }
        Err(m) => app.set_message(&m),
    }
}

/// `[` / `]` inside extend mode: move to the clip on the other side of
/// the same cut, keeping the ripple flag and playhead.
fn set_extend_side(app: &mut App, player: &dyn Mpv, side: Side) {
    let Some(st) = extend_state(app) else { return };
    let Some((armed, edge)) = armed_on_side(&app.project, st.edge_pos, side, st.track) else {
        app.set_message(side_hint(side));
        return;
    };
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
/// `pos`.
fn exit_extend(app: &mut App, player: &dyn Mpv, pos: f64, msg: &str) {
    match preview::build_composite_edl(&app.project) {
        Ok(edl) => player.load(&edl, Some(pos.max(0.0))),
        Err(e) => app.set_message(&format!("preview rebuild failed: {e}")),
    }
    app.playhead = pos;
    app.mode = Mode::Normal;
    app.set_message(msg);
}

/// Map a commit kind to its edit op. The playhead does the routing: it
/// sits wherever navigation left it, and each op reads positionally.
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

/// Commit: route, apply as one undo step, and leave the mode. Failures
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
    if run_edit(
        app,
        player,
        run_commit_action(app, &st.armed, kind, st.ripple),
    ) {
        app.mode = Mode::Normal;
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

    if code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        return;
    }

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
    // the timeline is allowed to hide a track.
    let (timeline_h, keys, msg_h) =
        panel_heights(app.project.timeline.tracks.len(), f.area().height);
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
            "type a command · enter run · esc cancel",
            ":track add video|audio · mute [N] · rm N",
            ":speed N — retime the clip under the playhead",
            ":purge — snap to frames, drop stumps, clamp to media",
            ":import PATH… — probe into the media pool",
            ":help · :q quit",
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
            "j/k assets · { } list ends · h/l ±1s · H/L ±5s · f/F frame · gg/0 start · G/$ end",
            "space play · enter insert · i import · d remove · u undo · m/esc close",
            "tracks     t/T target · <N>t jump · insert goes to the active track",
        ];
    }
    vec![
        "transport  space play · h/l ±1s · H/L ±5s · f/F frame · gg/G/0/$ ends",
        "motion     j/k cuts · w/b clips · n/N gaps · < > (, .) clip start/end",
        "edit       i/o trim · I/O ripple-trim · s split · _ gap · dd/D delete · yy/p/P",
        "           R speed (rate prompt) · e/E extend · [ ] side · m pool",
        "extend     e/E enter · [ ] side · m pool · u undo · r reload · : cmd · q quit",
        "tracks     t/T cycle · <N>t jump to N · :track add video|audio",
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
        "⏸"
    } else {
        "▶"
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

    // In the pool the monitor shows a raw file, so the clock reads the
    // asset position/duration and the mode tag says POOL.
    if is_pool(app) {
        let sel = pool_sel(app).unwrap_or(0);
        let (pos, dur) = match app.project.media.get(sel) {
            Some(a) => (pool_pos(app), a.duration().map(|d| secs(&d)).unwrap_or(0.0)),
            None => (0.0, 0.0),
        };
        let fs = app.project.media.get(sel).and_then(|a| a.fps());
        let line = Line::from(vec![
            Span::styled(" nle ", style.add_modifier(Modifier::BOLD)),
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

    let line = Line::from(vec![
        Span::styled(" nle ", style.add_modifier(Modifier::BOLD)),
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
    ]);
    f.render_widget(Paragraph::new(line), area);
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
fn track_parts(project: &Project, track: &Track, total: f64) -> Vec<(f64, String, Style)> {
    let gap_style = Style::default().bg(Color::DarkGray).fg(Color::Black);
    let mut parts: Vec<(f64, String, Style)> = track
        .segments
        .iter()
        .map(|s| match s {
            TimelineSegment::Clip(c) => {
                let name = project
                    .asset(c.asset)
                    .and_then(|a| a.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| format!("#{}", c.id));
                (
                    secs(&c.duration()),
                    name,
                    Style::default().bg(Color::Cyan).fg(Color::Black),
                )
            }
            TimelineSegment::Gap(g) => (secs(&g.duration), "gap".to_string(), gap_style),
        })
        .collect();
    let tail = total - secs(&track.end());
    if tail > 1e-9 {
        parts.push((tail, "gap".to_string(), gap_style));
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
    for (i, (dur, label, style)) in parts.iter().enumerate() {
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
            format!("▏{}", truncate_label(label, width.saturating_sub(1)))
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
mod tests {
    use super::*;
    use nle_core::media::{RawFormat, RawProbe, RawStream};
    use nle_core::timeline::{Gap as TimelineGap, Timeline, Track, TrackKind};
    use std::path::PathBuf;

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
                    }),
                ],
            }],
        };
        p
    }

    #[test]
    fn resolve_armed_matrix() {
        let p = asset_project();
        // Clip starts win (head-of-right at shared cuts).
        assert_eq!(
            resolve_armed(&p, 0.0, 0),
            Ok((Armed::Clip { id: 1, head: true }, 0.0))
        );
        assert_eq!(
            resolve_armed(&p, 5.0, 0),
            Ok((Armed::Clip { id: 2, head: true }, 5.0))
        );
        // Clip ends (gap starts resolve to the clip tail).
        // Clip end with a gap on the right: `e` arms the gap's head (the
        // edge at the cut), so it can be grown left over the clip.
        assert_eq!(
            resolve_armed(&p, 3.0, 0),
            Ok((Armed::GapHead { index: 1 }, 3.0))
        );
        assert_eq!(
            resolve_armed(&p, 7.0, 0),
            Ok((Armed::Clip { id: 2, head: false }, 7.0))
        );
        // Mid-segment positions refuse.
        assert!(resolve_armed(&p, 1.5, 0).is_err());
        assert!(resolve_armed(&p, 4.0, 0).is_err());
        // Shared clip|clip cut arms the right head.
        let mut q = asset_project();
        q.timeline.video_track_mut().unwrap().segments.remove(1);
        assert_eq!(
            resolve_armed(&q, 3.0, 0),
            Ok((Armed::Clip { id: 2, head: true }, 3.0))
        );
        // Virtual edges with gaps.
        let mut r = asset_project();
        r.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(3),
            }));
        assert_eq!(
            resolve_armed(&r, 10.0, 0),
            Ok((Armed::GapTail { index: 3 }, 10.0))
        );
        let mut s = asset_project();
        s.timeline.video_track_mut().unwrap().segments.insert(
            0,
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
        );
        assert_eq!(
            resolve_armed(&s, 0.0, 0),
            Ok((Armed::GapHead { index: 0 }, 0.0))
        );
        // Empty timeline refuses.
        assert!(resolve_armed(&Project::new(), 0.0, 0).is_err());
    }

    #[test]
    fn plan_extend_clip_bounds() {
        let p = asset_project();
        // Tail of #1 at 3.0: material-wide — lo is the far edge, hi is
        // the asset end mapped (gap no longer binds). Resolve is no longer
        // via plan_extend (e defaults to the gap on the right there).
        let plan = plan_for_armed(&p, Armed::Clip { id: 1, head: false }, 3.0, 0).unwrap();
        assert_eq!(plan.armed, Armed::Clip { id: 1, head: false });
        assert_eq!(plan.edge_pos, 3.0);
        assert_eq!((plan.lo, plan.hi), (0.0, Some(9.0)));
        assert_eq!(plan.mat, Some((-1.0, 9.0)));
        match plan.preview {
            ExtendPreviewSpec::Clip {
                src_start,
                length,
                origin,
                ..
            } => {
                assert_eq!(src_start, Seconds::from_integer(0));
                assert_eq!(length, Seconds::from_integer(10));
                assert_eq!(origin, -1.0);
            }
            _ => panic!("expected clip preview"),
        }
        // Head of #2 at 5.0: ain 0 means no left room.
        let plan = plan_extend(&p, 5.0, true, 0).unwrap();
        assert_eq!((plan.lo, plan.hi), (5.0, Some(7.0)));
        // Head of #1 at 0.0: the head material reaches past the timeline
        // start, so the range dips negative (prepend).
        let plan = plan_extend(&p, 0.0, false, 0).unwrap();
        assert_eq!((plan.lo, plan.hi), (-1.0, Some(3.0)));
        // Its preview stops at aout (4), not the whole asset, so mpv can't
        // play past the head's edge.
        match plan.preview {
            ExtendPreviewSpec::Clip { length, origin, .. } => {
                assert_eq!(length, Seconds::from_integer(4), "trimmed to aout");
                assert_eq!(origin, -1.0);
            }
            _ => panic!("expected clip preview"),
        }
        // Unknown asset duration: no extension room, trim/delete only,
        // no material strip, span-mapped preview.
        let mut q = asset_project();
        q.media[0].probe.format.duration = None;
        let plan = plan_for_armed(&q, Armed::Clip { id: 1, head: false }, 3.0, 0).unwrap();
        assert_eq!((plan.lo, plan.hi), (0.0, Some(3.0)));
        assert_eq!(plan.mat, None);
        match plan.preview {
            ExtendPreviewSpec::Clip {
                src_start,
                length,
                origin,
                ..
            } => {
                assert_eq!(src_start, Seconds::from_integer(1));
                assert_eq!(length, Seconds::from_integer(3));
                assert_eq!(origin, 0.0);
            }
            _ => panic!("expected clip preview"),
        }
    }

    // A timeline-start segment can extend left of time 0 by prepending its
    // head material; the timeline grows at the front.
    #[test]
    fn first_clip_head_extends_past_timeline_start() {
        let p = asset_project(); // C1 [0..3) src 1..4 of a 10s asset
        let head1 = Armed::Clip { id: 1, head: true };
        let plan = plan_for_armed(&p, head1, 0.0, 0).unwrap();
        assert_eq!(
            (plan.lo, plan.hi),
            (-1.0, Some(3.0)),
            "one second of head room below zero"
        );
        assert_eq!(
            plan_commit(&p, &head1, 0.0, -1.0, 0),
            Ok(CommitKind::AbsorbHead)
        );

        for ripple in [false, true] {
            let mut app = App::new(asset_project(), PathBuf::from("t.json"));
            app.playhead = -1.0;
            let batch = run_commit_action(&app, &head1, CommitKind::AbsorbHead, ripple).unwrap();
            let mut h = History::default();
            h.apply(&mut app.project, batch, 0).unwrap();
            let c1 = clip_by_id(&app.project, 1);
            assert_eq!(c1.asset_in, Seconds::from_integer(0), "head material used");
            assert_eq!(c1.duration(), Seconds::from_integer(4));
            assert_eq!(
                app.project.timeline.end(),
                Seconds::from_integer(8),
                "grew at the front"
            );
        }
    }

    // Rendering must survive a negative playhead (head extend past 0).
    #[test]
    fn extend_head_below_zero_renders() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let p = asset_project();
        let plan = plan_for_armed(&p, Armed::Clip { id: 1, head: true }, 0.0, 0).unwrap();
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = -1.0;
        app.mode = Mode::Extend(ExtendState {
            track: 0,
            armed: plan.armed,
            edge_pos: plan.edge_pos,
            ripple: false,
            lo: plan.lo,
            hi: plan.hi,
            mat: plan.mat,
            preview_off: -1.0, // edge 0 at source 1
        });
        let mut term = Terminal::new(TestBackend::new(60, 4)).unwrap();
        term.draw(|f| {
            render_timeline(
                f,
                Rect {
                    x: 0,
                    y: 0,
                    width: 60,
                    height: 4,
                },
                &app,
            )
        })
        .unwrap();
    }

    #[test]
    fn track_labels_number_within_kind() {
        let mut p = asset_project();
        p.timeline.add_track(TrackKind::Audio); // index 1
        p.timeline.add_track(TrackKind::Video); // index 2
        assert_eq!(track_label(&p, 0), "v1");
        assert_eq!(track_label(&p, 1), "a1");
        assert_eq!(track_label(&p, 2), "v2");
    }

    #[test]
    fn track_window_centres_on_the_active_track() {
        assert_eq!(track_window(2, 0, 4), (0, 2), "all fit");
        assert_eq!(track_window(10, 0, 4), (0, 4));
        assert_eq!(track_window(10, 9, 4), (6, 4), "clamped to the end");
        assert_eq!(track_window(10, 5, 4), (3, 4), "centred");
    }

    #[test]
    fn multi_track_timeline_draws_a_row_per_track() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = asset_project();
        p.timeline.add_track(TrackKind::Audio); // index 1, empty
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.active_track = 1;
        let mut term = Terminal::new(TestBackend::new(60, 6)).unwrap();
        term.draw(|f| {
            render_timeline(
                f,
                Rect {
                    x: 0,
                    y: 0,
                    width: 60,
                    height: 6,
                },
                &app,
            )
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let row = |y: u16| {
            (0..60)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        // With two tracks the gutter appears: row 1 is v1, row 2 is a1.
        assert!(row(1).contains("v1"), "{:?}", row(1));
        assert!(row(2).contains("a1"), "{:?}", row(2));
        assert!(!row(1).contains("a1"), "{:?}", row(1));
    }

    // A track shorter than the timeline reads as a trailing gap, not blank.
    #[test]
    fn shorter_track_pads_with_a_trailing_gap() {
        let mut p = asset_project(); // track 0 ends at 7.0
        let a = p.media[0].id;
        let ti = p.timeline.add_track(TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![TimelineSegment::Clip(nle_core::Clip {
            id: 7,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
        })];
        // The 2 s track is padded to the 7 s timeline with a 5 s gap.
        let parts = track_parts(&p, &p.timeline.tracks[ti], 7.0);
        assert_eq!(parts.len(), 2, "{parts:?}");
        let (dur, label, _) = parts.last().unwrap();
        assert_eq!(label, "gap");
        assert!((dur - 5.0).abs() < 1e-9, "{dur}");
        // A track that already reaches the end gains nothing.
        assert_eq!(track_parts(&p, &p.timeline.tracks[0], 7.0).len(), 3);
    }

    // The panel is sized so every track fits; the bottom panels are dropped
    // before a track is hidden.
    #[test]
    fn panel_heights_show_every_track_when_they_fit() {
        let (tl, keys, msg) = panel_heights(3, 24);
        assert!(tl >= 3 + 3, "3 tracks + marker + borders: {tl}");
        assert_eq!((keys, msg), (8, 3), "bottom panels still fit");
        // It grows with the track count instead of windowing.
        assert!(panel_heights(6, 24).0 > panel_heights(2, 24).0);
        // Too short: the bottom panels go first.
        assert_eq!(panel_heights(6, 14).1, 0);
        assert_eq!(panel_heights(6, 14).2, 0);
    }

    // The full render shows every track's gutter on a normal terminal.
    #[test]
    fn all_tracks_are_visible_in_the_timeline_panel() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = asset_project();
        p.timeline.add_track(TrackKind::Video); // v2
        p.timeline.add_track(TrackKind::Audio); // a1
        let app = App::new(p, PathBuf::from("t.json"));
        let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let row = |y: u16| {
            (0..60)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        // status at y=0, timeline border at y=1, tracks at y=2.., marker last.
        assert!(row(2).contains("v1"), "{:?}", row(2));
        assert!(row(3).contains("v2"), "{:?}", row(3));
        assert!(row(4).contains("a1"), "{:?}", row(4));
    }

    // The pool's asset list scrolls its viewport so the selected row stays
    // visible once there are more assets than fit (a stateless `List` always
    // draws from row 0 and lets the highlight walk off the bottom).
    #[test]
    fn pool_list_scrolls_to_keep_the_selection_visible() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = asset_project();
        let probe = p.asset(1).unwrap().probe.clone();
        for i in 2..=20 {
            p.add_asset(format!("/media/a{i}.mp4").into(), probe.clone());
        }
        let mut app = App::new(p, PathBuf::from("t.json"));
        open_pool_mode(&mut app, 19); // the last asset, far past a short pane
        let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
        term.draw(|f| render_pool(f, f.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..8u16)
            .map(|y| (0..60u16).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("a20.mp4"),
            "selected row must be visible:\n{text}"
        );
        assert!(
            !text.contains("a2.mp4"),
            "list scrolled, early rows gone:\n{text}"
        );
    }

    #[test]
    fn track_command_adds_mutes_and_removes() {
        let path = std::env::temp_dir().join(format!(
            "nle-track-command-test-{}.json",
            std::process::id()
        ));
        asset_project().save(&path).unwrap();
        let mut app = App::new(asset_project(), path.clone());
        assert!(track_command(&mut app, "track add audio").contains("a1"));
        assert_eq!(app.project.timeline.tracks.len(), 2);
        assert_eq!(app.active_track, 1, "the new track becomes active");
        assert!(track_command(&mut app, "track mute 2").contains("muted"));
        assert!(app.project.timeline.track(1).unwrap().muted);
        // Refuses to remove the only video track…
        assert!(track_command(&mut app, "track rm 1").contains("only video"));
        // …but removes the empty audio track.
        assert!(track_command(&mut app, "track rm 2").contains("removed"));
        assert_eq!(app.project.timeline.tracks.len(), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    }

    #[test]
    fn import_command_reports_usage_and_missing_files() {
        let mut app = App::new(asset_project(), temp_project_path("import-cmd"));
        assert!(import_command(&mut app, "import").contains("usage"));
        let msg = import_command(&mut app, "import /no/such/file.xyz");
        assert!(msg.contains("not found"), "{msg}");
        assert_eq!(app.project.media.len(), 1, "nothing was added");
    }

    // `:track rm N` takes the track's clips with it (no "empty it first").
    #[test]
    fn track_rm_deletes_a_nonempty_track_with_its_clips() {
        let path = std::env::temp_dir().join(format!(
            "nle-track-rm-nonempty-test-{}.json",
            std::process::id()
        ));
        let mut p = asset_project();
        let ai = p.timeline.add_track(TrackKind::Audio);
        p.timeline
            .track_mut(ai)
            .unwrap()
            .segments
            .push(TimelineSegment::Clip(nle_core::Clip::new(
                9,
                1,
                Seconds::from_integer(0),
                Seconds::from_integer(2),
            )));
        p.save(&path).unwrap();
        let mut app = App::new(p, path.clone());
        let msg = track_command(&mut app, "track rm 2");
        assert!(msg.contains("removed") && msg.contains("1 clip"), "{msg}");
        assert_eq!(app.project.timeline.tracks.len(), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(nle_core::History::sidecar_path(&path));
    }

    // A gap has no material, so its head extends without bound — including
    // past time 0, where it inserts empty space.
    #[test]
    fn gap_head_extends_past_zero() {
        // Insert a leading gap: [gap 2 @0..2][C1 2..5][gap 2 @5..7][C2 7..9]
        let mut p = asset_project();
        p.timeline.video_track_mut().unwrap().segments.insert(
            0,
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
        );
        let head0 = Armed::GapHead { index: 0 };
        let plan = plan_for_armed(&p, head0, 0.0, 0).unwrap();
        assert_eq!(plan.lo, f64::NEG_INFINITY, "unbounded leftward");
        assert_eq!(plan.hi, Some(2.0));
        assert_eq!(
            plan_commit(&p, &head0, 0.0, -1.0, 0),
            Ok(CommitKind::GapHead)
        );

        for ripple in [false, true] {
            let mut app = App::new(p.clone(), PathBuf::from("t.json"));
            app.playhead = -1.0;
            let batch = run_commit_action(&app, &head0, CommitKind::GapHead, ripple).unwrap();
            let mut h = History::default();
            h.apply(&mut app.project, batch, 0).unwrap();
            let segs = app.project.timeline.video_track().unwrap().segments.clone();
            assert!(
                matches!(&segs[0], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(3))
            );
            assert_eq!(
                app.project.timeline.end(),
                Seconds::from_integer(10),
                "grew at the front"
            );
        }
    }

    // Rendering must survive a gap head armed below 0 (unbounded lo).
    #[test]
    fn gap_head_below_zero_renders() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = asset_project();
        p.timeline.video_track_mut().unwrap().segments.insert(
            0,
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
        );
        let plan = plan_for_armed(&p, Armed::GapHead { index: 0 }, 0.0, 0).unwrap();
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = -1.0;
        app.mode = Mode::Extend(ExtendState {
            track: 0,
            armed: plan.armed,
            edge_pos: plan.edge_pos,
            ripple: false,
            lo: plan.lo,
            hi: plan.hi,
            mat: plan.mat,
            preview_off: 0.0,
        });
        let mut term = Terminal::new(TestBackend::new(60, 4)).unwrap();
        term.draw(|f| {
            render_timeline(
                f,
                Rect {
                    x: 0,
                    y: 0,
                    width: 60,
                    height: 4,
                },
                &app,
            )
        })
        .unwrap();
    }

    #[test]
    fn plan_extend_virtual_edges() {
        let mut p = asset_project();
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(3),
            }));
        let plan = plan_extend(&p, 10.0, false, 0).unwrap();
        assert_eq!(plan.armed, Armed::GapTail { index: 3 });
        assert_eq!(plan.lo, 7.0);
        assert_eq!(plan.hi, None);
        assert_eq!(plan.mat, None);
        assert!(matches!(plan.preview, ExtendPreviewSpec::Black { origin } if origin == 7.0));
        // Leading gap: unbounded head range, black preview, no material.
        let mut s = asset_project();
        s.timeline.video_track_mut().unwrap().segments.insert(
            0,
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
        );
        let plan = plan_extend(&s, 0.0, false, 0).unwrap();
        assert_eq!(plan.armed, Armed::GapHead { index: 0 });
        assert_eq!((plan.lo, plan.hi), (f64::NEG_INFINITY, Some(2.0)));
        assert_eq!(plan.mat, None);
    }

    #[test]
    fn clamp_extend_holds_both_modes() {
        let st = ExtendState {
            track: 0,
            armed: Armed::GapTail { index: 1 },
            edge_pos: 7.0,
            ripple: false,
            lo: 7.0,
            hi: Some(9.0),
            mat: None,
            preview_off: 7.0,
        };
        assert_eq!(clamp_extend(&st, 5.0), 7.0);
        assert_eq!(clamp_extend(&st, 8.0), 8.0);
        assert_eq!(clamp_extend(&st, 99.0), 9.0);
        let open = ExtendState {
            track: 0,
            hi: None,
            ..st.clone()
        };
        assert_eq!(clamp_extend(&open, 1e9), 1e9);
    }

    // Seek-clamping for a head: never past the segment's end (nor before
    // the material/timeline start).
    #[test]
    fn extend_head_nav_clamps_at_segment_end() {
        let fps = Seconds::new(30, 1);
        let st = ExtendState {
            track: 0,
            armed: Armed::Clip { id: 1, head: true },
            edge_pos: 0.0,
            ripple: false,
            lo: -1.0,
            hi: Some(3.0),
            mat: None,
            preview_off: -1.0,
        };
        assert_eq!(
            snap_extend_target(Some(&fps), &st, 99.0),
            3.0,
            "blocked at the end"
        );
        assert_eq!(
            snap_extend_target(Some(&fps), &st, -99.0),
            -1.0,
            "blocked at the start"
        );
        assert_eq!(snap_extend_target(Some(&fps), &st, 1.0), 1.0);
    }

    // Frame-stepping an armed head must reach negative positions when the
    // range allows it (a clip at the timeline start prepends head material);
    // the old code floored the index at 0.
    #[test]
    fn extend_step_reaches_a_negative_head_range() {
        let fps = Seconds::new(30, 1);
        assert_eq!(extend_step_index(-1.0, Some(3.0), fps, 0.0, -1, 1), -1);
        assert_eq!(
            extend_step_index(-1.0, Some(3.0), fps, -1.0, -1, 1),
            -30,
            "clamps at lo"
        );
        assert_eq!(
            extend_step_index(f64::NEG_INFINITY, Some(2.0), fps, 0.0, -1, 1),
            -1,
            "unbounded gap head"
        );
        assert_eq!(
            extend_step_index(0.0, Some(2.0), fps, 2.0, 1, 5),
            60,
            "clamps at hi"
        );
    }

    // Extending a tail to the asset's end leaves the material bound
    // off-grid (blue.mp4's 6.006s). seek_to_mode used to park the playhead
    // there unsnapped while the commit snapped the cut to a frame, so the
    // playhead read red and `]` reported no clip ending there.
    #[test]
    fn extend_to_material_end_lands_on_grid() {
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
                duration: Some("6.006".into()),
                ..Default::default()
            },
        };
        let (a, _) = p.add_asset("/media/blue.mp4".into(), probe);
        p.timeline.video_track_mut().unwrap().segments = vec![
            TimelineSegment::Clip(nle_core::Clip {
                id: 1,
                asset: a,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(3),
                speed: Seconds::from_integer(1),
            }),
            TimelineSegment::Clip(nle_core::Clip {
                id: 2,
                asset: a,
                asset_in: Seconds::new(1, 2),
                asset_out: Seconds::new(7, 2),
                speed: Seconds::from_integer(1),
            }),
        ];
        let (armed, edge) = resolve_armed(&p, 6.0, 0).unwrap();
        assert_eq!(armed, Armed::Clip { id: 2, head: false });
        let plan = plan_extend(&p, 6.0, false, 0).unwrap();
        let hi = plan.hi.unwrap();
        assert!((hi - 8.506).abs() < 1e-6, "material end off-grid: {hi}");
        let st = ExtendState {
            track: 0,
            armed: plan.armed,
            edge_pos: plan.edge_pos,
            ripple: false,
            lo: plan.lo,
            hi: plan.hi,
            mat: plan.mat,
            preview_off: edge - 3.5,
        };
        // Navigating past the end clamps to the material bound and snaps
        // to the frame grid, staying within the reachable range.
        let fps = p.fps;
        let target = snap_extend_target(fps.as_ref(), &st, hi + 1.0);
        assert_eq!(target, 8.5);
        assert!(target <= hi + 1e-9);

        // Commit there: the new cut is exactly at the snapped playhead.
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = target;
        let kind = plan_commit(&app.project, &armed, edge, target, 0).unwrap();
        let batch = run_commit_action(&app, &armed, kind, false).unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        assert!(
            on_boundary(&app.project, target, 0),
            "cut recognised on-grid"
        );
        assert_eq!(
            armed_on_side(&app.project, target, Side::Left, 0),
            Some((Armed::Clip { id: 2, head: false }, target)),
            "] finds the clip ending at the new cut"
        );
        assert!(
            !on_boundary(&app.project, hi, 0),
            "the raw off-grid bound is not a cut"
        );
    }

    #[test]
    fn plan_commit_routing_table() {
        let p = asset_project(); // C1 [0..3), gap [3..5), C2 [5..7)
        let tail1 = Armed::Clip { id: 1, head: false };
        assert_eq!(
            plan_commit(&p, &tail1, 3.0, 4.0, 0),
            Ok(CommitKind::GrowTailIntoGap)
        );
        assert_eq!(
            plan_commit(&p, &tail1, 3.0, 1.0, 0),
            Ok(CommitKind::TrimTail)
        );
        assert_eq!(plan_commit(&p, &tail1, 3.0, 0.0, 0), Ok(CommitKind::Delete));
        assert_eq!(plan_commit(&p, &tail1, 3.0, 3.0, 0), Ok(CommitKind::Noop));
        let head2 = Armed::Clip { id: 2, head: true };
        assert_eq!(
            plan_commit(&p, &head2, 5.0, 6.0, 0),
            Ok(CommitKind::TrimHead)
        );
        assert_eq!(
            plan_commit(&p, &head2, 5.0, 4.0, 0),
            Ok(CommitKind::GrowHeadIntoGap)
        );
        assert_eq!(plan_commit(&p, &head2, 5.0, 7.0, 0), Ok(CommitKind::Delete));
        assert_eq!(plan_commit(&p, &head2, 5.0, 5.0, 0), Ok(CommitKind::Noop));
        // Routing is flag-independent (ripple splits at run time).
        assert_eq!(
            plan_commit(&p, &tail1, 3.0, 4.0, 0),
            plan_commit(&p, &tail1, 3.0, 4.0, 0)
        );
        // Past-end tail with no following gap.
        let mut q = asset_project();
        q.timeline.video_track_mut().unwrap().segments.truncate(1);
        let tail = Armed::Clip { id: 1, head: false };
        assert_eq!(
            plan_commit(&q, &tail, 3.0, 4.0, 0),
            Ok(CommitKind::GrowTailPastEnd)
        );
        // Beyond the adjacent gap: absorb (both flags route here; run
        // splits e-replace vs E-push).
        assert_eq!(
            plan_commit(&p, &tail1, 3.0, 6.0, 0),
            Ok(CommitKind::AbsorbTail)
        );
        assert_eq!(
            plan_commit(&p, &head2, 5.0, 1.0, 0),
            Ok(CommitKind::AbsorbHead)
        );
        // Directly into the next clip: absorb as well.
        let mut s = asset_project();
        s.timeline.video_track_mut().unwrap().segments.remove(1);
        let tail = Armed::Clip { id: 1, head: false };
        assert_eq!(
            plan_commit(&s, &tail, 3.0, 4.0, 0),
            Ok(CommitKind::AbsorbTail)
        );
        // Virtual edges.
        let mut r = asset_project();
        r.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(3),
            }));
        assert_eq!(
            plan_commit(&r, &Armed::GapTail { index: 3 }, 10.0, 8.0, 0),
            Ok(CommitKind::GapTail)
        );
        assert_eq!(
            plan_commit(&r, &Armed::GapTail { index: 3 }, 10.0, 12.0, 0),
            Ok(CommitKind::GapTail)
        );
        assert_eq!(
            plan_commit(&r, &Armed::GapTail { index: 3 }, 10.0, 10.0, 0),
            Ok(CommitKind::Noop)
        );
    }

    #[test]
    fn run_commit_maps_kinds_to_ops() {
        let p = asset_project();
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 4.0; // inside the [3..5) gap
        let tail1 = Armed::Clip { id: 1, head: false };
        let b = run_commit_action(&app, &tail1, CommitKind::GrowTailIntoGap, false).unwrap();
        assert!(b.desc.contains("trim gap in"), "{}", b.desc);
        let b = run_commit_action(&app, &tail1, CommitKind::GrowTailIntoGap, true).unwrap();
        assert!(b.desc.contains("extend tail"), "{}", b.desc);
        // Opposite-end commit deletes the ARMED clip (tail1 = #1), not the
        // segment the playhead's boundary resolves to.
        let b = run_commit_action(&app, &tail1, CommitKind::Delete, true).unwrap();
        assert!(b.desc.contains("ripple delete"), "{}", b.desc);
        let b = run_commit_action(&app, &tail1, CommitKind::Delete, false).unwrap();
        assert!(b.desc.starts_with("delete clip #1"), "{}", b.desc);
        // Absorb splits by flag: e replaces covered content, E pushes it.
        app.playhead = 6.0; // past the gap, inside clip #2
        let b = run_commit_action(&app, &tail1, CommitKind::AbsorbTail, false).unwrap();
        assert!(b.desc.contains("absorbed"), "{}", b.desc);
        let b = run_commit_action(&app, &tail1, CommitKind::AbsorbTail, true).unwrap();
        assert!(b.desc.contains("extend tail"), "{}", b.desc);
    }

    // An extend commit records its undo position as where the mode was
    // entered (the armed edge), not the playhead it moved to.
    #[test]
    fn extend_commit_undo_returns_to_entry() {
        let p = asset_project(); // C1 tail armed at 3.0
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 3.0; // entered here
        app.mode = Mode::Extend(ExtendState {
            track: 0,
            armed: Armed::Clip { id: 1, head: false },
            edge_pos: 3.0,
            ripple: false,
            lo: 0.0,
            hi: Some(9.0),
            mat: None,
            preview_off: -1.0,
        });
        // Commit with the playhead moved to 6.0, via the run_edit path.
        app.playhead = 6.0;
        let batch = run_commit_action(
            &app,
            &Armed::Clip { id: 1, head: false },
            CommitKind::AbsorbTail,
            false,
        )
        .unwrap();
        // Mirror run_edit's attribution: extend uses the entry edge.
        let from = match &app.mode {
            Mode::Extend(st) => st.edge_pos,
            _ => app.playhead,
        };
        assert_eq!(from, 3.0);
        let frame = frame_of(app.project.fps.as_ref(), from);
        app.history.apply(&mut app.project, batch, frame).unwrap();
        // Move away, then undo.
        app.playhead = 1.0;
        let outcome = app.history.undo(&mut app.project).unwrap();
        app.playhead = frame_to_secs(app.project.fps.as_ref(), outcome.playhead);
        assert_eq!(app.playhead, 3.0, "back to where extend mode was entered");
    }

    // After committing a tail extend the playhead lands on the new clip
    // end (the edge we extended to), for both ripple and absorb.
    #[test]
    fn tail_extend_commit_lands_on_the_new_end() {
        for (kind, ripple) in [
            (CommitKind::AbsorbTail, false),
            (CommitKind::AbsorbTail, true),
        ] {
            let mut app = App::new(asset_project(), PathBuf::from("t.json"));
            app.playhead = 6.0; // extend C1's tail [0..3) out to 6.0
            app.mode = Mode::Extend(ExtendState {
                track: 0,
                armed: Armed::Clip { id: 1, head: false },
                edge_pos: 3.0,
                ripple,
                lo: 0.0,
                hi: Some(9.0),
                mat: None,
                preview_off: -1.0,
            });
            let target = app.playhead;
            let batch =
                run_commit_action(&app, &Armed::Clip { id: 1, head: false }, kind, ripple).unwrap();
            // Mirror run_edit's extend attribution (the armed edge).
            let frame = frame_of(app.project.fps.as_ref(), 3.0);
            app.history.apply(&mut app.project, batch, frame).unwrap();
            let end = {
                let track = app.project.timeline.video_track().unwrap();
                let si = track.clip_index(1).unwrap();
                secs(&(track.start_of(si) + track.segments[si].duration()))
            };
            assert_eq!(
                end, target,
                "clip end == committed target (ripple={ripple})"
            );
        }
    }

    // Any gap (here a middle one) can be armed at either edge and
    // extended. The tail ripples/overwrites what follows; the head
    // overwrites the preceding segment.
    #[test]
    fn arm_and_extend_middle_gap() {
        let p = asset_project(); // [C1 0..3][gap 3..5][C2 5..7]
        // `]` (Left) at the gap's end arms its tail; `[` (Right) at its
        // start arms its head — each edge sits at the cut.
        assert_eq!(
            armed_on_side(&p, 5.0, Side::Left, 0),
            Some((Armed::GapTail { index: 1 }, 5.0))
        );
        assert_eq!(
            armed_on_side(&p, 3.0, Side::Right, 0),
            Some((Armed::GapHead { index: 1 }, 3.0))
        );
        // `e` defaults to the gap's head at a clip|gap cut.
        assert_eq!(
            resolve_armed(&p, 3.0, 0),
            Ok((Armed::GapHead { index: 1 }, 3.0))
        );
        assert_eq!(
            resolve_armed(&p, 5.0, 0),
            Ok((Armed::Clip { id: 2, head: true }, 5.0))
        );

        let plan = plan_for_armed(&p, Armed::GapTail { index: 1 }, 5.0, 0).unwrap();
        assert_eq!((plan.lo, plan.hi), (3.0, None), "unbounded");
        assert!(matches!(plan.preview, ExtendPreviewSpec::Black { origin } if origin == 3.0));
        assert_eq!(
            plan_commit(&p, &Armed::GapTail { index: 1 }, 5.0, 9.0, 0),
            Ok(CommitKind::GapTail)
        );

        // Head plan: unbounded leftward (a gap has no material — extending
        // the head inserts space, even past time 0).
        let head = plan_for_armed(&p, Armed::GapHead { index: 1 }, 3.0, 0).unwrap();
        assert_eq!((head.lo, head.hi), (f64::NEG_INFINITY, Some(5.0)));
        assert_eq!(
            plan_commit(&p, &Armed::GapHead { index: 1 }, 3.0, 1.0, 0),
            Ok(CommitKind::GapHead)
        );
        assert_eq!(
            plan_commit(&p, &Armed::GapHead { index: 1 }, 3.0, 3.0, 0),
            Ok(CommitKind::Noop)
        );
        // Committing at a gap's opposite end removes it (ripple), like a
        // delete everywhere else.
        assert_eq!(
            plan_commit(&p, &Armed::GapTail { index: 1 }, 5.0, 3.0, 0),
            Ok(CommitKind::Delete),
            "gap tail walked to its start"
        );
        assert_eq!(
            plan_commit(&p, &Armed::GapHead { index: 1 }, 3.0, 5.0, 0),
            Ok(CommitKind::Delete),
            "gap head walked to its end"
        );
        // Committing that delete ripples the gap shut.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 3.0;
        let batch = run_commit_action(
            &app,
            &Armed::GapTail { index: 1 },
            CommitKind::Delete,
            false,
        )
        .unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        assert_eq!(
            app.project.timeline.end(),
            Seconds::from_integer(5),
            "gap removed, length shrank"
        );
        assert_eq!(
            app.project.timeline.video_track().unwrap().segments.len(),
            2
        );

        // Head grow to 1.0: clip #1's tail is trimmed, the gap grows left.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 1.0;
        let batch = run_commit_action(
            &app,
            &Armed::GapHead { index: 1 },
            CommitKind::GapHead,
            false,
        )
        .unwrap();
        assert!(batch.desc.contains("trimmed clip #1"), "{}", batch.desc);
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        let segs = app.project.timeline.video_track().unwrap().segments.clone();
        assert!(matches!(&segs[0], TimelineSegment::Clip(c) if c.id == 1));
        assert_eq!(
            clip_by_id(&app.project, 1).duration(),
            Seconds::from_integer(1)
        );
        assert!(
            matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(4))
        );
        assert_eq!(
            app.project.timeline.end(),
            Seconds::from_integer(7),
            "length held"
        );

        // Head grow to 0.0 consumes clip #1 entirely.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 0.0;
        let batch = run_commit_action(
            &app,
            &Armed::GapHead { index: 1 },
            CommitKind::GapHead,
            false,
        )
        .unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        let segs = app.project.timeline.video_track().unwrap().segments.clone();
        assert!(
            matches!(&segs[0], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(5))
        );
        assert_eq!(app.project.timeline.end(), Seconds::from_integer(7));

        // E (ripple): grow the gap tail to 7 — clip #2 slides right.
        let armed = Armed::GapTail { index: 1 };
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 7.0;
        let batch = run_commit_action(&app, &armed, CommitKind::GapTail, true).unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        let segs = app.project.timeline.video_track().unwrap().segments.clone();
        assert!(
            matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(4))
        );
        assert_eq!(app.project.timeline.end(), Seconds::from_integer(9));
        assert_eq!(
            clip_by_id(&app.project, 2).duration(),
            Seconds::from_integer(2)
        );

        // e (replace): grow to 7 overwrites clip #2 entirely; length held.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 7.0;
        let batch = run_commit_action(&app, &armed, CommitKind::GapTail, false).unwrap();
        assert!(batch.desc.contains("absorbed"), "{}", batch.desc);
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        let segs = app.project.timeline.video_track().unwrap().segments.clone();
        assert!(
            matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(4))
        );
        assert_eq!(
            app.project.timeline.end(),
            Seconds::from_integer(7),
            "length held"
        );
        assert_eq!(segs.len(), 2, "clip #2 replaced by gap");

        // Shrink it to 4: clip #2 slides left (either flag).
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 4.0;
        let batch = run_commit_action(&app, &armed, CommitKind::GapTail, false).unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        let segs = app.project.timeline.video_track().unwrap().segments.clone();
        assert!(
            matches!(&segs[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(1))
        );
        assert_eq!(app.project.timeline.end(), Seconds::from_integer(6));
        assert_eq!(
            clip_by_id(&app.project, 2).duration(),
            Seconds::from_integer(2)
        );
    }

    // The status projects the post-commit timeline length.
    #[test]
    fn projected_total_reflects_pending_commit() {
        let make = |ripple: bool, armed: Armed, edge: f64, ph: f64| {
            let mut app = App::new(asset_project(), PathBuf::from("t.json"));
            app.playhead = ph;
            let st = ExtendState {
                track: 0,
                armed,
                edge_pos: edge,
                ripple,
                lo: 0.0,
                hi: None,
                mat: None,
                preview_off: 0.0,
            };
            app.mode = Mode::Extend(st.clone());
            (app, st)
        };
        // Ripple-growing the gap to 7 pushes clip #2 right: timeline 9.
        let (app, st) = make(true, Armed::GapTail { index: 1 }, 5.0, 7.0);
        assert_eq!(projected_total(&app, &st), 9.0);
        // Replace-growing absorbs clip #2: length held at 7.
        let (app, st) = make(false, Armed::GapTail { index: 1 }, 5.0, 7.0);
        assert_eq!(projected_total(&app, &st), 7.0);
        // A clip tail grown past the end projects the new end.
        let (app, st) = make(true, Armed::Clip { id: 2, head: false }, 7.0, 8.0);
        assert_eq!(projected_total(&app, &st), 8.0);
    }

    // i/I/o/O on a gap all ripple; on a clip the case distinction stays.
    #[test]
    fn trim_key_ripples_on_gaps() {
        let p = asset_project(); // [C1 0..3][gap 3..5][C2 5..7]
        let a = trim_key(&p, 4.0, false, false, 0).unwrap();
        let b = trim_key(&p, 4.0, false, true, 0).unwrap();
        assert_eq!(a.ops, b.ops, "i and I match on a gap");
        assert!(a.desc.contains("ripple trim gap in"), "{}", a.desc);
        let c = trim_key(&p, 4.0, true, false, 0).unwrap();
        assert_eq!(c.ops, trim_key(&p, 4.0, true, true, 0).unwrap().ops);
        assert!(c.desc.contains("ripple trim gap out"), "{}", c.desc);
        // On a clip, i (gap left behind) and I (ripple) differ.
        assert_ne!(
            trim_key(&p, 1.0, false, false, 0).unwrap().ops,
            trim_key(&p, 1.0, false, true, 0).unwrap().ops
        );
    }

    // yy grabs whichever segment is under the playhead — gap included.
    #[test]
    fn yank_grabs_gaps_too() {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 4.0; // inside the [3..5) gap
        edit_yank(&mut app);
        assert!(
            matches!(&app.clipboard, Some(TimelineSegment::Gap(g)) if g.duration == Seconds::from_integer(2))
        );
        app.playhead = 1.0; // inside clip #1
        edit_yank(&mut app);
        assert!(matches!(&app.clipboard, Some(TimelineSegment::Clip(c)) if c.id == 1));
    }

    // yy/dd/D follow the active track, not the first video track.
    #[test]
    fn yank_follows_the_active_track() {
        let mut p = asset_project(); // track 0: C1 [0..3), gap, C2 [5..7)
        let a = p.media[0].id;
        let ti = p.timeline.add_track(TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![TimelineSegment::Clip(nle_core::Clip {
            id: 42,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
        })];
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.active_track = ti;
        app.playhead = 1.0; // over clip #42 on track 1 and clip #1 on track 0
        edit_yank(&mut app);
        assert!(
            matches!(&app.clipboard, Some(TimelineSegment::Clip(c)) if c.id == 42),
            "yanked the active track's clip, not v1's: {:?}",
            app.clipboard
        );
    }

    // At a segment edge, i/o delete it: non-ripple leaves a gap, capital
    // ripples, and a gap always ripples.
    #[test]
    fn trim_key_deletes_at_segment_edges() {
        let p = asset_project(); // [C1 0..3][gap 3..5][C2 5..7]
        let b = trim_key(&p, 0.0, true, false, 0).unwrap(); // o at C1's start
        assert!(b.desc.contains("delete clip #1"), "{}", b.desc);
        let b = trim_key(&p, 0.0, true, true, 0).unwrap(); // O at C1's start
        assert!(b.desc.contains("ripple delete"), "{}", b.desc);
        let b = trim_key(&p, 3.0, false, false, 0).unwrap(); // i at C1's end
        assert!(b.desc.contains("delete clip #1"), "{}", b.desc);
        let b = trim_key(&p, 3.0, true, false, 0).unwrap(); // o at the gap's start
        assert!(b.desc.contains("delete gap (ripple)"), "{}", b.desc);
        let b = trim_key(&p, 5.0, true, false, 0).unwrap(); // o at C2's start
        assert!(b.desc.contains("delete clip #2"), "{}", b.desc);
    }

    // After an edit the playhead stays on the media frame it was showing,
    // not the same timecode.
    #[test]
    fn playhead_reanchors_to_the_media_frame() {
        // Ripple head trim: the frame under the playhead moves back, so
        // the playhead follows it.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 1.0; // C1 [0..3) src 1..4 → source 2
        let anchor = edit_anchor(&app.project, app.playhead, 0);
        assert_eq!(
            anchor,
            Some(Anchor::Media(MediaAnchor {
                id: 1,
                source: Seconds::from_integer(2),
                start: 0.0,
                at: 1.0
            }))
        );
        let batch = edit::trim_in_ripple(&app.project, app.playhead).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        reanchor_playhead(&mut app, anchor);
        assert_eq!(app.playhead, 0.0, "moved back by the trimmed second");

        // Gap-left head trim: the clip's head moves to the playhead and a
        // gap takes the trimmed material — the frame is unchanged.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 1.0;
        let anchor = edit_anchor(&app.project, app.playhead, 0);
        let batch = edit::trim_in(&app.project, app.playhead).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        reanchor_playhead(&mut app, anchor);
        assert_eq!(app.playhead, 1.0, "frame unchanged");

        // Tail trim: material after the playhead is removed — unchanged.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 1.0;
        let anchor = edit_anchor(&app.project, app.playhead, 0);
        let batch = edit::trim_out(&app.project, app.playhead).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        reanchor_playhead(&mut app, anchor);
        assert_eq!(app.playhead, 1.0, "frame unchanged");

        // Delete of the clip under the playhead: a plain delete leaves a
        // gap of the same length, so the playhead keeps its offset in it.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 1.0;
        let anchor = edit_anchor(&app.project, app.playhead, 0);
        let batch = edit::delete_under(&app.project, app.playhead).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        reanchor_playhead(&mut app, anchor);
        assert_eq!(app.playhead, 1.0, "same position in the resulting gap");

        // Ripple delete between two clips: the playhead lands on the cut
        // the neighbours now share.
        let mut p = asset_project();
        p.timeline.video_track_mut().unwrap().segments = vec![
            TimelineSegment::Clip(nle_core::Clip {
                id: 1,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            }),
            TimelineSegment::Clip(nle_core::Clip {
                id: 2,
                asset: 1,
                asset_in: Seconds::from_integer(4),
                asset_out: Seconds::from_integer(6),
                speed: Seconds::from_integer(1),
            }),
            TimelineSegment::Clip(nle_core::Clip {
                id: 3,
                asset: 1,
                asset_in: Seconds::from_integer(8),
                asset_out: Seconds::from_integer(10),
                speed: Seconds::from_integer(1),
            }),
        ];
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 3.0; // inside clip #2 [2..4)
        let anchor = clip_anchor(&app.project, app.playhead).unwrap();
        assert_eq!(anchor.start, 2.0);
        let batch = edit::delete_ripple(&app.project, app.playhead).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        reanchor_playhead(&mut app, Some(Anchor::Media(anchor)));
        assert_eq!(app.playhead, 2.0, "cut between the neighbours");

        // Deleting a gap always ripples, so the playhead lands on the cut
        // its neighbours now share (its start).
        let mut app = App::new(asset_project(), PathBuf::from("t.json")); // [C1][gap 3..5][C2]
        app.playhead = 4.0; // inside the gap
        let anchor = edit_anchor(&app.project, app.playhead, 0);
        assert_eq!(anchor, Some(Anchor::Gap(GapAnchor { start: 3.0 })));
        let batch = edit::delete_under(&app.project, app.playhead).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        reanchor_playhead(&mut app, anchor);
        assert_eq!(app.playhead, 3.0, "cut between the clips");

        // Over a gap clip_anchor is None, but edit_anchor is not.
        let app = App::new(asset_project(), PathBuf::from("t.json"));
        assert_eq!(clip_anchor(&app.project, 4.0), None);
        assert!(matches!(
            edit_anchor(&app.project, 4.0, 0),
            Some(Anchor::Gap(_))
        ));
    }

    // The anchor maps through a speed clip's rate: the media frame is a
    // SOURCE position, and source = timeline × speed.
    #[test]
    fn playhead_reanchors_on_speed_clips() {
        // A 2× clip reading source [0,4) occupies timeline [0,2). The
        // playhead at 1.0 shows source frame 2.
        let mut p = asset_project();
        p.timeline.video_track_mut().unwrap().segments =
            vec![TimelineSegment::Clip(nle_core::Clip {
                id: 1,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(4),
                speed: Seconds::from_integer(2),
            })];
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 1.0;
        let anchor = edit_anchor(&app.project, app.playhead, 0);
        assert_eq!(
            anchor,
            Some(Anchor::Media(MediaAnchor {
                id: 1,
                source: Seconds::from_integer(2), // 1.0 × 2, not 1.0
                start: 0.0,
                at: 1.0
            }))
        );
        // Head trim to 0.5 (gap-left): the clip's head moves right onto the
        // playhead and a gap takes the trimmed material. Source 2 now sits
        // at timeline 0.5 (gap) + 0.5 (into the clip) = 1.0 — the same
        // media frame at the same playhead. A 1× mapping records source 1
        // and resolves to 0.5, off the frame.
        let batch = edit::trim_in(&app.project, 0.5).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        reanchor_playhead(&mut app, anchor);
        assert_eq!(app.playhead, 1.0, "media frame kept at the scaled offset");

        // A ripple shift under a 0.5× clip: source 1 is 2s into the clip
        // and stays 2s into it after the clip slides left.
        let mut p = asset_project();
        p.timeline.video_track_mut().unwrap().segments = vec![
            TimelineSegment::Clip(nle_core::Clip {
                id: 1,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(1),
                speed: Seconds::from_integer(1),
            }),
            TimelineSegment::Clip(nle_core::Clip {
                id: 2,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::new(1, 2),
            }),
        ];
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 3.0; // inside clip #2 [1..5): source 0 + (3-1)×0.5 = 1
        let anchor = edit_anchor(&app.project, app.playhead, 0);
        assert_eq!(
            anchor,
            Some(Anchor::Media(MediaAnchor {
                id: 2,
                source: Seconds::from_integer(1),
                start: 1.0,
                at: 3.0
            }))
        );
        let batch = edit::delete_ripple(&app.project, 0.5).unwrap(); // drop #1
        app.history.apply(&mut app.project, batch, 0).unwrap();
        reanchor_playhead(&mut app, anchor);
        assert_eq!(app.playhead, 2.0, "still 2s into clip #2 after the shift");
    }

    // Undo restores the playhead to the frame the edit was made from.
    #[test]
    fn undo_restores_the_playhead() {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        let fps = app.project.fps;
        app.playhead = 1.0;
        let at_frame = frame_of(fps.as_ref(), app.playhead);
        let batch = edit::trim_in_ripple(&app.project, app.playhead).unwrap();
        app.history
            .apply(&mut app.project, batch, at_frame)
            .unwrap();
        // move away, as the user would after editing
        app.playhead = 6.0;
        let outcome = app.history.undo(&mut app.project).unwrap();
        app.playhead = frame_to_secs(app.project.fps.as_ref(), outcome.playhead);
        assert_eq!(app.playhead, 1.0, "playhead returns to where the edit was");
    }

    // `_` inserts a gap of `count` frames at the playhead.
    #[test]
    fn insert_gap_key_uses_the_count() {
        // Over the [3..5) gap: 5 frames grows it by 5/30 s.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 4.0;
        let batch = edit::insert_gap(&app.project, app.playhead, 5).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        assert!(
            matches!(&app.project.timeline.video_track().unwrap().segments[1], TimelineSegment::Gap(g) if g.duration == Seconds::from_integer(2) + Seconds::new(5, 30))
        );
    }

    // The pool's `gg` needs both keys: a lone `g` only arms the sequence.
    #[test]
    fn completes_gg_needs_both_keys() {
        assert!(completes_gg(Pending::G, KeyCode::Char('g')));
        assert!(!completes_gg(Pending::None, KeyCode::Char('g')));
        assert!(!completes_gg(Pending::G, KeyCode::Char('G')));
    }

    // Media pool: an asset used by a clip can't be removed; an unused one
    // goes. Insert-from-pool is the shared ripple path (tested in core).
    #[test]
    fn media_pool_guarded_removal() {
        let mut p = asset_project();
        let (spare, _) = p.add_asset(
            "/media/spare.mp4".into(),
            RawProbe {
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
                    duration: Some("5".into()),
                    ..Default::default()
                },
            },
        );
        // Insert the spare as a clip (core path), then it's in use.
        let snapshot = TimelineSegment::Clip(Clip {
            id: 0,
            asset: spare,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::from_integer(1),
        });
        let batch = edit::paste_ripple(&p, &snapshot, 0.0).unwrap();
        p.timeline = {
            let mut q = p.clone();
            edit::History::default().apply(&mut q, batch, 0).unwrap();
            q.timeline
        };
        assert_eq!(p.asset_usage(spare).len(), 1);
        assert!(p.remove_asset(spare).is_err(), "in use → refused");

        // An unused import removes cleanly.
        let (free, _) = p.add_asset(
            "/media/free.mp4".into(),
            p.asset(spare).unwrap().probe.clone(),
        );
        assert!(free != spare);
        assert!(p.remove_asset(free).is_ok());
    }

    /// Put the app in the pool panel with `sel` selected (no player needed).
    fn open_pool_mode(app: &mut App, sel: usize) {
        app.mode = Mode::Pool(PoolState {
            sel,
            pos: 0.0,
            resume: 0.0,
        });
    }

    // Inserting an asset from the pool is recorded in history and undoes.
    #[test]
    fn media_pool_insert_is_undoable() {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 0.0;
        open_pool_mode(&mut app, 0); // first asset
        let end_before = app.project.timeline.end();

        // Same call insert_from_pool makes, minus the player hook.
        let snapshot = TimelineSegment::Clip(Clip {
            id: 0,
            asset: app.project.media[0].id,
            asset_in: Seconds::from_integer(0),
            asset_out: app.project.media[0].duration().unwrap(),
            speed: Seconds::from_integer(1),
        });
        let batch = edit::paste_ripple(&app.project, &snapshot, app.playhead).unwrap();
        let frame = frame_of(app.project.fps.as_ref(), 0.0);
        app.history.apply(&mut app.project, batch, frame).unwrap();
        assert!(
            app.project.timeline.end() > end_before,
            "insert grew the timeline"
        );

        let outcome = app.history.undo(&mut app.project).unwrap();
        assert_eq!(
            app.project.timeline.end(),
            end_before,
            "undo removed the insert"
        );
        assert_eq!(
            frame_to_secs(app.project.fps.as_ref(), outcome.playhead),
            0.0
        );
    }

    // Clip-edge dividers on the track row land on the same columns as the
    // cut positions the marker row/playhead use (one quantizer).
    #[test]
    fn clip_edges_align_with_cut_positions() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // Four equal clips on the shared asset → cuts at 3,6,9,12.
        let mut p = asset_project();
        p.timeline.video_track_mut().unwrap().segments = (1..=4)
            .map(|id| {
                TimelineSegment::Clip(Clip {
                    id,
                    asset: 1,
                    asset_in: Seconds::from_integer(0),
                    asset_out: Seconds::from_integer(3),
                    speed: Seconds::from_integer(1),
                })
            })
            .collect();
        let app = App::new(p, PathBuf::from("t.json"));
        let width = 80u16;
        let mut term = Terminal::new(TestBackend::new(width, 4)).unwrap();
        term.draw(|f| {
            render_timeline(
                f,
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 4,
                },
                &app,
            )
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let inner = Block::bordered().inner(Rect {
            x: 0,
            y: 0,
            width,
            height: 4,
        });

        // Every drawn divider is a `▏` glyph at a clip boundary; collect
        // its column (buffer x, offset by the border) and compare to the
        // quantized cut.
        let divider_cols: Vec<u16> = (0..width)
            .filter(|&x| buf[(x, 1)].symbol() == "▏")
            .collect();
        for cut in [3.0, 6.0, 9.0] {
            let want = inner.x + marker_col(cut, 12.0, inner.width);
            assert!(
                divider_cols.contains(&want),
                "divider at cut {cut} → col {want}, got {divider_cols:?}"
            );
        }
    }

    // Inserting a 6.006 s asset uses a whole-frame duration, so no
    // sub-frame stump is produced.
    #[test]
    fn pool_insert_uses_whole_frames() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.add_asset(
            "/media/blue.mp4".into(),
            RawProbe {
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
                    duration: Some("6.006".into()),
                    ..Default::default()
                },
            },
        );
        let mut app = App::new(p, PathBuf::from("t.json"));
        // Mirror insert_from_pool's whole-frame math.
        let d = app.project.media[0].duration().unwrap();
        let fps = app.project.fps.unwrap();
        let frames = (secs(&d) * secs(&fps)).floor().max(1.0) as i64;
        let whole = Seconds::new(frames, 1) / fps;
        assert_eq!(
            whole,
            Seconds::new(180, 30),
            "6.006 floored to 180 frames (6.0 s)"
        );
        let snapshot = TimelineSegment::Clip(Clip {
            id: 0,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: whole,
            speed: Seconds::from_integer(1),
        });
        let batch = edit::paste_ripple(&app.project, &snapshot, 0.0).unwrap();
        app.history.apply(&mut app.project, batch, 0).unwrap();
        // No zero-length or sub-frame segment created.
        for s in &app.project.timeline.video_track().unwrap().segments {
            assert!(s.duration() > Seconds::from_integer(0));
        }
    }

    // Frame steps advance on the asset's grid and clamp to its last frame.
    #[test]
    fn pool_step_moves_one_frame_and_clamps() {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        open_pool_mode(&mut app, 0);
        let fps = app.project.media[0].fps().unwrap();
        let f = secs(&fps);
        set_pool_pos(&mut app, 0.0);
        // Three forward steps land exactly 3 frames in (snapped, no drift).
        for _ in 0..3 {
            let (t, _) = pool_step_target(&app, 1);
            set_pool_pos(&mut app, t);
        }
        assert!((pool_pos(&app) - 3.0 / f).abs() < 1e-9);
        // Back two → one frame in.
        let (t, _) = pool_step_target(&app, -1);
        set_pool_pos(&mut app, t);
        let (t, _) = pool_step_target(&app, -1);
        assert!((t - 1.0 / f).abs() < 1e-9);
        // Clamps at 0 and at the last frame.
        let (t0, _) = pool_step_target(&app, -99);
        assert_eq!(t0, 0.0);
        let last = pool_last_frame(&app, 10.0);
        set_pool_pos(&mut app, last);
        let (tl, _) = pool_step_target(&app, 99);
        assert!((tl - last).abs() < 1e-9, "clamped to the last frame");
    }

    // The pool clock's last frame is one frame before the asset duration.
    #[test]
    fn pool_last_frame_is_one_before_duration() {
        let app = App::new(asset_project(), PathBuf::from("t.json"));
        // 30 fps, 10 s asset → last frame at 10 - 1/30.
        assert!((pool_last_frame(&app, 10.0) - (10.0 - 1.0 / 30.0)).abs() < 1e-9);
    }

    // Metadata pane formats the stored probe (no re-probe).
    #[test]
    fn asset_metadata_lists_streams() {
        let p = asset_project();
        let a = p.asset(1).unwrap();
        let md = asset_metadata(a).join("\n");
        assert!(md.contains("red.mp4"), "{md}");
        assert!(md.contains("320×240"), "{md}");
        assert!(md.contains("h264"), "{md}");
        assert!(md.contains("30/1"), "{md}");
    }

    // The pool renders list + metadata without panicking.
    #[test]
    fn media_pool_renders() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = asset_project();
        // A second asset so list index (1) differs from asset id (2).
        p.add_asset("/media/blue.mp4".into(), p.asset(1).unwrap().probe.clone());
        let mut app = App::new(p, PathBuf::from("t.json"));
        open_pool_mode(&mut app, 1); // second row

        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..20)
            .map(|y| {
                (0..60)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("media pool"), "{text}");
        assert!(text.contains("red.mp4"), "{text}");

        // The highlighted (cyan-bg) POOL row must hold the SECOND asset
        // (blue), not the first — index-based, not id-based. Find the pool
        // block by its title row first.
        let title_y = (0..20u16)
            .find(|&y| {
                let row: String = (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect();
                row.contains("media pool")
            })
            .expect("pool title present");
        let cyan_pool_row =
            (title_y..20u16).find(|&y| (0..60).any(|x| buf[(x, y)].bg == Color::Cyan));
        let row: String = cyan_pool_row
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .unwrap_or_default();
        assert!(
            row.contains("blue.mp4"),
            "highlight on the selected row: {row:?}"
        );
        assert!(!row.contains("red.mp4"), "not the first row: {row:?}");
    }

    // The bottom panels: a keys cheat-sheet and a dedicated notice panel.
    #[test]
    fn hints_and_notice_panels_render() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let render_text = |app: &App| {
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            term.draw(|f| render(f, app)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..24)
                .map(|y| {
                    (0..80)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Normal mode: the cheat-sheet and a separate notice panel.
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.set_message("extend tail of clip #1 (absorbed gap)");
        let text = render_text(&app);
        assert!(text.contains("keys"), "keys panel title: {text}");
        assert!(text.contains("message"), "message panel title: {text}");
        assert!(text.contains("edit"), "cheat-sheet present: {text}");
        assert!(text.contains("absorbed gap"), "notice shown: {text}");

        // Pool mode: the pool hint line replaces the timeline sheet.
        open_pool_mode(&mut app, 0);
        let text = render_text(&app);
        assert!(text.contains("media pool"), "{text}");
        assert!(text.contains("enter insert"), "{text}");
    }

    fn clip_by_id(p: &Project, id: u32) -> nle_core::Clip {
        p.timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .filter_map(|s| match s {
                TimelineSegment::Clip(c) => Some(c),
                TimelineSegment::Gap(_) => None,
            })
            .find(|c| c.id == id)
            .unwrap()
            .clone()
    }

    // Extending a tail to the adjacent gap's far edge (exactly where the
    // next clip starts — where `j` lands, and where 1s `l` steps often
    // land) must absorb the now-fully-spanned gap, not call trim_in at the
    // boundary and fail with "nothing to trim".
    #[test]
    fn commit_tail_to_gap_far_edge_absorbs() {
        let p = asset_project(); // [C1 [0..3) src1..4] [gap 2 @3..5] [C2 [5..7)]
        // `e` now defaults to the gap on the right, so arm the clip
        // explicitly with `]` (Left).
        let (tail1, edge) = armed_on_side(&p, 3.0, Side::Left, 0).unwrap();
        assert_eq!(tail1, Armed::Clip { id: 1, head: false });
        assert_eq!(
            plan_commit(&p, &tail1, edge, 5.0, 0),
            Ok(CommitKind::AbsorbTail),
            "the gap's far edge is not 'inside' the gap"
        );
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.playhead = 5.0;
        let batch = run_commit_action(&app, &tail1, CommitKind::AbsorbTail, false).unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        assert_eq!(
            clip_by_id(&app.project, 1).asset_out,
            Seconds::from_integer(6)
        );
        assert_eq!(app.project.timeline.end(), Seconds::from_integer(7));
        assert_eq!(
            app.project.timeline.video_track().unwrap().segments.len(),
            2,
            "gap absorbed"
        );
    }

    // Symmetric: a head reaching the adjacent gap's near edge absorbs it.
    #[test]
    fn commit_head_to_gap_near_edge_absorbs() {
        // [C1 [0..3)] [gap 1 @3..4] [C2 [4..6) src1..3]: one second of head
        // room, enough to reach the gap's near edge at 3.0.
        let mut p = asset_project();
        {
            let segs = &mut p.timeline.video_track_mut().unwrap().segments;
            segs[1] = TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(1),
            });
            segs[2] = TimelineSegment::Clip(nle_core::Clip {
                id: 2,
                asset: 1,
                asset_in: Seconds::from_integer(1),
                asset_out: Seconds::from_integer(3),
                speed: Seconds::from_integer(1),
            });
        }
        let (head2, edge) = resolve_armed(&p, 4.0, 0).unwrap();
        assert_eq!(head2, Armed::Clip { id: 2, head: true });
        assert_eq!(
            plan_commit(&p, &head2, edge, 3.0, 0),
            Ok(CommitKind::AbsorbHead)
        );
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 3.0;
        let batch = run_commit_action(&app, &head2, CommitKind::AbsorbHead, false).unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        assert_eq!(
            clip_by_id(&app.project, 2).asset_in,
            Seconds::from_integer(0)
        );
        assert_eq!(app.project.timeline.end(), Seconds::from_integer(6));
        assert_eq!(
            app.project.timeline.video_track().unwrap().segments.len(),
            2
        );
    }

    // `[` / `]` pick the clip on the left (ends at the cut) or right
    // (starts at it). A gap side has no clip.
    #[test]
    fn armed_on_side_picks_clip_by_side() {
        let mut p = asset_project();
        p.timeline.video_track_mut().unwrap().segments.remove(1); // C1 | C2 cut
        assert_eq!(
            armed_on_side(&p, 3.0, Side::Left, 0),
            Some((Armed::Clip { id: 1, head: false }, 3.0)),
            "[ is the clip ending at the cut"
        );
        assert_eq!(
            armed_on_side(&p, 3.0, Side::Right, 0),
            Some((Armed::Clip { id: 2, head: true }, 3.0)),
            "] is the clip starting at the cut"
        );
        // Against a gap, the gap is armed at its tail from either side.
        let q = asset_project(); // [C1] [gap] [C2]
        assert_eq!(
            armed_on_side(&q, 3.0, Side::Left, 0),
            Some((Armed::Clip { id: 1, head: false }, 3.0))
        );
        assert_eq!(
            armed_on_side(&q, 3.0, Side::Right, 0),
            Some((Armed::GapHead { index: 1 }, 3.0)),
            "gap on the right arms the gap head"
        );
        assert_eq!(
            armed_on_side(&q, 5.0, Side::Right, 0),
            Some((Armed::Clip { id: 2, head: true }, 5.0))
        );
        assert_eq!(
            armed_on_side(&q, 5.0, Side::Left, 0),
            Some((Armed::GapTail { index: 1 }, 5.0)),
            "gap on the left arms the gap"
        );
        // Nothing at a non-boundary.
        assert_eq!(armed_on_side(&q, 1.0, Side::Left, 0), None);
    }

    // `<` / `>` (and `,` / `.`): head/tail of the clip under the playhead.
    #[test]
    fn clip_edge_under_finds_head_and_tail() {
        let p = asset_project(); // C1 [0..3), gap [3..5), C2 [5..7)
        assert_eq!(clip_edge_under(&p, 1.0, true, 0), Some((1, 0.0)));
        assert_eq!(clip_edge_under(&p, 1.0, false, 0), Some((1, 3.0)));
        assert_eq!(clip_edge_under(&p, 6.0, true, 0), Some((2, 5.0)));
        assert_eq!(clip_edge_under(&p, 6.0, false, 0), Some((2, 7.0)));
        assert_eq!(clip_edge_under(&p, 4.0, true, 0), None, "in a gap");
    }

    // Extend mode: flipping the armed edge of the SAME clip re-plans its
    // range against that end (no clip change).
    #[test]
    fn flip_extend_edge_replans_same_clip() {
        let p = asset_project(); // C2 [5..7) src 0..2, asset 10s
        let (s, e) = clip_span(&p, 2, 0).unwrap();
        assert_eq!((s, e), (5.0, 7.0));
        // head -> tail: range becomes [start, end + tail room]
        let tail = plan_for_armed(&p, Armed::Clip { id: 2, head: false }, e, 0).unwrap();
        assert_eq!(tail.lo, 5.0);
        assert_eq!(tail.hi, Some(15.0)); // 7 + (10 - 2)
        // tail -> head: range becomes [start - head room, end]
        let head = plan_for_armed(&p, Armed::Clip { id: 2, head: true }, s, 0).unwrap();
        assert_eq!(head.lo, 5.0); // src in is 0: no head room
        assert_eq!(head.hi, Some(7.0));
    }

    // `<` / `>` flip either end of a clip or a gap.
    #[test]
    fn flip_target_covers_clips_and_gaps() {
        let p = asset_project(); // [C1 0..3][gap 3..5][C2 5..7]
        assert_eq!(
            flip_target(&p, Armed::Clip { id: 1, head: false }, true, 0),
            Some((Armed::Clip { id: 1, head: true }, 0.0))
        );
        assert_eq!(
            flip_target(&p, Armed::Clip { id: 1, head: true }, false, 0),
            Some((Armed::Clip { id: 1, head: false }, 3.0))
        );
        assert_eq!(
            flip_target(&p, Armed::GapHead { index: 1 }, false, 0),
            Some((Armed::GapTail { index: 1 }, 5.0))
        );
        assert_eq!(
            flip_target(&p, Armed::GapTail { index: 1 }, true, 0),
            Some((Armed::GapHead { index: 1 }, 3.0))
        );
        // Leading gap head flips to its tail.
        let mut s = asset_project();
        s.timeline.video_track_mut().unwrap().segments.insert(
            0,
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
        );
        assert_eq!(
            flip_target(&s, Armed::GapHead { index: 0 }, false, 0),
            Some((Armed::GapTail { index: 0 }, 2.0))
        );
    }

    #[test]
    fn commit_at_opposite_end_deletes_armed_clip() {
        // [C1 [0..3)] [gap [3..5)] [C2 [5..7)] [C3 [7..9)]: arm the head of
        // #2 at 5.0, walk to its far end (7.0), commit. #2 is consumed, so
        // #2 goes and the follower #3 survives — not the other way round.
        let mut p = asset_project();
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(TimelineSegment::Clip(nle_core::Clip {
                id: 3,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
            }));
        let mut app = App::new(p, PathBuf::from("t.json"));
        let head2 = Armed::Clip { id: 2, head: true };
        assert_eq!(
            plan_commit(&app.project, &head2, 5.0, 7.0, 0),
            Ok(CommitKind::Delete)
        );
        app.playhead = 7.0;
        let batch = run_commit_action(&app, &head2, CommitKind::Delete, false).unwrap();
        let mut h = History::default();
        h.apply(&mut app.project, batch, 0).unwrap();
        let ids: Vec<u32> = app
            .project
            .timeline
            .video_track()
            .unwrap()
            .segments
            .iter()
            .filter_map(|s| match s {
                TimelineSegment::Clip(c) => Some(c.id),
                TimelineSegment::Gap(_) => None,
            })
            .collect();
        assert_eq!(ids, vec![1, 3], "armed #2 removed, follower #3 kept");
        assert_eq!(app.project.timeline.end(), Seconds::from_integer(9));

        // Ripple flag closes the removed clip instead of leaving a gap.
        let mut app2 = App::new(asset_project(), PathBuf::from("t.json"));
        app2.playhead = 7.0;
        let batch = run_commit_action(&app2, &head2, CommitKind::Delete, true).unwrap();
        let mut h2 = History::default();
        h2.apply(&mut app2.project, batch, 0).unwrap();
        let end = app2.project.timeline.end();
        assert!(end < Seconds::from_integer(7), "shrank: {end}");
    }

    fn seg_clip(id: u32, dur: i64) -> TimelineSegment {
        TimelineSegment::Clip(nle_core::Clip {
            id,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(dur),
            speed: Seconds::from_integer(1),
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

    #[test]
    fn timecode_formatting() {
        let fps30 = Seconds::new(30, 1);
        let fps60 = Seconds::new(60, 1);
        assert_eq!(fmt_timecode(0.0, Some(&fps30)), "00:00:00");
        assert_eq!(fmt_timecode(8.2, Some(&fps60)), "00:08:12");
        assert_eq!(fmt_timecode(62.25, Some(&fps30)), "01:02:08");
        assert_eq!(fmt_timecode(3661.0, Some(&fps30)), "1:01:01:00");
        assert_eq!(fmt_timecode(-5.0, Some(&fps30)), "00:00:00");
        assert_eq!(fmt_timecode(62.25, None), "01:02.2");
        assert_eq!(fmt_timecode(0.0, None), "00:00.0");
    }

    #[test]
    fn cut_points_boundaries() {
        let pts = cut_points(&project(), 0);
        assert_eq!(pts, vec![0.0, 3.0, 5.0, 7.0]);
        let eps = 1e-6;
        assert_eq!(next_cut(&pts, 0.5, eps), Some(3.0));
        assert_eq!(next_cut(&pts, 3.0, eps), Some(5.0));
        assert_eq!(next_cut(&pts, 7.0, eps), None);
        assert_eq!(prev_cut(&pts, 4.0, eps), Some(3.0));
        assert_eq!(prev_cut(&pts, 0.0, eps), None);
        assert_eq!(next_cut(&pts, 2.9999999, 0.0167), Some(5.0));
    }

    #[test]
    fn motions_tolerate_playhead_drift() {
        let pts = vec![0.0, 3.0, 5.0, 7.0];
        let eps = 0.5 / 30.0;
        assert_eq!(prev_cut(&pts, 3.0005, eps), Some(0.0));
        assert_eq!(prev_cut(&pts, 3.1, eps), Some(3.0));
        assert_eq!(prev_cut(&pts, 0.033, eps), Some(0.0));
        assert_eq!(next_cut(&pts, 2.9995, eps), Some(5.0));
        assert_eq!(next_clip_start(&project(), 2.9995, eps, 0), Some(5.0));
        assert_eq!(prev_clip_start(&project(), 3.0005, eps, 0), Some(0.0));
    }

    #[test]
    fn gap_navigation_skips_clip_boundaries() {
        // project(): [C1 0..3][gap 3..5][C2 5..7]
        let eps = 0.5 / 30.0;
        assert_eq!(next_gap_start(&project(), 0.0, eps, 0), Some(3.0));
        assert_eq!(next_gap_start(&project(), 2.0, eps, 0), Some(3.0));
        // Within half a frame of the gap we're "at" it, so there is no next
        // one (same drift tolerance as `j`/`k`).
        assert_eq!(next_gap_start(&project(), 2.9995, eps, 0), None);
        assert_eq!(next_gap_start(&project(), 3.0, eps, 0), None);
        assert_eq!(next_gap_start(&project(), 6.0, eps, 0), None);
        assert_eq!(prev_gap_start(&project(), 7.0, eps, 0), Some(3.0));
        assert_eq!(prev_gap_start(&project(), 4.0, eps, 0), Some(3.0));
        assert_eq!(prev_gap_start(&project(), 2.0, eps, 0), None);
    }

    #[test]
    fn n_and_shift_n_jump_to_gaps() {
        let mut app = App::new(project(), PathBuf::from("t.json"));
        app.loaded = true;
        let fake = FakeMpv::new();
        handle_key(&mut app, &fake, key('n'));
        assert_eq!(app.playhead, 3.0, "n → the gap start");
        // From the gap start there is no earlier gap; the playhead stays.
        handle_key(&mut app, &fake, key('N'));
        assert_eq!(app.playhead, 3.0);
        // From a clip past the gap, N comes back to it.
        app.playhead = 6.0;
        handle_key(&mut app, &fake, key('N'));
        assert_eq!(app.playhead, 3.0, "N → the previous gap start");
    }

    #[test]
    fn on_boundary_detects_cuts_and_edges() {
        let p = project(); // cuts at 3 and 5; edges 0 and 7
        assert!(on_boundary(&p, 0.0, 0), "timeline start");
        assert!(on_boundary(&p, 3.0, 0));
        assert!(on_boundary(&p, 5.0, 0));
        assert!(on_boundary(&p, 7.0, 0), "timeline end");
        assert!(!on_boundary(&p, 1.5, 0));
        assert!(!on_boundary(&p, 4.0, 0));
    }

    #[test]
    fn total_includes_the_sequence_tail() {
        // no tail concept in the segment model: total == sum of segments
        let app = App::new(project(), PathBuf::from("t.json"));
        assert_eq!(app.total(), 7.0);
    }

    // Frame numbering is 0-based everywhere: timecode FF is 0-based.
    #[test]
    fn timecode_frames_are_zero_based() {
        let fps = Seconds::new(30, 1);
        assert_eq!(fmt_timecode(0.0, Some(&fps)), "00:00:00");
        assert_eq!(fmt_timecode(1.0, Some(&fps)), "00:01:00");
        assert_eq!(fmt_timecode(1.0 / 30.0, Some(&fps)), "00:00:01");
    }

    #[test]
    fn snap_to_cut_is_magnetic_within_one_and_a_half_frames() {
        let mut p = project();
        p.fps = Some(Seconds::new(30, 1));
        let radius = 1.5 / 30.0;
        assert_eq!(snap_to_cut(3.0 + radius, &p, 0), 3.0);
        assert_eq!(snap_to_cut(3.0 - radius, &p, 0), 3.0);
        assert_eq!(snap_to_cut(3.5, &p, 0), 3.5);
        assert_eq!(snap_to_cut(1.0, &p, 0), 1.0);
    }

    #[test]
    fn sync_applies_grid_and_cut_snapping_while_playing() {
        let mut p = project();
        p.fps = Some(Seconds::new(30, 1));
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.sync(&PlaybackState {
            time_pos: Some(5.0 + 1.0 / 30.0),
            paused: false,
            file_loaded: true,
            ..Default::default()
        });
        assert_eq!(app.playhead, 5.0);
        app.sync(&PlaybackState {
            time_pos: Some(4.011),
            paused: false,
            file_loaded: true,
            ..Default::default()
        });
        assert_eq!(app.playhead, 4.0);
    }

    #[test]
    fn sync_ignores_mpv_positions_while_paused() {
        let mut p = project();
        p.fps = Some(Seconds::new(30, 1));
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 2.0;
        app.sync(&PlaybackState {
            time_pos: Some(5.0 + 1.0 / 30.0),
            paused: true,
            file_loaded: true,
            ..Default::default()
        });
        assert_eq!(app.playhead, 2.0);
        app.sync(&PlaybackState {
            time_pos: Some(5.0 + 1.0 / 30.0),
            paused: false,
            file_loaded: true,
            ..Default::default()
        });
        assert_eq!(app.playhead, 5.0);
    }

    // While the extend preview plays, the playhead must stay within the
    // armed range (a head can't run past the segment's end).
    #[test]
    fn sync_clamps_to_the_extend_range_while_playing() {
        let mut p = project();
        p.fps = Some(Seconds::new(30, 1));
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.mode = Mode::Extend(ExtendState {
            track: 0,
            armed: Armed::Clip { id: 1, head: true },
            edge_pos: 0.0,
            ripple: false,
            lo: -1.0,
            hi: Some(3.0),
            mat: None,
            preview_off: 0.0,
        });
        app.sync(&PlaybackState {
            time_pos: Some(9.0),
            paused: false,
            file_loaded: true,
            ..Default::default()
        });
        assert_eq!(app.playhead, 3.0, "clamped at the segment end");
        app.sync(&PlaybackState {
            time_pos: Some(-5.0),
            paused: false,
            file_loaded: true,
            ..Default::default()
        });
        assert_eq!(app.playhead, -1.0, "clamped at the range start");
    }

    #[test]
    fn frame_steps_move_one_frame_and_clamp() {
        let mut p = project();
        p.fps = Some(Seconds::new(30, 1));
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.step_playhead(1);
        assert_eq!(app.playhead, 1.0 / 30.0);
        app.step_playhead(-1);
        assert_eq!(app.playhead, 0.0);
        app.step_playhead(-1);
        assert_eq!(app.playhead, 0.0, "clamped at the start");
        app.playhead = 1000.0;
        app.step_playhead(1);
        assert_eq!(app.playhead, 209.0 / 30.0, "clamped at the last frame");
    }

    #[test]
    fn label_truncation_and_padding() {
        assert_eq!(truncate_label("red.mp4", 10), " red.mp4  ");
        assert_eq!(truncate_label("very-long-name.mp4", 8), "very-lon");
        assert_eq!(truncate_label("ab", 2), "ab");
    }

    // Render test: extend mode overlays the project timeline — the track
    // row is the clips (not a source strip) with the playhead notched, and
    // the marker row carries the dim material extent, the yellow workable
    // span, and the playhead.
    #[test]
    fn extend_overlays_the_timeline() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let p = asset_project(); // C1 [0..3) src1..4, gap [3..5), C2 [5..7) src0..2
        let plan = plan_for_armed(&p, Armed::Clip { id: 1, head: false }, 3.0, 0).unwrap();
        assert_eq!((plan.lo, plan.hi), (0.0, Some(9.0)));
        assert_eq!(plan.mat, Some((-1.0, 9.0)));

        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 1.0; // roamed off the armed edge (3.0)
        app.mode = Mode::Extend(ExtendState {
            track: 0,
            armed: plan.armed,
            edge_pos: plan.edge_pos,
            ripple: false,
            lo: plan.lo,
            hi: plan.hi,
            mat: plan.mat,
            preview_off: 2.0,
        });

        let mut term = Terminal::new(TestBackend::new(60, 4)).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 4,
        };
        term.draw(|f| render_timeline(f, area, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let sym = |x: u16, y: u16| -> (String, Color, Color) {
            let c = &buf[(x, y)];
            (c.symbol().to_string(), c.fg, c.bg)
        };

        // Track row is the project timeline: cyan clip #1 (label) then the
        // dim gap, with the roamed playhead notched in yellow.
        let (_, _, b) = sym(3, 1);
        assert_eq!(b, Color::Cyan, "clip cells");
        let (_, _, b) = sym(31, 1);
        assert_eq!(b, Color::DarkGray, "gap cells");
        let (s, f, _) = sym(9, 1);
        assert_eq!((s.as_str(), f), ("▏", Color::Yellow), "roaming notch");

        // Marker row: dim material extent under a yellow workable span
        // (the span [0, 9] covers it, so yellow wins); playhead notch
        // inside. The row rescales to fit the span past the timeline end.
        let (s, f, _) = sym(1, 2);
        assert_eq!((s.as_str(), f), ("─", Color::Yellow), "workable span");
        let (s, f, _) = sym(9, 2);
        assert_eq!((s.as_str(), f), ("▏", Color::Yellow), "playhead");
        let (s, f, _) = sym(58, 2);
        assert_eq!(
            (s.as_str(), f),
            ("─", Color::Yellow),
            "span reaches the far end"
        );
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

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// A project path under the system temp dir, so undo/redo's save doesn't
    /// write into the repo.
    fn temp_project_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nle-tui-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("t.json")
    }

    #[test]
    fn switching_track_keeps_the_composite_monitor() {
        let mut p = asset_project();
        p.timeline.add_track(TrackKind::Audio);
        let mut app = App::new(p, temp_project_path("track-switch"));
        let fake = FakeMpv::new();
        set_active_track(&mut app, 1);
        assert_eq!(app.active(), 1);
        assert!(app.message.contains("a1"), "{}", app.message);
        // The monitor plays the composite of all video tracks, so switching
        // the edit target does not disturb it.
        assert!(
            fake.calls().is_empty(),
            "no monitor reload: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn speed_key_opens_the_rate_prompt() {
        let mut app = App::new(asset_project(), temp_project_path("speed-key"));
        let fake = FakeMpv::new();
        handle_key(&mut app, &fake, key('R'));
        assert!(matches!(app.mode, Mode::Command(ref s) if s == "speed "));
    }

    #[test]
    fn speed_command_retimes_and_ripples() {
        let mut app = App::new(asset_project(), temp_project_path("speed-cmd"));
        app.loaded = true;
        app.playhead = 0.5; // inside clip #1 [0..3)
        let fake = FakeMpv::new();
        handle_key(&mut app, &fake, key('R')); // -> :speed
        for c in "0.5".chars() {
            handle_key(&mut app, &fake, key(c));
        }
        handle_key(
            &mut app,
            &fake,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        // clip #1 at 0.5x: its 3s of source now occupy 6s of timeline, so the
        // gap and clip #2 slide right (a speed change always ripples).
        let track = app.project.timeline.video_track().unwrap();
        let TimelineSegment::Clip(c) = &track.segments[0] else {
            panic!("expected clip")
        };
        assert_eq!(c.speed, Seconds::new(1, 2));
        assert_eq!(track.segments[0].duration(), Seconds::from_integer(6));
        assert_eq!(track.start_of(1), Seconds::from_integer(6));
    }

    #[test]
    fn purge_command_snaps_a_drifted_clip() {
        let mut app = App::new(asset_project(), temp_project_path("purge"));
        // Give clip #1 a fractional (drifted) duration: 60.4 frames at 30fps.
        if let TimelineSegment::Clip(c) =
            &mut app.project.timeline.video_track_mut().unwrap().segments[0]
        {
            c.asset_out = c.asset_in + Seconds::new(604, 300);
        }
        let msg = purge_command(&mut app);
        assert!(msg.contains("purged"), "{msg}");
        let TimelineSegment::Clip(c) = &app.project.timeline.video_track().unwrap().segments[0]
        else {
            panic!("expected clip")
        };
        assert_eq!(
            c.asset_out - c.asset_in,
            Seconds::from_integer(2),
            "snapped to 60 frames"
        );
    }

    #[test]
    fn transport_records_monitor_calls() {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.loaded = true;
        let fake = FakeMpv::new();

        handle_key(&mut app, &fake, key('l'));
        assert_eq!(app.playhead, 1.0);
        assert_eq!(fake.calls(), vec![Call::SeekRel(1.0)]);

        // A count multiplies the seek.
        handle_key(&mut app, &fake, key('3'));
        handle_key(&mut app, &fake, key('l'));
        assert_eq!(app.playhead, 4.0);
        assert_eq!(fake.calls().last(), Some(&Call::SeekRel(3.0)));

        handle_key(&mut app, &fake, key('f'));
        let want = 4.0 + 1.0 / 30.0;
        assert!((app.playhead - want).abs() < 1e-9);
        // f/F move the monitor to the model frame on the project grid (an
        // absolute seek), never mpv's source-rate `frame_step`.
        match fake.calls().last() {
            Some(Call::SeekAbs(t)) => assert!((t - (want + 0.004)).abs() < 1e-9),
            other => panic!("f must seek the monitor to the model frame, got {other:?}"),
        }
    }

    // The pool's `gg`: a lone `g` only arms the sequence; the second seeks.
    #[test]
    fn pool_gg_waits_for_the_second_key() {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.loaded = true;
        open_pool_mode(&mut app, 0);
        let fake = FakeMpv::new();

        handle_key(&mut app, &fake, key('g'));
        assert!(fake.calls().is_empty(), "a lone g must not seek");
        handle_key(&mut app, &fake, key('g'));
        assert_eq!(fake.calls(), vec![Call::SeekAbs(0.0)]);
    }

    // Pool space: restart at EOF, otherwise toggle.
    #[test]
    fn pool_space_restarts_at_eof_else_toggles() {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.loaded = true;
        open_pool_mode(&mut app, 0);

        let fake = FakeMpv::new();
        app.eof = false;
        handle_key(&mut app, &fake, key(' '));
        assert_eq!(fake.calls(), vec![Call::TogglePause]);

        let fake = FakeMpv::new();
        app.eof = true;
        handle_key(&mut app, &fake, key(' '));
        assert_eq!(
            fake.calls(),
            vec![Call::SeekAbs(0.0), Call::SetPause(false)]
        );
        assert_eq!(pool_pos(&app), 0.0);
    }

    // Undo while the pool is open must keep the pool preview (not kick the
    // monitor to the timeline EDL).
    #[test]
    fn pool_undo_keeps_the_pool_preview() {
        let mut app = App::new(asset_project(), temp_project_path("pool-undo"));
        app.playhead = 1.0;
        let batch = edit::trim_in_ripple(&app.project, app.playhead).unwrap();
        let frame = frame_of(app.project.fps.as_ref(), app.playhead);
        app.history.apply(&mut app.project, batch, frame).unwrap();
        app.loaded = true;
        open_pool_mode(&mut app, 0);

        let fake = FakeMpv::new();
        handle_key(&mut app, &fake, key('u'));
        assert!(!app.history.can_undo(), "the trim was undone");
        assert!(is_pool(&app), "still in the pool");
        match fake.calls().last() {
            Some(Call::Load(url, _)) => assert!(url.ends_with("red.mp4"), "{url}"),
            other => panic!("expected a pool reload, got {other:?}"),
        }
    }

    // Removing a pooled asset is recorded and undoable, and the preview
    // follows the new selection.
    #[test]
    fn pool_remove_is_undoable() {
        let mut p = asset_project();
        let (spare, _) = p.add_asset("/media/spare.mp4".into(), p.asset(1).unwrap().probe.clone());
        let mut app = App::new(p, temp_project_path("pool-remove"));
        app.loaded = true;
        open_pool_mode(&mut app, 1); // the spare row
        let fake = FakeMpv::new();

        handle_key(&mut app, &fake, key('d'));
        assert_eq!(app.project.media.len(), 1, "spare removed");
        assert!(app.history.can_undo());
        assert!(is_pool(&app), "still in the pool");
        assert!(
            matches!(fake.calls().last(), Some(Call::Load(url, _)) if url.ends_with("red.mp4")),
            "preview moved to the remaining asset: {:?}",
            fake.calls()
        );

        handle_key(&mut app, &fake, key('u'));
        assert_eq!(app.project.media.len(), 2, "undo restored the spare");
        assert!(app.project.asset(spare).is_some());
    }

    // `t`/`T` change the insert target while the pool is open, without
    // disturbing the asset preview on the monitor.
    #[test]
    fn pool_t_switches_the_insert_track() {
        let mut p = asset_project();
        p.timeline.add_track(TrackKind::Audio); // track 1
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.loaded = true;
        open_pool_mode(&mut app, 0);
        let fake = FakeMpv::new();

        handle_key(&mut app, &fake, key('t'));
        assert_eq!(app.active(), 1, "t cycles to the next track");
        assert!(app.message.contains("a1"), "{}", app.message);
        assert!(
            fake.calls().is_empty(),
            "the asset preview is untouched: {:?}",
            fake.calls()
        );

        handle_key(&mut app, &fake, key('T'));
        assert_eq!(app.active(), 0, "T cycles back");

        // `<2>t` jumps to track 2 (1-based).
        handle_key(&mut app, &fake, key('2'));
        handle_key(&mut app, &fake, key('t'));
        assert_eq!(app.active(), 1);
        assert!(is_pool(&app), "still in the pool");
    }

    // Reopening the pool returns to the row last selected.
    #[test]
    fn pool_reopen_returns_to_the_last_row() {
        let mut p = asset_project();
        p.add_asset("/media/blue.mp4".into(), p.asset(1).unwrap().probe.clone());
        // Drop the gap so close_pool's composite EDL needs no black file (ffmpeg).
        p.timeline.video_track_mut().unwrap().segments.remove(1);
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.loaded = true;
        let fake = FakeMpv::new();

        handle_key(&mut app, &fake, key('m')); // open
        handle_key(&mut app, &fake, key('j')); // second row
        assert_eq!(pool_sel(&app), Some(1));

        handle_key(&mut app, &fake, key('m')); // close (captures row 1)
        assert!(!is_pool(&app));
        handle_key(&mut app, &fake, key('m')); // reopen
        assert_eq!(pool_sel(&app), Some(1), "reopened on the last row");
    }

    // `i` in the pool opens the import path prompt (command line pre-filled).
    #[test]
    fn pool_i_opens_the_import_prompt() {
        let mut app = App::new(asset_project(), PathBuf::from("t.json"));
        app.loaded = true;
        let fake = FakeMpv::new();
        handle_key(&mut app, &fake, key('m')); // open the pool
        handle_key(&mut app, &fake, key('i'));
        assert_eq!(app.mode, Mode::Command("import ".to_string()));
    }

    // `:help` names the available commands.
    #[test]
    fn help_command_lists_commands() {
        let mut app = App::new(asset_project(), temp_project_path("help"));
        let fake = FakeMpv::new();
        handle_key(&mut app, &fake, key(':'));
        for c in "help".chars() {
            handle_key(&mut app, &fake, key(c));
        }
        handle_key(
            &mut app,
            &fake,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(app.message.contains(":import"), "{}", app.message);
        assert!(app.message.contains(":track"), "{}", app.message);
    }
}
