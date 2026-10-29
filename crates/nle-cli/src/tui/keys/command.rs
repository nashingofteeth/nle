//! The `:` line: the command grammar is the CLI grammar, so most lines turn
//! into an `Action` through `action::parse`. The few process-bound commands
//! (import, export, detect, EDL/OTIO) start a background job instead.

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};

use nle_core::action::{self, Action, ParseDefaults, ParseError};
use nle_core::{History, frame_of, track_label};

use crate::mpv::Mpv;

use super::super::actions::{clip_under, run_action};
use super::super::jobs::start_job;
use super::super::keys::pool::leave_pool_command;
use super::super::motion::fmt_timecode;
use super::super::{App, Mode, persist_playhead, secs};
/// The `:` commands, shown by `:help` and in the Command-mode cheat-sheet.
/// The edit commands are the CLI's, flag for flag (`--at` defaults to the
/// playhead here).
pub(crate) const COMMAND_HELP: &str = "commands — :trim-in|out · :split · :gap · :transition · :effect · :delete · :yank · :paste · :extend · :speed · :undo · :redo · :seek · :track · :media · :cache · :import · :add · :show · :purge · :detect · :edl-import · :otio-import · :export · :w · :q";

/// `:import PATH…` — probe files and add them to the media pool. An
/// out-of-band pool write, so it re-baselines the undo sidecar (a stale
/// `Change::Assets` entry would otherwise refuse to apply).
pub(crate) fn import_command(app: &mut App, cmd: &str) -> String {
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
                        app.project.add_asset(path, report, playhead);
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
        if let Err(e) = app.project.save_history(&History::sidecar_path(&app.path)) {
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

/// The front-end's own (non-edit) commands: queries, imports, and the two
/// long renders. Everything the *model* can do is an [`Action`] above;
/// these need processes (ffprobe/ffmpeg) or print state. Same flags as
/// the CLI, via the shared tokenizer.
pub(crate) fn io_command(app: &mut App, player: &dyn Mpv, cmd: &str) -> Option<String> {
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
pub(crate) fn flag_time(f: &mut action::Flags, name: &str) -> Result<Option<f64>, String> {
    match f.take(name) {
        Some(v) => nle_core::parse_signed_time(&v)
            .map(|t| Some(secs(&t)))
            .map_err(|e| format!("invalid --{name} {v:?} ({e})")),
        None => Ok(None),
    }
}

pub(crate) fn flag_usize(f: &mut action::Flags, name: &str) -> Result<Option<usize>, String> {
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
pub(crate) fn add_command(
    app: &mut App,
    player: &dyn Mpv,
    f: &mut action::Flags,
) -> Option<String> {
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
            track = crate::cmd::cli_index(n, "--track")?;
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
    let (scratch, imported) = match crate::cmd::project::stage_clip(
        &mut app.project,
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
        if let Err(e) = app.project.save_history(&History::sidecar_path(&app.path)) {
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
pub(crate) fn seek_command(
    app: &mut App,
    player: &dyn Mpv,
    f: &mut action::Flags,
) -> Option<String> {
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
pub(crate) fn export_command(app: &mut App, f: &mut action::Flags) -> Option<String> {
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
        let result =
            crate::cmd::export::run_export(&project_path, &output, encoder, crf, &preset, &bitrate)
                .map_err(|e| e.to_string());
        let _ = job.send(result);
    });
    Some(notice)
}

/// `:detect FILE [--threshold T] [--min-shot S] [--black-min S]
/// [--no-gaps] [--fps R] [--video-only] [--list] [--append] [--track N]
/// [--force]` — the CLI's `detect-edits`, in a worker (it can chew
/// through minutes of footage). The project reloads when it lands.
pub(crate) fn detect_command(app: &mut App, f: &mut action::Flags) -> Option<String> {
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
        let result = crate::cmd::detect::detect_edits_work(crate::cmd::detect::DetectArgs {
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
pub(crate) fn edl_import_command(app: &mut App, f: &mut action::Flags) -> Option<String> {
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
        let result = crate::cmd::import::import_edl_work(
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
pub(crate) fn otio_import_command(app: &mut App, f: &mut action::Flags) -> Option<String> {
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
        let result = crate::cmd::import::import_otio_work(
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
pub(crate) fn parse_defaults(app: &App) -> ParseDefaults {
    ParseDefaults {
        at: Some(app.playhead),
        track: app.active(),
        clip: clip_under(&app.project, app.playhead, app.active()).map(|c| c.id),
    }
}

pub(crate) fn command_key(app: &mut App, player: &dyn Mpv, buf: String, key: KeyEvent) {
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
pub(crate) fn run_command(app: &mut App, player: &dyn Mpv, cmd: &str) -> Option<String> {
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
pub(crate) fn show_command(app: &App) -> String {
    let report = crate::cmd::project::show_report(&app.project);
    crate::log::line(&report);
    report.lines().next().unwrap_or_default().to_string()
}

/// `:media ls` — pool summary; the per-asset listing goes to the log.
pub(crate) fn media_ls_command(app: &App) -> String {
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
pub(crate) fn track_ls_command(app: &App) -> String {
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
pub(crate) fn cache_command(f: &mut action::Flags) -> Option<String> {
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
