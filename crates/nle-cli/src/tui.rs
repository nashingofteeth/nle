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

/// "62.25 s" → "01:02.2" (and "3661" → "1:01:01.0" past an hour).
pub fn fmt_timecode(t: f64) -> String {
    let t = t.max(0.0);
    let h = (t / 3600.0).floor() as u64;
    let m = ((t % 3600.0) / 60.0).floor() as u64;
    let s = t % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{s:04.1}")
    } else {
        format!("{m:02}:{s:04.1}")
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
    for c in &track.clips {
        pts.push(secs(&c.start));
        pts.push(secs(&c.end()));
    }
    pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    pts.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
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
    track.clips.iter().map(|c| secs(&c.start)).find(|&s| s > t + eps)
}

pub fn prev_clip_start(project: &Project, t: f64, eps: f64) -> Option<f64> {
    let track = project.timeline.video_track()?;
    track.clips.iter().rev().map(|c| secs(&c.start)).find(|&s| s < t - eps)
}

// ------------------------------------------------------------ app state

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Command(String),
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
        if !st.paused && st.file_loaded
            && let Some(t) = st.time_pos {
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
            Mode::Normal => unreachable!("checked above"),
        };
        command_key(app, buf, key);
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
        KeyCode::Backspace => app.mode = Mode::Command(buf[..buf.len().saturating_sub(1)].to_string()),
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
    match code {
        KeyCode::Char(c @ '1'..='9') => {
            let d = c.to_digit(10).unwrap_or(0);
            app.count = Some(app.count.unwrap_or(0) * 10 + d);
            return;
        }
        KeyCode::Char('0') if app.count.is_none() => {
            app.seek_to(player, 0.0);
            return;
        }
        _ => {}
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
        // ---- editing (counts deliberately don't multiply edit ops) ----
        KeyCode::Char('i') => {
            let result = match clip_under(&app.project, app.playhead) {
                Some(c) => edit::trim_in(&app.project, c.id, app.playhead),
                None => edit::trim_gap_in(&app.project, app.playhead),
            };
            run_edit(app, player, result);
        }
        KeyCode::Char('I') => {
            let result = match clip_under(&app.project, app.playhead) {
                Some(c) => edit::trim_in_ripple(&app.project, c.id, app.playhead),
                None => edit::trim_gap_in(&app.project, app.playhead),
            };
            run_edit(app, player, result);
        }
        KeyCode::Char('o') => {
            let result = match clip_under(&app.project, app.playhead) {
                Some(c) => edit::trim_out(&app.project, c.id, app.playhead),
                None => edit::trim_gap_out(&app.project, app.playhead),
            };
            run_edit(app, player, result);
        }
        KeyCode::Char('O') => {
            let result = match clip_under(&app.project, app.playhead) {
                Some(c) => edit::trim_out_ripple(&app.project, c.id, app.playhead),
                None => edit::trim_gap_out_ripple(&app.project, app.playhead),
            };
            run_edit(app, player, result);
        }
        KeyCode::Char('s') => {
            let Some(clip) = clip_under(&app.project, app.playhead).cloned() else {
                app.set_message("playhead is in a gap — j/w/b to a clip");
                return;
            };
            let result = edit::split(&app.project, clip.id, app.playhead);
            run_edit(app, player, result);
        }
        KeyCode::Char('D') => {
            // ripple delete: a clip ripples its successors; a gap deletes
            // itself (which is inherently a ripple)
            let result = match clip_under(&app.project, app.playhead) {
                Some(c) => edit::delete_ripple(&app.project, c.id),
                None => edit::delete_gap(&app.project, app.playhead),
            };
            run_edit(app, player, result);
        }
        KeyCode::Char('p') => {
            let Some(clipboard) = app.clipboard.clone() else {
                app.set_message("clipboard empty — yy first");
                return;
            };
            let result = edit::paste(&app.project, &clipboard, Some(app.playhead));
            run_edit(app, player, result);
        }
        KeyCode::Char('P') => {
            let Some(clipboard) = app.clipboard.clone() else {
                app.set_message("clipboard empty — yy first");
                return;
            };
            let result = edit::paste(&app.project, &clipboard, None);
            run_edit(app, player, result);
        }
        KeyCode::Char('u') => match app.history.undo(&mut app.project) {
            Ok(desc) => {
                app.set_message(&format!("undo: {desc}"));
                persist_and_rebuild(app, player);
            }
            Err(e) => app.set_message(&e.to_string()),
        },
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

// ------------------------------------------------------------ edit plumbing

/// The clip the playhead sits inside, if any.
fn clip_under(project: &Project, t: f64) -> Option<&Clip> {
    let track = project.timeline.video_track()?;
    track.clips.iter().find(|c| secs(&c.start) <= t && t < secs(&c.end()))
}

/// Apply an edit batch, then save + rebuild the preview so the monitor
/// reflects the new timeline at the current playhead.
fn run_edit(app: &mut App, player: &MpvPlayer, result: Result<Batch, EditError>) {
    let batch = match result {
        Ok(b) => b,
        Err(e) => {
            app.set_message(&e.to_string());
            return;
        }
    };
    let desc = batch.desc.clone();
    match app.history.apply(&mut app.project, batch) {
        Ok(()) => {
            app.set_message(&desc);
            persist_and_rebuild(app, player);
        }
        Err(e) => app.set_message(&e.to_string()),
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
    // `dd` deletes whatever is under the playhead: a clip, or the gap.
    let result = match clip_under(&app.project, app.playhead) {
        Some(c) => edit::delete_clip(&app.project, c.id),
        None => edit::delete_gap(&app.project, app.playhead),
    };
    run_edit(app, player, result);
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
        Span::styled(fmt_timecode(app.playhead), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" / {}{frames} · ", fmt_timecode(app.total())), style),
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
    let total = app.total().max(0.001);
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
    // live here (it renders on the marker row below).
    let mut x: u16 = 0;
    let mut cum: f64 = 0.0;
    let mut spans: Vec<Span> = Vec::new();
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
        spans.push(Span::styled(text, *style));
        x = end_x;
    }
    let track_row = Rect { height: 1, ..inner };
    f.render_widget(Paragraph::new(Line::from(spans)), track_row);

    // Marker row: just the playhead — red ▏ normally, WHITE when it sits
    // exactly on a cut frame (the snapped position makes that test
    // exact). EOF still hugs the border with ▕.
    let ph = marker_col(app.playhead, total, inner.width);
    let at_end = app.eof;
    let color = if on_cut(&app.project, app.playhead) {
        Color::White
    } else {
        Color::Red
    };
    let mut cells: Vec<(char, Style)> = vec![(' ', Style::default()); inner.width as usize];
    cells[ph as usize] = (
        if at_end { '▕' } else { '▏' },
        Style::default().fg(color),
    );
    let mut marker: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in cells {
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

fn render_info(f: &mut Frame, area: Rect, app: &App) {
    let t = app.playhead;

    let clip_line = clip_under(&app.project, t)
        .map(|c| {
            let asset = app
                .project
                .asset(c.asset)
                .map(|a| a.path.display().to_string())
                .unwrap_or_else(|| "?".into());
            format!(
                "clip #{}  {}   {} – {}   (src {} – {})",
                c.id,
                asset,
                fmt_timecode(secs(&c.start)),
                fmt_timecode(secs(&c.end())),
                fmt_timecode(secs(&c.asset_in)),
                fmt_timecode(secs(&c.asset_out))
            )
        })
        .unwrap_or_else(|| "— gap —".to_string());

    let stats = format!(
        "{} clip(s) · {} · {} fps · undo depth {}",
        app.project.timeline.video_track().map(|t| t.clips.len()).unwrap_or(0),
        fmt_timecode(app.total()),
        app.project
            .fps
            .as_ref()
            .map(|f| format!("{}/{}", f.numer(), f.denom()))
            .unwrap_or_else(|| "?".into()),
        app.history.depth()
    );

    let help = "space play · h/l ±1s · H/L ±5s · f/F frame · j/k cuts · w/b clips · gg/G/0 ends · i/o trim clip/gap · I/O ripple-trim · s split · dd/D del/gap · yy/p/P · u/Ctrl-r undo · r reload · : cmd · q quit";

    let mut lines = vec![
        Line::from(clip_line),
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

/// True when the (frame-snapped) playhead sits exactly on an interior
/// cut — a clip start or end that isn't the timeline's own start/end.
/// Both values come from the same rational→f64 conversions, so exact
/// comparison is meaningful; the epsilon only guards against drift.
fn on_cut(project: &Project, t: f64) -> bool {
    let Some(track) = project.timeline.video_track() else {
        return false;
    };
    let total = secs(&track.end());
    let is_edge = |t: f64| t.abs() < 1e-9 || (total - t).abs() < 1e-9;
    track.clips.iter().any(|c| {
        let (s, e) = (secs(&c.start), secs(&c.end()));
        ((s - t).abs() < 1e-9 || (e - t).abs() < 1e-9) && !is_edge(t)
    })
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
    use nle_core::{Clip, Timeline, Track, TrackKind};

    fn project() -> Project {
        let mut p = Project::new();
        p.timeline = Timeline {
            length: None,
            tracks: vec![Track {
                kind: TrackKind::Video,
                clips: vec![
                    Clip {
                        id: 1,
                        asset: 1,
                        start: Seconds::from_integer(0),
                        asset_in: Seconds::from_integer(0),
                        asset_out: Seconds::from_integer(3),
                    },
                    Clip {
                        id: 2,
                        asset: 1,
                        start: Seconds::from_integer(5),
                        asset_in: Seconds::from_integer(0),
                        asset_out: Seconds::from_integer(2),
                    },
                ],
            }],
        };
        p
    }

    #[test]
    fn timecode_formatting() {
        assert_eq!(fmt_timecode(0.0), "00:00.0");
        assert_eq!(fmt_timecode(62.25), "01:02.2");
        assert_eq!(fmt_timecode(3661.0), "1:01:01.0");
        assert_eq!(fmt_timecode(-5.0), "00:00.0");
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
        // epsilon: within half a frame of a cut counts as standing on it,
        // so the motion goes to the ADJACENT cut
        assert_eq!(next_cut(&pts, 2.9999999, 0.0167), Some(5.0));
    }

    #[test]
    fn motions_tolerate_playhead_drift() {
        // mpv's EDL position can drift by ~a frame (source frame rounding).
        // Half-frame eps: standing just past cut 3 → k reaches the cut
        // BEFORE it, and one press later the cut itself is reachable.
        let pts = vec![0.0, 3.0, 5.0, 7.0];
        let eps = 0.5 / 30.0; // half a frame at 30fps
        assert_eq!(prev_cut(&pts, 3.0005, eps), Some(0.0)); // on cut 3 → skip to previous
        assert_eq!(prev_cut(&pts, 3.1, eps), Some(3.0)); // drifted a frame+ in → land on 3
        assert_eq!(prev_cut(&pts, 0.033, eps), Some(0.0)); // first cut reachable
        assert_eq!(next_cut(&pts, 2.9995, eps), Some(5.0)); // just before cut 3 → skip
        assert_eq!(next_clip_start(&project(), 2.9995, eps), Some(5.0));
        assert_eq!(prev_clip_start(&project(), 3.0005, eps), Some(0.0));
    }

    #[test]
    fn marker_column_alignment() {
        let total = 14.0;
        let width = 100;
        // ends meet: playhead at 0 → col 0, at the end → the last column
        assert_eq!(marker_col(0.0, total, width), 0);
        assert_eq!(marker_col(total, total, width), width - 1);
        // a boundary at half the timeline sits at half the row — the same
        // scale the segment spans use, so ticks and playhead agree
        assert_eq!(marker_col(7.0, total, width), 50);
        // beyond the end clamps; zero-width row can't panic
        assert_eq!(marker_col(99.0, total, width), width - 1);
        assert_eq!(marker_col(1.0, total, 0), 0);
    }

    #[test]
    fn frame_counts_are_clamped_and_1_based() {
        let mut p = project();
        p.fps = Some(Seconds::new(30, 1));
        let total = secs(&p.timeline.video_track().unwrap().end()); // 7.0
        assert_eq!(frame_counts(&p, total, 0.0), Some((1, 210)));
        // playhead beyond the end clamps to the last frame
        assert_eq!(frame_counts(&p, total, 8.2), Some((210, 210)));
        // exact end: clamped to the last frame, never one past it
        assert_eq!(frame_counts(&p, total, total), Some((210, 210)));
        // no fps → no frame display
        let mut no_fps = project();
        no_fps.fps = None;
        assert_eq!(frame_counts(&no_fps, 10.0, 5.0), None);
    }

    #[test]
    fn on_cut_detects_exact_interior_cuts() {
        let p = project(); // clips [0..3) and [5..7) → interior cuts 3 and 5
        let total = secs(&p.timeline.video_track().unwrap().end()); // 7.0
        assert!(on_cut(&p, 3.0));
        assert!(on_cut(&p, 5.0));
        // timeline edges are not cuts
        assert!(!on_cut(&p, 0.0));
        assert!(!on_cut(&p, total));
        // mid-clip / mid-gap positions aren't either
        assert!(!on_cut(&p, 1.5));
        assert!(!on_cut(&p, 4.0));
    }

    #[test]
    fn total_includes_the_sequence_tail() {
        let mut p = project(); // clips end at 7.0
        p.timeline.length = Some(Seconds::from_integer(10));
        let app = App::new(p, PathBuf::from("t.json"));
        assert_eq!(app.total(), 10.0);
        // without a tail, it's the last clip's end
        let mut p2 = project();
        p2.timeline.length = None;
        assert_eq!(App::new(p2, PathBuf::from("t.json")).total(), 7.0);
    }

    #[test]
    fn snap_to_cut_is_magnetic_within_one_and_a_half_frames() {
        let mut p = project(); // cuts at 0, 3, 5, 7
        p.fps = Some(Seconds::new(30, 1));
        let radius = 1.5 / 30.0;
        // a drifted seek that landed one frame PAST the cut snaps back
        assert_eq!(snap_to_cut(3.0 + radius, &p), 3.0);
        assert_eq!(snap_to_cut(3.0 - radius, &p), 3.0);
        // nearest cut wins when two are in range (gap narrower than 3 frames)
        // ... and mid-clip positions stay untouched
        assert_eq!(snap_to_cut(3.5, &p), 3.5);
        assert_eq!(snap_to_cut(1.0, &p), 1.0);
        // exactly at the boundary of the radius: still captured (<=)
        assert_eq!(snap_to_cut(3.0 + 1.5 / 30.0, &p), 3.0);
    }

    #[test]
    fn sync_applies_grid_and_cut_snapping_while_playing() {
        let mut p = project(); // cuts [0, 3, 5, 7], 30fps
        p.fps = Some(Seconds::new(30, 1));
        let mut app = App::new(p, PathBuf::from("t.json"));
        // mpv settled one frame past the cut at 5 → magnetic back to 5
        app.sync(&PlaybackState {
            time_pos: Some(5.0 + 1.0 / 30.0),
            paused: false,
            file_loaded: true,
            ..Default::default()
        });
        assert_eq!(app.playhead, 5.0);
        // mid-clip position: only grid-snapped
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
        let mut p = project(); // cuts [0, 3, 5, 7]
        p.fps = Some(Seconds::new(30, 1));
        let mut app = App::new(p, PathBuf::from("t.json"));
        app.playhead = 2.0;
        // paused: drifted mpv reports must not move the model playhead
        app.sync(&PlaybackState {
            time_pos: Some(5.0 + 1.0 / 30.0),
            paused: true,
            file_loaded: true,
            ..Default::default()
        });
        assert_eq!(app.playhead, 2.0);
        // playing: the streamed position drives it (grid + magnet)
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
        let mut p = project(); // end 7.0 → 210 frames at 30fps
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
        // center-pad: extra space lands on the right
        assert_eq!(truncate_label("red.mp4", 10), " red.mp4  ");
        assert_eq!(truncate_label("very-long-name.mp4", 8), "very-lon");
        assert_eq!(truncate_label("ab", 2), "ab");
    }
}
