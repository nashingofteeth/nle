//! The terminal dashboard: timeline visualization, vim keys, live playhead.
//!
//! Input model: a small vim. Counts work (`3l` = seek 3s), `gg`/`G` jump to
//! ends, `j`/`k` walk cut points, `w`/`b` walk clip starts, `:` opens a
//! command line. Playback keys are sent to mpv; state flows back through
//! the reader thread and is rendered on the next tick (~20fps redraw).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use nle_core::edit;
use nle_core::timeline::Segment as TimelineSegment;
use nle_core::{Batch, Clip, EditError, History, Project, Seconds};

use crate::mpv::{MpvPlayer, PlaybackState};
use crate::preview::{self, Segment};

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
pub fn snap_to_cut(t: f64, project: &Project) -> f64 {
    let Some(fps) = project.fps.as_ref() else {
        return t;
    };
    let radius = 1.5 / secs(fps);
    let mut best = t;
    let mut best_d = radius;
    for c in cut_points(project) {
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
fn gap_under(project: &Project, t: f64) -> Option<(Seconds, Seconds)> {
    let track = project.timeline.video_track()?;
    if clip_under(project, t).is_some() {
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
    while last + 1 < track.segments.len() && matches!(track.segments[last + 1], TimelineSegment::Gap(_)) {
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
pub fn cut_points(project: &Project) -> Vec<f64> {
    let Some(track) = project.timeline.video_track() else {
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

pub fn next_clip_start(project: &Project, t: f64, eps: f64) -> Option<f64> {
    let track = project.timeline.video_track()?;
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

pub fn prev_clip_start(project: &Project, t: f64, eps: f64) -> Option<f64> {
    let track = project.timeline.video_track()?;
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

// ------------------------------------------------------------ app state

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Command(String),
    Extend(ExtendState),
}

/// Which boundary is armed in extend mode: a clip edge, or a virtual
/// timeline edge where a gap meets the sequence boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Armed {
    /// Head (`head == true`) or tail of the clip with this id.
    Clip { id: u32, head: bool },
    /// Timeline end with a trailing gap.
    TailGap,
    /// Timeline start (0) with a leading gap.
    HeadGap,
}

/// Extend-mode session state. All bounds are timeline seconds; `hi` is
/// `None` for unbounded ranges (gap extension past the end). `mat` is
/// the asset-material span for display (clip-armed only — gaps have no
/// material). `preview_off` translates seeks: `mpv = playhead - off`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtendState {
    pub armed: Armed,
    pub edge_pos: f64,
    pub ripple: bool,
    pub lo: f64,
    pub hi: Option<f64>,
    pub mat: Option<(f64, f64)>,
    pub preview_off: f64,
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
    pub clipboard: Option<Clip>,
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
        }
    }

    pub fn set_message(&mut self, msg: &str) {
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
        if !st.paused && st.file_loaded
            && let Some(t) = st.time_pos {
                let t = match &self.mode {
                    Mode::Extend(st) => t + st.preview_off,
                    _ => t,
                };
                let snapped = snap_to_frame(t, self.project.fps.as_ref());
                self.playhead = snap_to_cut(snapped, &self.project);
            }
        self.paused = st.paused;
        self.loaded = st.file_loaded;
        self.dead = st.dead;
        self.eof = st.eof;
        if let Some(e) = &st.last_error {
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

    fn seek_rel(&mut self, player: &MpvPlayer, delta: f64) {
        if self.guard_loaded() {
            player.seek_relative(delta);
            // Model-authoritative while paused: h/l moves the playhead by
            // the same arithmetic the display uses.
            self.playhead = (self.playhead + delta).clamp(0.0, self.total());
        }
    }

    fn seek_to(&mut self, player: &MpvPlayer, t: f64) {
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
        let Some(fps) = self.project.fps.as_ref() else { return };
        let f = secs(fps);
        let last = ((self.total() * f).ceil() as i64 - 1).max(0);
        let idx = ((self.playhead * f).round() as i64 + dir).clamp(0, last);
        self.playhead = idx as f64 / f;
    }
}

// ------------------------------------------------------------ event loop

/// Sidecar path for the undo history: `nle.json` → `nle.history.json`.
fn history_path(project_path: &Path) -> PathBuf {
    project_path.with_extension("history.json")
}

pub fn run(project: Project, path: PathBuf, player: MpvPlayer) -> anyhow::Result<()> {
    let mut terminal = ratatui::init(); // raw mode + alternate screen + panic hook
    let history = History::load(&history_path(&path)).unwrap_or_default();
    let mut app = App::new(project, path);
    app.history = history;
    // Media title (MPRIS "Now playing") follows the clip under the playhead.
    let mut last_media_title: Option<String> = None;

    loop {
        let st = player.state();
        app.sync(&st);

        if app.reload_requested {
            app.reload_requested = false;
            match reload_project(&mut app, &player) {
                Ok(()) => app.set_message("timeline reloaded"),
                Err(e) => app.set_message(&format!("reload failed: {e}")),
            }
        }

        // Keep the desktop's "Now playing" in sync with the clip playing.
        if st.file_loaded {
            let current = clip_under(&app.project, app.playhead)
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
                && key.kind == KeyEventKind::Press {
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
fn reload_project(app: &mut App, player: &MpvPlayer) -> anyhow::Result<()> {
    let project = Project::load(&app.path)?;
    let edl = preview::build_edl(&project)?;
    app.project = project;
    app.history = History::load(&history_path(&app.path)).unwrap_or_default();
    player.load(&edl, Some(app.playhead));
    Ok(())
}

// ------------------------------------------------------------ keys

pub fn handle_key(app: &mut App, player: &MpvPlayer, key: KeyEvent) {
    if matches!(app.mode, Mode::Command(_)) {
        let buf = match std::mem::replace(&mut app.mode, Mode::Normal) {
            Mode::Command(buf) => buf,
            _ => unreachable!("checked above"),
        };
        command_key(app, buf, key);
    } else if matches!(app.mode, Mode::Extend(_)) {
        extend_key(app, player, key);
    } else {
        normal_key(app, player, key);
    }
}

fn command_key(app: &mut App, buf: String, key: KeyEvent) {
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

fn normal_key(app: &mut App, player: &MpvPlayer, key: KeyEvent) {
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
                    player.frame_step();
                }
                app.step_playhead(1);
            }
        }
        KeyCode::Char('F') => {
            if app.guard_loaded() {
                for _ in 0..mult {
                    player.frame_back_step();
                }
                app.step_playhead(-1);
            }
        }
        KeyCode::Char('j') => match next_cut(&cut_points(&app.project), app.playhead, eps) {
            Some(t) => app.seek_to(player, t),
            None => app.set_message("no cut after the playhead"),
        },
        KeyCode::Char('k') => match prev_cut(&cut_points(&app.project), app.playhead, eps) {
            Some(t) => app.seek_to(player, t),
            None => app.set_message("no cut before the playhead"),
        },
        KeyCode::Char('w') => match next_clip_start(&app.project, app.playhead, eps) {
            Some(t) => app.seek_to(player, t),
            None => app.set_message("no clip after the playhead"),
        },
        KeyCode::Char('b') => match prev_clip_start(&app.project, app.playhead, eps) {
            Some(t) => app.seek_to(player, t),
            None => app.set_message("no clip before the playhead"),
        },
        KeyCode::Char('g') => app.pending = Pending::G,
        KeyCode::Char('d') => app.pending = Pending::D,
        KeyCode::Char('y') => app.pending = Pending::Y,
        // ---- editing: uniform playhead-based ops (counts don't multiply) ----
        KeyCode::Char('i') => { let _ = run_edit(app, player, edit::trim_in(&app.project, app.playhead)); }
        KeyCode::Char('I') => { let _ = run_edit(app, player, edit::trim_in_ripple(&app.project, app.playhead)); }
        KeyCode::Char('o') => { let _ = run_edit(app, player, edit::trim_out(&app.project, app.playhead)); }
        KeyCode::Char('O') => { let _ = run_edit(app, player, edit::trim_out_ripple(&app.project, app.playhead)); }
        KeyCode::Char('s') => { let _ = run_edit(app, player, edit::split(&app.project, app.playhead)); }
        KeyCode::Char('D') => {
            edit_yank(app); // D yanks too
            run_edit(app, player, edit::delete_ripple(&app.project, app.playhead));
        }
        KeyCode::Char('p') => {
            let Some(clipboard) = app.clipboard.clone() else {
                app.set_message("clipboard empty — yy first");
                return;
            };
            run_edit(app, player, edit::paste(&app.project, &clipboard, app.playhead));
        }
        KeyCode::Char('P') => {
            let Some(clipboard) = app.clipboard.clone() else {
                app.set_message("clipboard empty — yy first");
                return;
            };
            run_edit(app, player, edit::paste_ripple(&app.project, &clipboard, app.playhead));
        }
        KeyCode::Char('u') => match app.history.undo(&mut app.project) {
            Ok(desc) => {
                app.set_message(&format!("undo: {desc}"));
                persist_and_rebuild(app, player);
            }
            Err(e) => app.set_message(&e.to_string()),
        },
        KeyCode::Char('e') => enter_extend(app, player, false),
        KeyCode::Char('E') => enter_extend(app, player, true),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match app.history.redo(&mut app.project) {
                Ok(desc) => {
                    app.set_message(&format!("redo: {desc}"));
                    persist_and_rebuild(app, player);
                }
                Err(e) => app.set_message(&e.to_string()),
            }
        }
        // ---- navigation/transport (unchanged) ----
        KeyCode::Char('G') | KeyCode::Char('$') => app.seek_to(player, app.total()),
        KeyCode::Char('r') => app.reload_requested = true,
        KeyCode::Char('q') => app.quit = true,
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

/// What committing at the current playhead means. Maps 1:1 to edit ops
/// in `run_commit` below; kept separate so the routing table is unit
/// testable without mpv.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CommitKind {
    TrimHead,
    TrimTail,
    GrowHeadIntoGap,
    GrowTailIntoGap,
    GrowTailPastEnd,
    ShortenTail,
    ShortenHeadGap,
    GrowTailGap,
    Delete,
    Noop,
}

/// Resolve the armed boundary at timeline position `t`: clip starts win
/// (head-of-right at shared cuts, consistent with `clip_under`), then
/// clip ends, then virtual timeline edges with gaps. Anything else is
/// not a boundary.
fn resolve_armed(project: &Project, t: f64) -> Result<(Armed, f64), String> {
    let track = project
        .timeline
        .video_track()
        .ok_or_else(|| "empty timeline — add clips first".to_string())?;
    if track.segments.is_empty() {
        return Err("empty timeline — add clips first".to_string());
    }
    let eps = 1e-6;
    // Pass 1: clip starts (shared cuts arm the right clip's head).
    let mut acc = 0.0;
    for seg in &track.segments {
        if let TimelineSegment::Clip(c) = seg
            && (acc - t).abs() < eps {
                return Ok((Armed::Clip { id: c.id, head: true }, acc));
            }
        acc += secs(&seg.duration());
    }
    // Pass 2: clip ends.
    let mut acc = 0.0;
    for seg in &track.segments {
        acc += secs(&seg.duration());
        if let TimelineSegment::Clip(c) = seg
            && (acc - t).abs() < eps {
                return Ok((Armed::Clip { id: c.id, head: false }, acc));
            }
    }
    // Pass 3: virtual timeline edges with gaps.
    let total = acc;
    if t.abs() < eps
        && matches!(track.segments.first(), Some(TimelineSegment::Gap(_))) {
            return Ok((Armed::HeadGap, 0.0));
        }
    if (total - t).abs() < eps
        && matches!(track.segments.last(), Some(TimelineSegment::Gap(_)))
    {
        return Ok((Armed::TailGap, total));
    }
    Err("extend starts at a boundary — j/k to a cut".to_string())
}

/// The timeline span of a clip by id.
fn clip_span(project: &Project, id: u32) -> Option<(f64, f64)> {
    let track = project.timeline.video_track()?;
    let mut acc = 0.0;
    for seg in &track.segments {
        let dur = secs(&seg.duration());
        if let TimelineSegment::Clip(c) = seg
            && c.id == id {
                return Some((acc, acc + dur));
            }
        acc += dur;
    }
    None
}

/// Plan an extend-mode session: armed boundary, committable range,
/// material span for display, and preview parameters. Pure — the caller
/// realizes it with mpv IPC. Positions below are timeline seconds;
/// `hi == None` means unbounded (gap extension past the end).
struct ExtendPlan {
    armed: Armed,
    edge_pos: f64,
    lo: f64,
    hi: Option<f64>,
    mat: Option<(f64, f64)>,
    preview: ExtendPreviewSpec,
}

enum ExtendPreviewSpec {
    /// Asset path + asset-relative start/length + timeline origin.
    Clip { path: std::path::PathBuf, src_start: Seconds, length: Seconds, origin: f64 },
    Black { origin: f64 },
}

fn plan_extend(project: &Project, playhead: f64, ripple: bool) -> Result<ExtendPlan, String> {
    let _ = ripple;
    let (armed, edge) = resolve_armed(project, playhead)?;
    let track = project.timeline.video_track().ok_or_else(|| "empty timeline".to_string())?;
    let total = secs(&project.timeline.end());

    match armed {
        Armed::Clip { id, head } => {
            let (s, e) = clip_span(project, id).ok_or_else(|| "clip vanished".to_string())?;
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
            // Unknown asset duration: no extension room, trim/delete only.
            let mat_avail_out = asset.duration().map(|d| (secs(&d) - aout).max(0.0)).unwrap_or(0.0);
            let mat_avail_in = ain;
            // Neighbor bounds: the adjacent gap's far end, or the edge
            // itself when a clip (or nothing) blocks the way.
            let si = track
                .segments
                .iter()
                .position(|seg| matches!(seg, TimelineSegment::Clip(c) if c.id == id))
                .ok_or_else(|| "clip vanished".to_string())?;
            let (lo, hi, src_start, length, mat) = if head {
                let lo = if si > 0 {
                    match &track.segments[si - 1] {
                        // Adjacent gap: reach back over it, but never past
                        // the available head material (or time zero).
                        TimelineSegment::Gap(g) => {
                            let gs = s - secs(&g.duration);
                            (s - mat_avail_in.min(s - gs)).max(0.0)
                        }
                        _ => s,
                    }
                } else {
                    // First segment: nothing exists left of the edge.
                    s.max(0.0)
                };
                let src_start = Seconds::new(((ain - (s - lo)) * 1e6).round() as i64, 1_000_000);
                let mat_hi = asset.duration().map(|d| s + (secs(&d) - ain)).unwrap_or(e);
                (lo, Some(e), src_start, e - lo, Some((s - ain, mat_hi)))
            } else {
                let next_gap_end = if si + 1 < track.segments.len() {
                    match &track.segments[si + 1] {
                        TimelineSegment::Gap(g) => Some(e + secs(&g.duration)),
                        _ => None,
                    }
                } else {
                    None
                };
                let hi = match next_gap_end {
                    Some(ge) => (e + mat_avail_out).min(ge),
                    None if si + 1 >= track.segments.len() => e + mat_avail_out,
                    None => e,
                };
                let src_start = Seconds::new((ain * 1e6).round() as i64, 1_000_000);
                let mat_lo = s - ain;
                let mat_hi = asset.duration().map(|d| e + (secs(&d) - aout)).unwrap_or(e);
                (s, Some(hi), src_start, hi - s, Some((mat_lo, mat_hi)))
            };
            Ok(ExtendPlan {
                armed,
                edge_pos: edge,
                lo,
                hi,
                mat,
                preview: ExtendPreviewSpec::Clip {
                    path: asset.path.clone(),
                    src_start,
                    length: Seconds::new(((length) * 1e6).round() as i64, 1_000_000),
                    origin: lo,
                },
            })
        }
        Armed::TailGap => {
            let gap_start = total - secs(&track.segments.last().map(|s| s.duration()).unwrap_or(Seconds::from_integer(0)));
            Ok(ExtendPlan {
                armed,
                edge_pos: total,
                lo: gap_start,
                hi: None,
                mat: None,
                preview: ExtendPreviewSpec::Black { origin: gap_start },
            })
        }
        Armed::HeadGap => {
            let gap_end = secs(&track.segments.first().map(|s| s.duration()).unwrap_or(Seconds::from_integer(0)));
            Ok(ExtendPlan {
                armed,
                edge_pos: 0.0,
                lo: 0.0,
                hi: Some(gap_end),
                mat: None,
                preview: ExtendPreviewSpec::Black { origin: 0.0 },
            })
        }
    }
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// Route a commit from the armed edge and playhead. Positions use the
/// live track layout (unchanged while the mode is active).
fn plan_commit(project: &Project, armed: &Armed, edge_pos: f64, ph: f64) -> Result<CommitKind, String> {
    if approx(ph, edge_pos) {
        return Ok(CommitKind::Noop);
    }
    match armed {
        Armed::Clip { id, head } => {
            let (s, e) = clip_span(project, *id).ok_or_else(|| "clip vanished".to_string())?;
            if *head {
                if ph > edge_pos && ph < e - 1e-9 {
                    Ok(CommitKind::TrimHead)
                } else if ph < edge_pos {
                    // In the preceding gap (clamp guarantees adjacency):
                    // extend the head back. E ripples (no neighbor touch).
                    Ok(CommitKind::GrowHeadIntoGap)
                } else {
                    Ok(CommitKind::Delete)
                }
            } else if ph < edge_pos && ph > s + 1e-9 {
                Ok(CommitKind::TrimTail)
            } else if ph > edge_pos {
                // Outward: adjacent gap, or past the timeline end.
                let track = project.timeline.video_track().ok_or_else(|| "clip vanished".to_string())?;
                let si = track
                    .segments
                    .iter()
                    .position(|seg| matches!(seg, TimelineSegment::Clip(c) if c.id == *id))
                    .ok_or_else(|| "clip vanished".to_string())?;
                if si + 1 < track.segments.len()
                    && matches!(track.segments[si + 1], TimelineSegment::Gap(_))
                {
                    Ok(CommitKind::GrowTailIntoGap)
                } else if si + 1 >= track.segments.len() {
                    Ok(CommitKind::GrowTailPastEnd)
                } else {
                    // Next is a clip: clamp keeps this unreachable (hi == edge).
                    Ok(CommitKind::Noop)
                }
            } else {
                Ok(CommitKind::Delete)
            }
        }
        Armed::TailGap => {
            if ph > edge_pos {
                Ok(CommitKind::GrowTailGap)
            } else {
                Ok(CommitKind::ShortenTail)
            }
        }
        Armed::HeadGap => Ok(CommitKind::ShortenHeadGap),
    }
}

// ------------------------------------------------------------ edit plumbing

/// The clip the playhead sits inside, if any.
fn clip_under(project: &Project, t: f64) -> Option<&Clip> {
    let track = project.timeline.video_track()?;
    track.clip_at(grid_time(project, t))
}

/// The playhead's position on the project's frame grid, as Seconds.
fn grid_time(project: &Project, t: f64) -> Seconds {
    match project.fps.as_ref() {
        Some(fps) => {
            let n = (t * secs(fps)).round() as i64;
            Seconds::new(n, 1) / *fps
        }
        None => Seconds::from_integer(t.round() as i64),
    }
}

/// Apply an edit batch, then save + rebuild the preview so the monitor
/// reflects the new timeline at the current playhead.
fn run_edit(app: &mut App, player: &MpvPlayer, result: Result<Batch, EditError>) -> bool {
    let batch = match result {
        Ok(b) => b,
        Err(e) => {
            app.set_message(&e.to_string());
            return false;
        }
    };
    let desc = batch.desc.clone();
    match app.history.apply(&mut app.project, batch) {
        Ok(()) => {
            app.set_message(&desc);
            persist_and_rebuild(app, player);
            true
        }
        Err(e) => {
            app.set_message(&e.to_string());
            false
        }
    }
}

/// Atomic save + preview rebuild. Called after every successful edit
/// (and undo/redo) — no manual save state to get wrong. The undo history
/// sidecar is written alongside the project, and the preview reloads
/// with the playhead position baked into the load (no race).
fn persist_and_rebuild(app: &mut App, player: &MpvPlayer) {
    if let Err(e) = app.project.save(&app.path) {
        app.set_message(&format!("save failed: {e}"));
        return;
    }
    if let Err(e) = app.history.save(&history_path(&app.path)) {
        app.set_message(&format!("history save failed: {e}"));
    }
    match preview::build_edl(&app.project) {
        Ok(edl) => player.load(&edl, Some(app.playhead)),
        Err(e) => app.set_message(&format!("preview rebuild failed: {e}")),
    }
}

fn edit_delete(app: &mut App, player: &MpvPlayer) {
    edit_yank(app); // dd yanks first (vim: dd fills the register)
    run_edit(app, player, edit::delete_under(&app.project, app.playhead));
}

fn edit_yank(app: &mut App) {
    match clip_under(&app.project, app.playhead) {
        Some(c) => {
            let id = c.id;
            app.clipboard = Some(c.clone());
            app.set_message(&format!("yanked clip #{id}"));
        }
        None => app.set_message("nothing to yank — playhead is in a gap"),
    }
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

fn armed_desc(armed: &Armed) -> String {
    match armed {
        Armed::Clip { id, head } => format!("{} of clip #{id}", if *head { "head" } else { "tail" }),
        Armed::TailGap => "timeline end".to_string(),
        Armed::HeadGap => "timeline start".to_string(),
    }
}

/// Enter extend mode: resolve the boundary, build the material preview,
/// and hand mpv over to it. Stays in Normal on any failure.
fn enter_extend(app: &mut App, player: &MpvPlayer, ripple: bool) {
    let plan = match plan_extend(&app.project, app.playhead, ripple) {
        Err(msg) => {
            app.set_message(&msg);
            return;
        }
        Ok(p) => p,
    };
    let (url, origin) = match &plan.preview {
        ExtendPreviewSpec::Clip { path, src_start, length, origin } => {
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
    app.set_message(&format!("extend {}{flag} — navigate, e to commit", armed_desc(&plan.armed)));
    app.mode = Mode::Extend(ExtendState {
        armed: plan.armed,
        edge_pos: plan.edge_pos,
        ripple,
        lo: plan.lo,
        hi: plan.hi,
        mat: plan.mat,
        preview_off: origin,
    });
}

/// Leave extend mode: project timeline back on the monitor, playhead at
/// `pos`.
fn exit_extend(app: &mut App, player: &MpvPlayer, pos: f64, msg: &str) {
    match preview::build_edl(&app.project) {
        Ok(edl) => player.load(&edl, Some(pos.max(0.0))),
        Err(e) => app.set_message(&format!("preview rebuild failed: {e}")),
    }
    app.playhead = pos;
    app.mode = Mode::Normal;
    app.set_message(msg);
}

/// Map a commit kind to its edit op. The playhead does the routing: it
/// sits wherever navigation left it, and each op reads positionally.
fn run_commit_action(app: &App, armed: &Armed, kind: CommitKind, ripple: bool) -> Result<Batch, EditError> {
    let (project, ph) = (&app.project, app.playhead);
    let (id, head) = match armed {
        Armed::Clip { id, head } => (*id, *head),
        Armed::TailGap | Armed::HeadGap => (0, false),
    };
    match kind {
        CommitKind::TrimHead => {
            if ripple {
                edit::trim_in_ripple(project, ph)
            } else {
                edit::trim_in(project, ph)
            }
        }
        CommitKind::TrimTail => {
            if ripple {
                edit::trim_out_ripple(project, ph)
            } else {
                edit::trim_out(project, ph)
            }
        }
        CommitKind::GrowHeadIntoGap => {
            // Preserve: the existing gap op extends the head while the
            // neighboring gap shrinks (length unchanged). Ripple: extend
            // without touching the gap (followers shift).
            if ripple {
                edit::extend_edge(project, id, true, ph)
            } else {
                edit::trim_out(project, ph)
            }
        }
        CommitKind::GrowTailIntoGap => {
            if ripple {
                edit::extend_edge(project, id, false, ph)
            } else {
                edit::trim_in(project, ph)
            }
        }
        CommitKind::GrowTailPastEnd => {
            edit::extend_edge(project, id, head, ph)
        }
        CommitKind::ShortenTail | CommitKind::ShortenHeadGap => {
            if matches!(armed, Armed::TailGap) {
                edit::trim_out(project, ph)
            } else {
                edit::trim_in(project, ph)
            }
        }
        CommitKind::GrowTailGap => edit::extend_gap_end(project, ph),
        CommitKind::Delete => {
            if ripple {
                edit::delete_ripple(project, ph)
            } else {
                edit::delete_under(project, ph)
            }
        }
        CommitKind::Noop => Err(EditError::NothingToTrim(format!("edge already at {}", ph))),
    }
}

/// Commit: route, apply as one undo step, and leave the mode. Failures
/// keep the mode (and its preview) so the position isn't lost.
fn commit_extend(app: &mut App, player: &MpvPlayer) {
    let Some(st) = extend_state(app) else { return };
    let kind = match plan_commit(&app.project, &st.armed, st.edge_pos, app.playhead) {
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
    if run_edit(app, player, run_commit_action(app, &st.armed, kind, st.ripple)) {
        app.mode = Mode::Normal;
    }
}

/// Cancel: snap the playhead back to the armed edge, project timeline
/// back on the monitor.
fn cancel_extend(app: &mut App, player: &MpvPlayer) {
    let Some(st) = extend_state(app) else { return };
    exit_extend(app, player, st.edge_pos, "extend cancelled");
}

/// Mode-aware absolute seek: clamped to the committable range, translated
/// through the preview offset for mpv.
fn seek_to_mode(app: &mut App, player: &MpvPlayer, st: &ExtendState, t: f64) {
    if !app.guard_loaded() {
        return;
    }
    let target = clamp_extend(st, t).max(0.0);
    app.playhead = target;
    player.seek_absolute(target - st.preview_off + 0.004);
}

fn seek_rel_mode(app: &mut App, player: &MpvPlayer, st: &ExtendState, delta: f64) {
    let target = app.playhead + delta;
    seek_to_mode(app, player, st, target);
}

/// Mode-aware frame step: the frame index is clamped to the range, and
/// mpv only steps when the model actually moved (no divergence).
fn step_mode(app: &mut App, player: &MpvPlayer, st: &ExtendState, dir: i64, mult: u32) {
    if !app.guard_loaded() {
        return;
    }
    let Some(fps) = app.project.fps.as_ref() else { return };
    let f = secs(fps);
    let lo_i = (st.lo * f).round() as i64;
    let hi_i = st.hi.map(|h| (h * f).round() as i64).unwrap_or(i64::MAX);
    let idx = ((app.playhead * f).round() as i64 + dir * mult as i64).clamp(lo_i, hi_i);
    let next = (idx.max(0) as f64) / f;
    if next != app.playhead {
        app.playhead = next;
        for _ in 0..mult {
            if dir > 0 {
                player.frame_step();
            } else {
                player.frame_back_step();
            }
        }
    }
}

fn extend_inert(app: &mut App) {
    app.set_message("exit extend mode first (Esc)");
}

fn extend_key(app: &mut App, player: &MpvPlayer, key: KeyEvent) {
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
        KeyCode::Char('j') => match next_cut(&cut_points(&app.project), app.playhead, eps) {
            Some(t) => seek_to_mode(app, player, &st, t),
            None => seek_to_mode(app, player, &st, app.total()),
        },
        KeyCode::Char('k') => match prev_cut(&cut_points(&app.project), app.playhead, eps) {
            Some(t) => seek_to_mode(app, player, &st, t),
            None => seek_to_mode(app, player, &st, 0.0),
        },
        KeyCode::Char('w') => match next_clip_start(&app.project, app.playhead, eps) {
            Some(t) => seek_to_mode(app, player, &st, t),
            None => seek_to_mode(app, player, &st, app.total()),
        },
        KeyCode::Char('b') => match prev_clip_start(&app.project, app.playhead, eps) {
            Some(t) => seek_to_mode(app, player, &st, t),
            None => seek_to_mode(app, player, &st, 0.0),
        },
        KeyCode::Char('g') => app.pending = Pending::G,
        KeyCode::Char('G') | KeyCode::Char('$') => seek_to_mode(app, player, &st, app.total()),
        KeyCode::Char('e') => commit_extend(app, player),
        KeyCode::Char('E') => {
            let ripple = match &mut app.mode {
                Mode::Extend(inner) => {
                    inner.ripple = !inner.ripple;
                    inner.ripple
                }
                _ => return,
            };
            app.set_message(if ripple { "extend: ripple ON" } else { "extend: preserve" });
        }
        KeyCode::Esc => cancel_extend(app, player),
        KeyCode::Char('q') => cancel_extend(app, player),
        KeyCode::Char(':') | KeyCode::Char('r') | KeyCode::Char('u')
        | KeyCode::Char('i') | KeyCode::Char('I') | KeyCode::Char('o') | KeyCode::Char('O')
        | KeyCode::Char('s') | KeyCode::Char('p') | KeyCode::Char('P')
        | KeyCode::Char('d') | KeyCode::Char('y') => extend_inert(app),
        _ => {}
    }

    if !matches!(
        code,
        KeyCode::Char('g') | KeyCode::Char('0'..='9')
    ) {
        app.pending = Pending::None;
    }
}

// ------------------------------------------------------------ rendering

fn render(f: &mut Frame, app: &App) {
    let [status, timeline, info] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Min(0),
    ])
    .areas(f.area());

    render_status(f, status, app);
    render_timeline(f, timeline, app);
    render_info(f, info, app);
}

/// Current/total frame counts for display (1-based current, clamped to
/// the last frame at the timeline's end).
fn frame_counts(project: &Project, total: f64, playhead: f64) -> Option<(i64, i64)> {
    let fps = project.fps.as_ref()?;
    let total_frames = (total * secs(fps)).ceil().max(1.0) as i64;
    let current = ((playhead * secs(fps)).floor() as i64 + 1).clamp(1, total_frames);
    Some((current, total_frames))
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
    };
    let count = app.count.map(|c| format!("{c}")).unwrap_or_default();

    let style = Style::default().bg(Color::Blue).fg(Color::White);
    let frames = match frame_counts(&app.project, app.total(), app.playhead) {
        Some((c, t)) => format!(" · f{c}/f{t}"),
        None => String::new(),
    };
    let line = Line::from(vec![
        Span::styled(" nle ", style.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {name} · "), style),
        Span::styled(
            fmt_timecode(app.playhead, app.project.fps.as_ref()),
            style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " / {}{frames} · ",
                fmt_timecode(app.total(), app.project.fps.as_ref())
            ),
            style,
        ),
        Span::styled(format!("{icon} · {mode}{count}"), style),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_timeline(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" v1 ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 4 || inner.height < 2 {
        return;
    }

    let segs = preview::segments(&app.project).unwrap_or_default();
    // In extend mode the row rescales past the timeline end so the
    // projected range stays visible while navigating outward.
    let in_extend = matches!(app.mode, Mode::Extend(_));
    let total = if in_extend {
        app.total().max(app.playhead).max(0.001)
    } else {
        app.total().max(0.001)
    };
    let w = inner.width as f64;

    let parts: Vec<(f64, String, Style)> = segs
        .iter()
        .map(|s| match s {
            Segment::Clip { path, length, .. } => (
                secs(length),
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                Style::default().bg(Color::Cyan).fg(Color::Black),
            ),
            Segment::Gap { length } => (
                secs(length),
                "gap".to_string(),
                Style::default().bg(Color::DarkGray).fg(Color::Black),
            ),
        })
        .collect();

    // Track row: segments with boundary dividers. The playhead doesn't
    // live here in normal mode (it renders on the marker row below); in
    // extend mode a yellow notch marks it on this row too.
    let mut x: u16 = 0;
    let mut cum: f64 = 0.0;
    let mut cells: Vec<(u16, String, Style)> = Vec::new();
    let n = parts.len();
    for (i, (dur, label, style)) in parts.iter().enumerate() {
        cum += dur;
        let mut end_x = ((cum / total) * w).round() as u16;
        if i == n - 1 {
            end_x = inner.width;
        }
        let width = end_x.saturating_sub(x).max(1);
        // A divider at the boundary, drawn on this segment's own
        // background so it contrasts with both neighbors.
        let text = if i == 0 {
            truncate_label(label, width)
        } else {
            format!("▏{}", truncate_label(label, width.saturating_sub(1)))
        };
        cells.push((x, text, *style));
        x = end_x;
    }
    let mut spans: Vec<Span> = Vec::new();
    if in_extend {
        // Notch the playhead into the row: split the containing part's
        // text around a yellow marker (same left-hugging convention as
        // every other timeline glyph).
        let ph = marker_col(app.playhead, total, inner.width);
        let notch = Style::default().fg(Color::Yellow);
        for (x0, text, style) in &cells {
            let chars: Vec<char> = text.chars().collect();
            let off = (ph as usize).saturating_sub(*x0 as usize);
            if ph >= *x0 && off < chars.len() {
                let before: String = chars[..off].iter().collect();
                let after: String = chars[off + 1..].iter().collect();
                if !before.is_empty() {
                    spans.push(Span::styled(before, *style));
                }
                spans.push(Span::styled("▏".to_string(), notch));
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
    let track_row = Rect { height: 1, ..inner };
    f.render_widget(Paragraph::new(Line::from(spans)), track_row);

    // Marker row: just the playhead — red ▏ normally, WHITE when it sits
    // exactly on a cut frame (the snapped position makes that test
    // exact). EOF still hugs the border with ▕. In extend mode the row
    // also carries the workable span (yellow) and the material extent
    // beyond it (dim), with a yellow playhead.
    let ph = marker_col(app.playhead, total, inner.width);
    let at_end = app.eof;
    let mut marker_cells: Vec<(char, Style)> = vec![(' ', Style::default()); inner.width as usize];
    if let Mode::Extend(st) = &app.mode {
        // Material extent first (dim, underneath), then the workable span.
        if let Some((m0, m1)) = st.mat {
            paint_run(&mut marker_cells, total, inner.width, m0, m1, '·', Style::default().fg(Color::DarkGray));
        }
        let hi_edge = st.hi.unwrap_or(f64::INFINITY);
        paint_run(&mut marker_cells, total, inner.width, st.lo, hi_edge, '─', Style::default().fg(Color::Yellow));
        let color = if on_boundary(&app.project, app.playhead) {
            Color::White
        } else {
            Color::Yellow
        };
        marker_cells[ph as usize] = (
            if at_end { '▕' } else { '▏' },
            Style::default().fg(color),
        );
    } else {
        let color = if on_boundary(&app.project, app.playhead) {
            Color::White
        } else {
            Color::Red
        };
        marker_cells[ph as usize] = (
            if at_end { '▕' } else { '▏' },
            Style::default().fg(color),
        );
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
    let playhead_row = Rect { y: inner.y + 1, height: 1, ..inner };
    f.render_widget(Paragraph::new(Line::from(marker)), playhead_row);
}

/// Paint `glyph` with `style` over the cells spanning `[a, b)` in
/// timeline seconds (`b` may exceed the row — it clamps).
fn paint_run(cells: &mut [(char, Style)], total: f64, width: u16, a: f64, b: f64, glyph: char, style: Style) {
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

    let first_line = match &app.mode {
        Mode::Extend(st) => extend_status(app, st),
        _ => clip_line(app, t),
    };
fn clip_line(app: &App, t: f64) -> String {
    clip_under(&app.project, t)
        .map(|c| {
            let asset = app
                .project
                .asset(c.asset)
                .map(|a| a.path.display().to_string())
                .unwrap_or_else(|| "?".into());
            let fps = app.project.fps.as_ref();
            let (seg_start, seg_end) = {
                let track = app.project.timeline.video_track().unwrap();
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
        .unwrap_or_else(|| match gap_under(&app.project, t) {
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
    let hi = st.hi.map(|h| fmt_timecode(h, fps)).unwrap_or_else(|| "…".to_string());
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
        Armed::TailGap => format!(
            "EXTEND timeline end · range {range} · free growth{flag}"
        ),
        Armed::HeadGap => format!(
            "EXTEND timeline start · range {range}{flag}"
        ),
    }
}

/// Source seconds still unused on the extend side of a clip edge:
/// before the in-point (head) or past the out-point (tail). `None`
/// when the asset duration is unknown.
fn material_remaining(app: &App, id: u32, head: bool) -> Option<f64> {
    let track = app.project.timeline.video_track()?;
    let clip = track.segments.iter().filter_map(|seg| match seg {
        TimelineSegment::Clip(c) if c.id == id => Some(c),
        _ => None,
    }).next()?;
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
        app.project
            .timeline
            .video_track()
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

    let help = if matches!(app.mode, Mode::Extend(_)) {
        "h/l move · f/F frame · j/k cuts · space play · e commit · E ripple · Esc cancel"
    } else {
        "space play · h/l ±1s · H/L ±5s · f/F frame · j/k cuts · w/b clips · gg/G/0 ends · i/o trim clip/gap · I/O ripple-trim · s split · dd/D del/gap · yy/p/P · u/Ctrl-r undo · e/E extend · r reload · : cmd · q quit"
    };

    let mut lines = vec![
        Line::from(first_line),
        Line::from(""),
        Line::from(Span::styled(stats, Style::default().fg(Color::Gray))),
        Line::from(""),
        Line::from(Span::styled(help.to_string(), Style::default().fg(Color::DarkGray))),
    ];
    if let Some(err) = &app.mpv_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("mpv: {err}"),
            Style::default().fg(Color::Red),
        )));
    }
    if !app.message.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            app.message.clone(),
            Style::default().fg(Color::Yellow),
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
fn on_boundary(project: &Project, t: f64) -> bool {
    // The cut list already includes the timeline's start (0) and end —
    // every boundary instant in one place.
    cut_points(project).iter().any(|&c| (c - t).abs() < 1e-9)
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
                segments: vec![
                    TimelineSegment::Clip(nle_core::Clip {
                        id: 1,
                        asset: a,
                        asset_in: Seconds::from_integer(1),
                        asset_out: Seconds::from_integer(4),
                    }),
                    TimelineSegment::Gap(TimelineGap { duration: Seconds::from_integer(2) }),
                    TimelineSegment::Clip(nle_core::Clip {
                        id: 2,
                        asset: a,
                        asset_in: Seconds::from_integer(0),
                        asset_out: Seconds::from_integer(2),
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
        assert_eq!(resolve_armed(&p, 0.0), Ok((Armed::Clip { id: 1, head: true }, 0.0)));
        assert_eq!(resolve_armed(&p, 5.0), Ok((Armed::Clip { id: 2, head: true }, 5.0)));
        // Clip ends (gap starts resolve to the clip tail).
        assert_eq!(resolve_armed(&p, 3.0), Ok((Armed::Clip { id: 1, head: false }, 3.0)));
        assert_eq!(resolve_armed(&p, 7.0), Ok((Armed::Clip { id: 2, head: false }, 7.0)));
        // Mid-segment positions refuse.
        assert!(resolve_armed(&p, 1.5).is_err());
        assert!(resolve_armed(&p, 4.0).is_err());
        // Shared clip|clip cut arms the right head.
        let mut q = asset_project();
        q.timeline.video_track_mut().unwrap().segments.remove(1);
        assert_eq!(resolve_armed(&q, 3.0), Ok((Armed::Clip { id: 2, head: true }, 3.0)));
        // Virtual edges with gaps.
        let mut r = asset_project();
        r.timeline.video_track_mut().unwrap().segments.push(
            TimelineSegment::Gap(TimelineGap { duration: Seconds::from_integer(3) }),
        );
        assert_eq!(resolve_armed(&r, 10.0), Ok((Armed::TailGap, 10.0)));
        let mut s = asset_project();
        s.timeline.video_track_mut().unwrap().segments.insert(
            0,
            TimelineSegment::Gap(TimelineGap { duration: Seconds::from_integer(2) }),
        );
        assert_eq!(resolve_armed(&s, 0.0), Ok((Armed::HeadGap, 0.0)));
        // Empty timeline refuses.
        assert!(resolve_armed(&Project::new(), 0.0).is_err());
    }

    #[test]
    fn plan_extend_clip_bounds() {
        let p = asset_project();
        // Tail of #1 at 3.0: lo = clip start, hi = min(gap end 5, 3 + (10-4)).
        let plan = plan_extend(&p, 3.0, false).unwrap();
        assert_eq!(plan.armed, Armed::Clip { id: 1, head: false });
        assert_eq!(plan.edge_pos, 3.0);
        assert_eq!((plan.lo, plan.hi), (0.0, Some(5.0)));
        assert_eq!(plan.mat, Some((-1.0, 9.0)));
        match plan.preview {
            ExtendPreviewSpec::Clip { src_start, length, origin, .. } => {
                assert_eq!(src_start, Seconds::from_integer(1));
                assert_eq!(length, Seconds::from_integer(5));
                assert_eq!(origin, 0.0);
            }
            _ => panic!("expected clip preview"),
        }
        // Head of #2 at 5.0: ain 0 means no left room.
        let plan = plan_extend(&p, 5.0, true).unwrap();
        assert_eq!((plan.lo, plan.hi), (5.0, Some(7.0)));
        // Head of #1 at 0.0: clamped at the origin despite material.
        let plan = plan_extend(&p, 0.0, false).unwrap();
        assert_eq!((plan.lo, plan.hi), (0.0, Some(3.0)));
        // Unknown asset duration: no extension room, trim/delete only.
        let mut q = asset_project();
        q.media[0].probe.format.duration = None;
        let plan = plan_extend(&q, 3.0, false).unwrap();
        assert_eq!((plan.lo, plan.hi), (0.0, Some(3.0)));
        assert_eq!(plan.mat, Some((-1.0, 3.0)));
    }

    #[test]
    fn plan_extend_virtual_edges() {
        let mut p = asset_project();
        p.timeline.video_track_mut().unwrap().segments.push(
            TimelineSegment::Gap(TimelineGap { duration: Seconds::from_integer(3) }),
        );
        let plan = plan_extend(&p, 10.0, false).unwrap();
        assert_eq!(plan.armed, Armed::TailGap);
        assert_eq!(plan.lo, 7.0);
        assert_eq!(plan.hi, None);
        assert_eq!(plan.mat, None);
        assert!(matches!(plan.preview, ExtendPreviewSpec::Black { origin } if origin == 7.0));
        // Leading gap: bounded head range, black preview, no material line.
        let mut s = asset_project();
        s.timeline.video_track_mut().unwrap().segments.insert(
            0,
            TimelineSegment::Gap(TimelineGap { duration: Seconds::from_integer(2) }),
        );
        let plan = plan_extend(&s, 0.0, false).unwrap();
        assert_eq!(plan.armed, Armed::HeadGap);
        assert_eq!((plan.lo, plan.hi), (0.0, Some(2.0)));
        assert_eq!(plan.mat, None);
    }

    #[test]
    fn clamp_extend_holds_both_modes() {
        let st = ExtendState {
            armed: Armed::TailGap,
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
        let open = ExtendState { hi: None, ..st.clone() };
        assert_eq!(clamp_extend(&open, 1e9), 1e9);
    }

    #[test]
    fn plan_commit_routing_table() {
        let p = asset_project(); // C1 [0..3), gap [3..5), C2 [5..7)
        let tail1 = Armed::Clip { id: 1, head: false };
        assert_eq!(plan_commit(&p, &tail1, 3.0, 4.0), Ok(CommitKind::GrowTailIntoGap));
        assert_eq!(plan_commit(&p, &tail1, 3.0, 1.0), Ok(CommitKind::TrimTail));
        assert_eq!(plan_commit(&p, &tail1, 3.0, 0.0), Ok(CommitKind::Delete));
        assert_eq!(plan_commit(&p, &tail1, 3.0, 3.0), Ok(CommitKind::Noop));
        let head2 = Armed::Clip { id: 2, head: true };
        assert_eq!(plan_commit(&p, &head2, 5.0, 6.0), Ok(CommitKind::TrimHead));
        assert_eq!(plan_commit(&p, &head2, 5.0, 4.0), Ok(CommitKind::GrowHeadIntoGap));
        assert_eq!(plan_commit(&p, &head2, 5.0, 7.0), Ok(CommitKind::Delete));
        assert_eq!(plan_commit(&p, &head2, 5.0, 5.0), Ok(CommitKind::Noop));
        // Routing is flag-independent (ripple splits at run time).
        assert_eq!(
            plan_commit(&p, &tail1, 3.0, 4.0),
            plan_commit(&p, &tail1, 3.0, 4.0)
        );
        // Past-end tail with no following gap.
        let mut q = asset_project();
        q.timeline.video_track_mut().unwrap().segments.truncate(1);
        let tail = Armed::Clip { id: 1, head: false };
        assert_eq!(plan_commit(&q, &tail, 3.0, 4.0), Ok(CommitKind::GrowTailPastEnd));
        // Virtual edges.
        let mut r = asset_project();
        r.timeline.video_track_mut().unwrap().segments.push(
            TimelineSegment::Gap(TimelineGap { duration: Seconds::from_integer(3) }),
        );
        assert_eq!(plan_commit(&r, &Armed::TailGap, 10.0, 8.0), Ok(CommitKind::ShortenTail));
        assert_eq!(plan_commit(&r, &Armed::TailGap, 10.0, 12.0), Ok(CommitKind::GrowTailGap));
        assert_eq!(plan_commit(&r, &Armed::TailGap, 10.0, 10.0), Ok(CommitKind::Noop));
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
        let b = run_commit_action(&app, &tail1, CommitKind::Delete, true).unwrap();
        assert!(b.desc.contains("ripple"), "{}", b.desc);
        let b = run_commit_action(&app, &tail1, CommitKind::Delete, false).unwrap();
        assert_eq!(b.desc, "delete gap");
    }

    fn seg_clip(id: u32, dur: i64) -> TimelineSegment {
        TimelineSegment::Clip(nle_core::Clip {
            id,
            asset: 1,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(dur),
        })
    }

    fn project() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.timeline = Timeline {
            tracks: vec![Track {
                kind: TrackKind::Video,
                segments: vec![
                    seg_clip(1, 3),
                    TimelineSegment::Gap(TimelineGap { duration: Seconds::from_integer(2) }),
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
        let pts = cut_points(&project());
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
        assert_eq!(next_clip_start(&project(), 2.9995, eps), Some(5.0));
        assert_eq!(prev_clip_start(&project(), 3.0005, eps), Some(0.0));
    }

    #[test]
    fn on_boundary_detects_cuts_and_edges() {
        let p = project(); // cuts at 3 and 5; edges 0 and 7
        assert!(on_boundary(&p, 0.0), "timeline start");
        assert!(on_boundary(&p, 3.0));
        assert!(on_boundary(&p, 5.0));
        assert!(on_boundary(&p, 7.0), "timeline end");
        assert!(!on_boundary(&p, 1.5));
        assert!(!on_boundary(&p, 4.0));
    }

    #[test]
    fn total_includes_the_sequence_tail() {
        // no tail concept in the segment model: total == sum of segments
        let app = App::new(project(), PathBuf::from("t.json"));
        assert_eq!(app.total(), 7.0);
    }

    #[test]
    fn frame_counts_are_clamped_and_1_based() {
        let mut p = project(); // 7.0 @ 30fps → 210 frames
        p.fps = Some(Seconds::new(30, 1));
        let total = 7.0;
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 0.0;
        assert_eq!(frame_counts(&app.project, total, 0.0), Some((1, 210)));
        assert_eq!(frame_counts(&app.project, total, 8.2), Some((210, 210)));
        assert_eq!(frame_counts(&app.project, total, total), Some((210, 210)));
        app.project.fps = None;
        assert_eq!(frame_counts(&app.project, 10.0, 5.0), None);
        let _ = &mut app;
    }

    #[test]
    fn snap_to_cut_is_magnetic_within_one_and_a_half_frames() {
        let mut p = project();
        p.fps = Some(Seconds::new(30, 1));
        let radius = 1.5 / 30.0;
        assert_eq!(snap_to_cut(3.0 + radius, &p), 3.0);
        assert_eq!(snap_to_cut(3.0 - radius, &p), 3.0);
        assert_eq!(snap_to_cut(3.5, &p), 3.5);
        assert_eq!(snap_to_cut(1.0, &p), 1.0);
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
}
