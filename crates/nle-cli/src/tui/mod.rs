//! The terminal dashboard: timeline visualization, vim keys, live playhead.
//!
//! Input model: a small vim. Counts work (`3l` = seek 3s), `gg`/`G` jump to
//! ends, `j`/`k` walk cut points, `w`/`b` walk clip starts, `:` opens a
//! command line. Playback keys are sent to mpv; state flows back through
//! the reader thread and is rendered on the next tick (~20fps redraw).

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use nle_core::action::{self, Action, Edge, GapLen, ParseDefaults, ParseError, Target};
use nle_core::timeline::{Segment as TimelineSegment, Track, TransitionKind, TransitionSource};
use nle_core::{
    Armed, Clip, CommitKind, History, Project, Seconds, clip_span, commit_batch, frame_of,
    gap_span, load_register, plan_commit, resolve_armed, track_label,
};
#[cfg(test)]
use nle_core::{Batch, EditError};

use crate::mpv::{Mpv, PlaybackState};
use crate::preview;

mod motion;
mod panels;

use panels::render;

use motion::{
    cut_points, fmt_timecode, gap_under, half_frame, next_clip_start, next_cut, next_gap_start,
    prev_clip_start, prev_cut, prev_gap_start, secs, snap_to_cut, snap_to_frame,
};

// ------------------------------------------------------------ cut motions

mod jobs;

use jobs::{Job, PreviewJob, PreviewProgress, poll_job, poll_preview, start_job, start_preview};

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

mod keys;

use keys::fx::{
    add_transition, fx_key, open_fx, open_transition_form, transition_form_key, transition_target,
};
use keys::pool::{
    leave_pool_command, load_pool_preview_at, open_pool, pool_key, pool_last_frame,
    pool_strip_active,
};

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

/// Undo the last edit, restoring the playhead to where it was made.
fn do_undo(app: &mut App, player: &dyn Mpv) {
    run_action(app, player, &Action::Undo);
}

/// Redo the last undone edit.
fn do_redo(app: &mut App, player: &dyn Mpv) {
    run_action(app, player, &Action::Redo);
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

mod actions;

use actions::{clip_under, edit_delete, edit_yank, run_action, seek_clip_end};

// ------------------------------------------------------------ edit plumbing

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

/// The status-bar mark: a crocodile. `=` is the closed mouth; a delete opens
/// it to `>` for a moment (`CHOMP_MS`), then it closes again.
const CROC: &str = r#"=";::;>--"#;
const CHOMP: &str = r#">";::;>--"#;
const CHOMP_MS: u64 = 250;
/// How long a preview build runs before the status row announces it. A warm
/// cache (or a no-proxy timeline) finishes in milliseconds; without this every
/// edit flashed `building preview…` for a frame.
const PREVIEW_NOTICE_DELAY: Duration = Duration::from_millis(200);

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

#[cfg(test)]
mod tests;
