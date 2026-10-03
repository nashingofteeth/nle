//! The terminal dashboard: timeline visualization, vim keys, live playhead.
//!
//! Input model: a small vim. Counts work (`3l` = seek 3s), `gg`/`G` jump to
//! ends, `j`/`k` walk cut points, `w`/`b` walk clip starts, `:` opens a
//! command line. Playback keys are sent to mpv; state flows back through
//! the reader thread and is rendered on the next tick (~20fps redraw).

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
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
    /// Seek to apply once the (re)loaded file is ready.
    pub pending_seek: Option<f64>,
    /// After a reload we must see `file_loaded=false` before trusting
    /// `file_loaded=true` again (else we'd seek the OLD file).
    pub awaiting_unload: bool,
    pub quit: bool,
    pub message: String,
    // synced from mpv every tick
    pub paused: bool,
    pub loaded: bool,
    pub dead: bool,
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
            pending_seek: None,
            awaiting_unload: false,
            quit: false,
            message: String::new(),
            paused: true,
            loaded: false,
            dead: false,
            mpv_error: None,
            exit_message: None,
        }
    }

    pub fn set_message(&mut self, msg: &str) {
        self.message = msg.to_string();
    }

    pub fn sync(&mut self, st: &PlaybackState) {
        if let Some(t) = st.time_pos {
            self.playhead = t;
        }
        self.paused = st.paused;
        self.loaded = st.file_loaded;
        self.dead = st.dead;
        if let Some(e) = &st.last_error {
            self.mpv_error = Some(e.clone());
        }
    }

    pub fn total(&self) -> f64 {
        self.project
            .timeline
            .video_track()
            .map(|t| secs(&t.end()))
            .unwrap_or(0.0)
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
        }
    }

    fn seek_to(&mut self, player: &MpvPlayer, t: f64) {
        if self.guard_loaded() {
            player.seek_absolute(t.max(0.0));
        }
    }
}

// ------------------------------------------------------------ event loop

pub fn run(project: Project, path: PathBuf, player: MpvPlayer) -> anyhow::Result<()> {
    let mut terminal = ratatui::init(); // raw mode + alternate screen + panic hook
    let mut app = App::new(project, path);

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

        // Apply a queued seek once the (re)loaded file is actually playing.
        if app.awaiting_unload && !st.file_loaded {
            app.awaiting_unload = false;
        }
        if let Some(t) = app.pending_seek
            && st.file_loaded && !app.awaiting_unload {
                player.seek_absolute(t.max(0.0));
                app.pending_seek = None;
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

/// Reload the project from disk and rebuild the preview. Lets you edit
/// `nle.json` in another window (e.g. vim) and watch it here.
fn reload_project(app: &mut App, player: &MpvPlayer) -> anyhow::Result<()> {
    let project = Project::load(&app.path)?;
    let edl = preview::build_edl(&project)?;
    app.project = project;
    player.load(&edl);
    app.pending_seek = Some(app.playhead);
    app.awaiting_unload = true;
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
                player.toggle_pause();
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
            }
        }
        KeyCode::Char('F') => {
            if app.guard_loaded() {
                for _ in 0..mult {
                    player.frame_back_step();
                }
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
            let Some(clip) = clip_under(&app.project, app.playhead).cloned() else {
                app.set_message("playhead is in a gap — j/w/b to a clip");
                return;
            };
            let result = edit::trim_in(&app.project, clip.id, app.playhead);
            run_edit(app, player, result);
        }
        KeyCode::Char('I') => {
            let Some(clip) = clip_under(&app.project, app.playhead).cloned() else {
                app.set_message("playhead is in a gap — j/w/b to a clip");
                return;
            };
            let result = edit::trim_in_ripple(&app.project, clip.id, app.playhead);
            run_edit(app, player, result);
        }
        KeyCode::Char('o') => {
            let Some(clip) = clip_under(&app.project, app.playhead).cloned() else {
                app.set_message("playhead is in a gap — j/w/b to a clip");
                return;
            };
            let result = edit::trim_out(&app.project, clip.id, app.playhead);
            run_edit(app, player, result);
        }
        KeyCode::Char('O') => {
            let Some(clip) = clip_under(&app.project, app.playhead).cloned() else {
                app.set_message("playhead is in a gap — j/w/b to a clip");
                return;
            };
            let result = edit::trim_out_ripple(&app.project, clip.id, app.playhead);
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
/// (and undo/redo) — no manual save state to get wrong.
fn persist_and_rebuild(app: &mut App, player: &MpvPlayer) {
    if let Err(e) = app.project.save(&app.path) {
        app.set_message(&format!("save failed: {e}"));
        return;
    }
    match preview::build_edl(&app.project) {
        Ok(edl) => {
            player.load(&edl);
            app.pending_seek = Some(app.playhead);
            app.awaiting_unload = true;
        }
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
    let line = Line::from(vec![
        Span::styled(" nle ", style.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {name} · "), style),
        Span::styled(fmt_timecode(app.playhead), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" / {} · ", fmt_timecode(app.total())), style),
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

    // Cumulative rounding: no drift, and the last segment fills the row.
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
        spans.push(Span::styled(truncate_label(label, width), *style));
        x = end_x;
    }
    let track_row = Rect { height: 1, ..inner };
    f.render_widget(Paragraph::new(Line::from(spans)), track_row);

    // Playhead marker on the row below.
    let col = (((app.playhead / total) * (w - 1.0)).round() as u16).min(inner.width - 1);
    let marker = format!("{}▲", " ".repeat(col as usize));
    let playhead_row = Rect { y: inner.y + 1, height: 1, ..inner };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(marker, Style::default().fg(Color::Red)))),
        playhead_row,
    );
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

    let help = "space play · h/l ±1s · H/L ±5s · f/F frame · j/k cuts · w/b clips · gg/G/0 ends · i/o trim · I/O ripple-trim · s split · dd/D del/gap · yy/p/P · u/Ctrl-r undo · r reload · : cmd · q quit";

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
        Paragraph::new(lines).block(Block::bordered().title(" info ")),
        area,
    );
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
    fn label_truncation_and_padding() {
        // center-pad: extra space lands on the right
        assert_eq!(truncate_label("red.mp4", 10), " red.mp4  ");
        assert_eq!(truncate_label("very-long-name.mp4", 8), "very-lon");
        assert_eq!(truncate_label("ab", 2), "ab");
    }
}
