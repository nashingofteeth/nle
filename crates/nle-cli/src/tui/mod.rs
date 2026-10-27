//! The terminal dashboard: timeline visualization, vim keys, live playhead.
//!
//! Input model: a small vim. Counts work (`3l` = seek 3s), `gg`/`G` jump to
//! ends, `j`/`k` walk cut points, `w`/`b` walk clip starts, `:` opens a
//! command line. Playback keys are sent to mpv; state flows back through
//! the reader thread and is rendered on the next tick (~20fps redraw).

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use nle_core::action;
use nle_core::timeline::{Segment as TimelineSegment, Track, TransitionKind};
use nle_core::{Armed, Clip, History, Project, frame_of, load_register, track_label};

use crate::mpv::{Mpv, PlaybackState};

mod motion;
mod panels;

use panels::render;

use motion::{fmt_timecode, gap_under, secs, snap_to_cut, snap_to_frame};

// ------------------------------------------------------------ cut motions

mod jobs;

use jobs::{Job, PreviewJob, PreviewProgress, poll_job, poll_preview, start_preview};

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
    out.push_str(&crate::cmd::project::show_report(&app.project));
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

use keys::handle_key;

use keys::extend::clamp_extend;
use keys::pool::{load_pool_preview_at, pool_last_frame, pool_strip_active};

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

// ------------------------------------------------------------ io commands

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

// ------------------------------------------------------------ extend mode

mod actions;

use actions::clip_under;

// ------------------------------------------------------------ edit plumbing

// ------------------------------------------------------------ extend mode

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

#[cfg(test)]
mod tests;
