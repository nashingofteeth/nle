//! nle — terminal-native video editor.
//!
//! M2 status: interactive editor — TUI dashboard, mpv monitor, vim keys,
//! instant EDL preview. Project file, media pool, timeline, and export
//! landed in M0/M1.

mod log;
#[cfg(feature = "tui")]
mod mpv;
// The extend-mode preview helpers are TUI-only; without that feature they're
// compiled but unused, so allow the dead code rather than gate each one.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod preview;
// The proxy cache renders what `edl://` can't (effects, transitions); the
// preview uses it, so it shares preview's fate outside the TUI.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
mod proxy;
#[cfg(feature = "tui")]
mod tui;

/// Print a line, tolerating a closed consumer (`nle show | head`). Rust
/// ignores `SIGPIPE`, so `say!` *panics* on the broken pipe; a Unix
/// tool should just stop talking. Same syntax as `say!` — all CLI
/// output goes through here. (A genuine write failure, e.g. a full disk,
/// is swallowed too: the exit status stays the command's, not the
/// terminal's.)
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}
pub(crate) use say;

/// `say!` for warnings (stderr).
macro_rules! say_err {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use nle_core::detect::{Span, reconstruct};
use nle_core::edit;
use nle_core::edl::{self, EdlEvent};
use nle_core::otio::{self, OtioItem};
use nle_core::project::wrong_kind;
use nle_core::time::{parse_signed_time, parse_timecode, timecode_to_frames};
use nle_core::timeline::{Clip, Gap, Segment, Timeline, Track, TrackKind};
use nle_core::{
    Action, Asset, Edge, GapLen, Project, Seconds, Target, VideoSettings, format_seconds, frame_of,
    load_register, parse_rate, parse_time, run_action, save_register, track_label,
};
use nle_ffmpeg::detect::{DetectOptions, detect};
use nle_ffmpeg::{Encoder, ExportSpec, RawProbe, RawStream, TrimSpec, probe};

#[derive(Parser)]
#[command(
    name = "nle",
    version,
    about = "Terminal-native video editor",
    // `--to -1.5` is a real thing (an edge reaching back past time 0), and
    // for the rest a negative is our own clear error, not clap's
    // "unexpected argument '-1'".
    allow_negative_numbers = true
)]
struct Cli {
    /// Project file
    #[arg(
        short,
        long,
        global = true,
        default_value = "nle.json",
        value_name = "FILE"
    )]
    project: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Create an empty project
    Init {
        /// Overwrite an existing project file
        #[arg(long)]
        force: bool,
    },
    /// Probe files and add them to the media pool
    Import {
        /// Media files to import
        files: Vec<PathBuf>,
    },
    /// Place a clip on the video track
    Add {
        /// Media file (auto-imported if new)
        file: PathBuf,
        /// Track number to place on (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Timeline position (default: end of track)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: Option<String>,
        /// Source in-point (default: 0)
        #[arg(long = "in", value_name = "TIME", allow_negative_numbers = true)]
        in_point: Option<String>,
        /// Source out-point (default: end of file)
        #[arg(long = "out", value_name = "TIME", allow_negative_numbers = true)]
        out_point: Option<String>,
    },
    /// Remove a clip by id (see `nle show`); `--ripple` closes the gap
    Remove {
        /// Clip id
        id: u32,
        /// Ripple: followers slide left instead of leaving a gap
        #[arg(long)]
        ripple: bool,
    },
    /// Change a clip's playback speed (the track ripples to fit)
    Speed {
        /// Clip id (from `nle show`)
        id: u32,
        /// New speed: 1, 2, 0.5, 0.8, 24/25 … (a rate, not a duration)
        #[arg(allow_negative_numbers = true)]
        rate: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Move a segment's in-edge to a position (TUI `i`; `--ripple` = `I`)
    TrimIn {
        /// Where the new edge lands (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: followers slide left to close the trimmed span
        #[arg(long)]
        ripple: bool,
    },
    /// Move a segment's out-edge to a position (TUI `o` / `O`)
    TrimOut {
        /// Where the new edge lands (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: followers slide left to close the trimmed span
        #[arg(long)]
        ripple: bool,
    },
    /// Split the segment under a position (TUI `s`)
    Split {
        /// Where to cut (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Insert a gap at a position (TUI `_`)
    Gap {
        /// Where to insert (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Gap length in frames (default 1)
        #[arg(long, allow_negative_numbers = true)]
        frames: Option<i64>,
        /// Gap length as a time (alternative to --frames)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        dur: Option<String>,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Delete a segment (TUI `dd`; `--ripple` = `D`)
    Delete {
        /// Delete the segment under this position
        #[arg(
            long,
            value_name = "TIME",
            required_unless_present_any = ["clip", "seg"]
        , allow_negative_numbers = true)]
        at: Option<String>,
        /// Delete this clip by id (finds its track)
        #[arg(long)]
        clip: Option<u32>,
        /// Delete this segment by index (from `nle show`)
        #[arg(long)]
        seg: Option<usize>,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: followers slide left instead of leaving a gap
        #[arg(long)]
        ripple: bool,
    },
    /// Yank a segment into the shared register (TUI `yy`)
    Yank {
        /// Yank the segment under this position
        #[arg(
            long,
            value_name = "TIME",
            required_unless_present_any = ["clip", "seg"]
        , allow_negative_numbers = true)]
        at: Option<String>,
        /// Yank this clip by id (finds its track)
        #[arg(long)]
        clip: Option<u32>,
        /// Yank this segment by index (from `nle show`)
        #[arg(long)]
        seg: Option<usize>,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Paste the register at a position (TUI `p`; `--ripple` = `P`)
    Paste {
        /// Where to insert (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: insert and push followers right
        #[arg(long)]
        ripple: bool,
    },
    /// Move one edge of a clip or gap to a position (the extend commit)
    Extend {
        /// The clip whose edge to move (finds its track)
        #[arg(long, required_unless_present_any = ["seg", "at"])]
        clip: Option<u32>,
        /// The segment whose edge to move (index from `nle show`)
        #[arg(long)]
        seg: Option<usize>,
        /// The segment under this position (alternative to --clip/--seg)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: Option<String>,
        /// Which edge: in (head/left) or out (tail/right)
        #[arg(long, value_name = "EDGE")]
        edge: String,
        /// Where the edge lands (seconds or timecode; may be negative)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        to: String,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// Ripple: push followers aside instead of overwriting
        #[arg(long)]
        ripple: bool,
    },
    /// Undo the last edit (TUI `u`; works on TUI-made edits too)
    Undo {},
    /// Redo the last undone edit (TUI `Ctrl-r`)
    Redo {},
    /// Drop sub-frame stumps and snap a drifted timeline to the frame grid
    PurgeStumps {
        /// Only this track (1-based); default: every track
        #[arg(long, value_name = "N")]
        track: Option<usize>,
    },
    /// Show the timeline
    Show {
        /// Print the raw project JSON
        #[arg(long)]
        json: bool,
    },
    /// Show or move the stored playhead (shared with the TUI)
    Seek {
        /// Where to move it (seconds or timecode); omit to print it
        #[arg(value_name = "TIME", allow_negative_numbers = true)]
        at: Option<String>,
    },
    /// Media pool: list assets and their usage
    Media {
        #[command(subcommand)]
        action: MediaCmd,
    },
    /// Timeline tracks: list, add, mute, remove
    Track {
        #[command(subcommand)]
        action: TrackCmd,
    },
    /// Import a CMX3600 EDL, building a project from the listed media
    ImportEdl {
        /// The .edl file
        file: PathBuf,
        /// Directory of media to match reels/clip names against
        #[arg(long, value_name = "DIR")]
        media: PathBuf,
        /// Force the frame rate (e.g. 30000/1001, 29.97, 24)
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
        /// Source timecode that maps to file time zero
        #[arg(long, value_name = "TC", default_value = "00:00:00:00")]
        src_offset: String,
        /// Override a reel: REEL=PATH (repeatable)
        #[arg(long = "map", value_name = "REEL=PATH")]
        map: Vec<String>,
        /// Overwrite an existing project file
        #[arg(long)]
        force: bool,
    },
    /// Import an OpenTimelineIO (.otio) timeline — e.g. a Kdenlive export
    ImportOtio {
        /// The .otio file
        file: PathBuf,
        /// Extra directory of media to pool (for moved files)
        #[arg(long, value_name = "DIR")]
        media: Option<PathBuf>,
        /// Force the frame rate (e.g. 30000/1001, 29.97, 24)
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
        /// Remap a clip path: OLD=NEW (repeatable)
        #[arg(long = "remap", value_name = "OLD=NEW")]
        remap: Vec<String>,
        /// Overwrite an existing project file
        #[arg(long)]
        force: bool,
    },
    /// Detect the cuts in a rendered file and rebuild an editable timeline
    DetectEdits {
        /// The rendered/edited video to analyze
        file: PathBuf,
        /// Scene-change score in 0..1 (lower = more cuts)
        #[arg(long, default_value_t = 0.4)]
        threshold: f32,
        /// Ignore shots shorter than this (suppresses flashes)
        #[arg(
            long,
            value_name = "TIME",
            default_value = "0.5",
            allow_negative_numbers = true
        )]
        min_shot: String,
        /// Shortest black interval to treat as a gap
        #[arg(
            long,
            value_name = "TIME",
            default_value = "0.1",
            allow_negative_numbers = true
        )]
        black_min: String,
        /// Do not recover black intervals as gaps
        #[arg(long)]
        no_gaps: bool,
        /// Force the frame rate (e.g. 30000/1001, 29.97, 24)
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
        /// Rebuild video only, dropping the file's audio
        #[arg(long)]
        video_only: bool,
        /// Print the detected cut points; write no project
        #[arg(long)]
        list: bool,
        /// Append the detected clips to an existing project (at the end)
        #[arg(long)]
        append: bool,
        /// Video track to append to (1-based; default: first video track)
        #[arg(long, value_name = "N")]
        track: Option<usize>,
        /// Overwrite an existing project file
        #[arg(long)]
        force: bool,
    },
    /// Render the timeline to a file
    Export {
        /// Output file
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        /// Encode engine: cpu, vaapi, qsv
        #[arg(long, value_enum, default_value = "cpu")]
        encoder: EncoderArg,
        /// x264 quality, 0-51, lower = better [cpu only]
        #[arg(long, default_value_t = 18)]
        crf: u8,
        /// x264 preset: ultrafast..veryslow [cpu only]
        #[arg(long, default_value = "fast")]
        preset: String,
        /// Target bitrate for hardware encoders [vaapi/qsv]
        #[arg(long, default_value = "8M")]
        bitrate: String,
    },
    /// Interactive editor: TUI dashboard + mpv monitor, vim keys
    #[cfg(feature = "tui")]
    Edit {},

    /// Print the mpv preview URL for the timeline (debugging)
    Edl {
        /// Track number to preview (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },

    /// Show what's inside a media file (streams, codecs, rates)
    Probe {
        /// Media file to inspect
        file: PathBuf,
        /// Print raw ffprobe JSON instead of a summary
        #[arg(long)]
        json: bool,
    },
    /// Cut [in, out) out of a file into a new file
    Trim {
        /// Source file
        input: PathBuf,
        /// Start: seconds ("12.5") or timecode ("0:01:23.500")
        #[arg(long = "in", value_name = "TIME", allow_negative_numbers = true)]
        in_point: String,
        /// End (same formats, exclusive)
        #[arg(long = "out", value_name = "TIME", allow_negative_numbers = true)]
        out_point: String,
        /// Where to write the result
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        /// Encode engine: cpu (libx264), vaapi (Intel GPU), qsv (Quick Sync)
        #[arg(long, value_enum, default_value = "cpu")]
        encoder: EncoderArg,
        /// Stream copy: instant + lossless; starts exactly at --in, end lands
        /// within a frame or two of --out (packet boundaries)
        #[arg(long)]
        copy: bool,
        /// x264 quality, 0-51, lower = better [cpu only]
        #[arg(long, default_value_t = 18)]
        crf: u8,
        /// x264 preset: ultrafast..veryslow [cpu only]
        #[arg(long, default_value = "fast")]
        preset: String,
        /// Target bitrate for hardware encoders [vaapi/qsv]
        #[arg(long, default_value = "8M")]
        bitrate: String,
    },
}

/// Media-pool subcommands.
#[derive(Debug, Subcommand)]
enum MediaCmd {
    /// List pooled assets and how many clips use each
    Ls,
    /// Remove an unused asset from the pool
    Rm {
        /// Asset id (from `nle media ls`)
        id: u32,
    },
}

/// Track subcommands.
#[derive(Debug, Subcommand)]
enum TrackCmd {
    /// List tracks (index, kind, mute, segment count)
    Ls,
    /// Append a new track
    Add {
        /// Track kind
        #[arg(value_enum)]
        kind: TrackKindArg,
    },
    /// Toggle a track's mute flag (default: track 1)
    Mute {
        /// Track index (1-based, from `nle track ls`)
        index: Option<usize>,
    },
    /// Remove a track by index (1-based), deleting its clips
    Rm {
        /// Track index (1-based)
        index: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TrackKindArg {
    Video,
    Audio,
}

/// CLI spelling of the encoder choices (clap needs ValueEnum to build
/// an argument parser from it).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncoderArg {
    Cpu,
    Vaapi,
    Qsv,
}

impl From<EncoderArg> for Encoder {
    fn from(v: EncoderArg) -> Self {
        match v {
            EncoderArg::Cpu => Encoder::Cpu,
            EncoderArg::Vaapi => Encoder::Vaapi,
            EncoderArg::Qsv => Encoder::Qsv,
        }
    }
}

// ------------------------------------------------------------ action mapping

/// A CLI time: seconds or timecode, optionally negative (an extend target
/// can reach back past time 0).
fn cli_time(s: &str) -> Result<f64, String> {
    parse_signed_time(s)
        .map(|t| *t.numer() as f64 / *t.denom() as f64)
        .map_err(|e| format!("invalid time {s:?} ({e})"))
}

/// The user-facing text for a model error. Track numbers are 1-based in
/// the CLI and the TUI (`v1`, `a1`), so the model's 0-based index is
/// translated here — one place, so the two front-ends agree.
pub(crate) fn edit_message(e: &edit::EditError) -> String {
    match e {
        edit::EditError::TrackNotFound(n) => format!("no track #{}", n + 1),
        other => other.to_string(),
    }
}

/// A 1-based CLI index (`--track N`, `nle track rm N`) → 0-based model
/// index. Track numbers are 1-based everywhere, matching `v1`/`a1`.
fn cli_index(n: usize, what: &str) -> Result<usize, String> {
    if n >= 1 {
        Ok(n - 1)
    } else {
        Err(format!("{what} is 1-based, got {n}"))
    }
}

/// `--at`/`--clip`/`--seg`: exactly one way to address a segment.
fn cli_target(
    at: &Option<String>,
    clip: Option<u32>,
    seg: Option<usize>,
) -> Result<Target, String> {
    match (at, clip, seg) {
        (Some(_), None, None) => cli_time(at.as_deref().unwrap_or_default()).map(Target::At),
        (None, Some(c), None) => Ok(Target::Clip(c)),
        (None, None, Some(s)) => Ok(Target::Segment(s)),
        (None, None, None) => Err("pass --at T, --clip N, or --seg N".to_string()),
        _ => Err("use only one of --at/--clip/--seg".to_string()),
    }
}

/// `--edge in|out` (head/tail aliases accepted).
fn cli_edge(s: &str) -> Result<Edge, String> {
    match s {
        "in" | "head" => Ok(Edge::In),
        "out" | "tail" => Ok(Edge::Out),
        other => Err(format!("--edge expects in|out, got {other:?}")),
    }
}

/// The edit-shaped CLI commands as catalog [`Action`]s — the single
/// mapping behind both `main`'s dispatch and the CLI-parity test, so a
/// command can't exist without its action. `None` means "not an action"
/// (probe, export, imports, queries); `Some(Err)` is an argument problem.
///
/// `nle add` is the one exception: it is a pool write + paste in one
/// step, so it builds its `Paste` action after probing.
fn to_action(cmd: &Cmd) -> Option<Result<Action, String>> {
    match cmd {
        Cmd::TrimIn { at, track, ripple } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_time(at).map(|at| Action::TrimIn {
                track,
                at,
                ripple: *ripple,
            })
        })),
        Cmd::TrimOut { at, track, ripple } => {
            Some(cli_index(*track, "--track").and_then(|track| {
                cli_time(at).map(|at| Action::TrimOut {
                    track,
                    at,
                    ripple: *ripple,
                })
            }))
        }
        Cmd::Split { at, track } => Some(
            cli_index(*track, "--track")
                .and_then(|track| cli_time(at).map(|at| Action::Split { track, at })),
        ),
        Cmd::Gap {
            at,
            frames,
            dur,
            track,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            let at = cli_time(at)?;
            let len = match (frames, dur) {
                (Some(_), Some(_)) => return Err("use either --frames or --dur".to_string()),
                (Some(n), None) => GapLen::Frames(*n),
                (None, Some(d)) => cli_time(d).map(GapLen::Seconds)?,
                (None, None) => GapLen::Frames(1),
            };
            match len {
                GapLen::Frames(n) if n < 1 => {
                    return Err("a gap needs at least one frame".to_string());
                }
                GapLen::Seconds(s) if s <= 0.0 => {
                    return Err("a gap needs a positive length".to_string());
                }
                _ => {}
            }
            Ok(Action::InsertGap { track, at, len })
        })),
        Cmd::Delete {
            at,
            clip,
            seg,
            track,
            ripple,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_target(at, *clip, *seg).map(|target| Action::Delete {
                track,
                target,
                ripple: *ripple,
            })
        })),
        Cmd::Yank {
            at,
            clip,
            seg,
            track,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_target(at, *clip, *seg).map(|target| Action::Yank { track, target })
        })),
        Cmd::Paste { at, track, ripple } => Some(cli_index(*track, "--track").and_then(|track| {
            cli_time(at).map(|at| Action::Paste {
                track,
                at,
                ripple: *ripple,
                segment: None,
            })
        })),
        Cmd::Extend {
            clip,
            seg,
            at,
            edge,
            to,
            track,
            ripple,
        } => Some(cli_index(*track, "--track").and_then(|track| {
            let target = cli_target(at, *clip, *seg)?;
            let edge = cli_edge(edge)?;
            let to = cli_time(to)?;
            Ok(Action::Extend {
                track,
                target,
                edge,
                to,
                ripple: *ripple,
            })
        })),
        Cmd::Remove { id, ripple } => Some(Ok(Action::Delete {
            track: 0,
            target: Target::Clip(*id),
            ripple: *ripple,
        })),
        Cmd::Speed { id, rate, track } => Some(cli_index(*track, "--track").and_then(|track| {
            parse_speed_arg(rate)
                .map_err(|e| e.to_string())
                .map(|rate| Action::SetSpeed {
                    track,
                    clip: *id,
                    rate,
                })
        })),
        Cmd::PurgeStumps { track } => Some(
            track
                .map(|n| cli_index(n, "--track"))
                .transpose()
                .map(|track| Action::PurgeStumps { track }),
        ),
        Cmd::Undo {} => Some(Ok(Action::Undo)),
        Cmd::Redo {} => Some(Ok(Action::Redo)),
        Cmd::Track { action } => match action {
            TrackCmd::Ls => None,
            TrackCmd::Add { kind } => Some(Ok(Action::TrackAdd {
                kind: match kind {
                    TrackKindArg::Video => TrackKind::Video,
                    TrackKindArg::Audio => TrackKind::Audio,
                },
            })),
            TrackCmd::Mute { index } => Some(
                index
                    .map(|n| cli_index(n, "track index"))
                    .transpose()
                    .map(|track| Action::TrackMute {
                        track: track.unwrap_or(0),
                    }),
            ),
            TrackCmd::Rm { index } => {
                Some(cli_index(*index, "track index").map(|track| Action::TrackRm { track }))
            }
        },
        Cmd::Media { action } => match action {
            MediaCmd::Ls => None,
            MediaCmd::Rm { id } => Some(Ok(Action::AssetRm { id: *id })),
        },
        _ => None,
    }
}

/// Run one catalog action against the project file: load, apply through
/// the undo history, persist project + sidecar + register. A CLI edit is
/// undoable (`nle undo`) exactly like a TUI edit — same executor, same
/// history.
fn run_cli_action(project_path: &Path, action: &Action) -> Result<()> {
    let mut project = Project::load(project_path)?;
    let mut history =
        edit::History::load(&edit::History::sidecar_path(project_path)).unwrap_or_default();
    let mut register = load_register(project_path);
    // The edit is "made from" its own position, so undoing it returns
    // there (the TUI passes the playhead instead — its session state).
    let from = match action {
        Action::TrimIn { at, .. }
        | Action::TrimOut { at, .. }
        | Action::Split { at, .. }
        | Action::InsertGap { at, .. }
        | Action::Paste { at, .. } => *at,
        Action::Delete { target, .. } | Action::Yank { target, .. } => match target {
            Target::At(t) => *t,
            _ => 0.0,
        },
        _ => 0.0,
    };
    let outcome = run_action(&mut project, &mut history, &mut register, action, from)
        .map_err(|e| anyhow::Error::msg(edit_message(&e)))?;
    project.save(project_path)?;
    history.save(&edit::History::sidecar_path(project_path))?;
    save_register(project_path, register.as_ref())?;
    log::line(&format!("action {}", outcome.desc));
    match outcome.playhead {
        Some(f) => say!("✓ {} (playhead f{f})", outcome.desc),
        None => say!("✓ {}", outcome.desc),
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Process-bound commands and queries dispatch here; every edit-shaped
    // command falls through to the action catalog below — the same
    // executor (and undo history) the TUI uses.
    match &cli.command {
        #[cfg(feature = "tui")]
        Cmd::Edit {} => return cmd_edit(&cli.project),
        Cmd::Edl { track } => return cmd_edl(&cli.project, *track),
        Cmd::Init { force } => return cmd_init(&cli.project, *force),
        Cmd::Import { files } => return cmd_import(&cli.project, files),
        Cmd::Add {
            file,
            track,
            at,
            in_point,
            out_point,
        } => {
            return cmd_add(
                &cli.project,
                file,
                *track,
                at.as_deref(),
                in_point.as_deref(),
                out_point.as_deref(),
            );
        }
        Cmd::Show { json } => return cmd_show(&cli.project, *json),
        Cmd::Seek { at } => return cmd_seek(&cli.project, at.as_deref()),
        Cmd::Track {
            action: TrackCmd::Ls,
        } => return cmd_track_ls(&cli.project),
        Cmd::Media {
            action: MediaCmd::Ls,
        } => return cmd_media_ls(&cli.project),
        Cmd::ImportEdl {
            file,
            media,
            fps,
            src_offset,
            map,
            force,
        } => {
            return cmd_import_edl(
                &cli.project,
                file,
                media,
                fps.as_deref(),
                src_offset,
                map,
                *force,
            );
        }
        Cmd::ImportOtio {
            file,
            media,
            fps,
            remap,
            force,
        } => {
            return cmd_import_otio(
                &cli.project,
                file,
                media.as_deref(),
                fps.as_deref(),
                remap,
                *force,
            );
        }
        Cmd::DetectEdits {
            file,
            threshold,
            min_shot,
            black_min,
            no_gaps,
            fps,
            video_only,
            list,
            append,
            track,
            force,
        } => {
            return cmd_detect_edits(DetectArgs {
                project: &cli.project,
                file: file.as_path(),
                threshold: *threshold,
                min_shot: min_shot.as_str(),
                black_min: black_min.as_str(),
                no_gaps: *no_gaps,
                fps: fps.as_deref(),
                video_only: *video_only,
                list: *list,
                append: *append,
                track: *track,
                force: *force,
            });
        }
        Cmd::Export {
            output,
            encoder,
            crf,
            preset,
            bitrate,
        } => {
            return cmd_export(
                &cli.project,
                output,
                (*encoder).into(),
                *crf,
                preset,
                bitrate,
            );
        }
        Cmd::Probe { file, json } => return cmd_probe(file, *json),
        Cmd::Trim {
            input,
            in_point,
            out_point,
            output,
            encoder,
            copy,
            crf,
            preset,
            bitrate,
        } => {
            return cmd_trim(
                input,
                in_point,
                out_point,
                output,
                (*encoder).into(),
                *copy,
                *crf,
                preset,
                bitrate,
            );
        }
        _ => {}
    }
    // Everything else is an edit: run it through the catalog.
    let action =
        to_action(&cli.command).ok_or_else(|| anyhow::anyhow!("internal: not an action"))?;
    run_cli_action(&cli.project, &action.map_err(anyhow::Error::msg)?)
}

// ------------------------------------------------------------ project cmds

#[cfg(feature = "tui")]
fn cmd_edit(project_path: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    // The monitor plays the composite, so any video track with a clip counts.
    let has_clip = project
        .timeline
        .video_tracks()
        .any(|t| t.segments.iter().any(|s| matches!(s, Segment::Clip(_))));
    if !has_clip {
        bail!("timeline is empty — add clips first (the monitor needs content)");
    }
    let media_title = format!(
        "nle — {}",
        project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_path.display().to_string())
    );
    let player = mpv::MpvPlayer::spawn(
        &format!("nle monitor — {}", project_path.display()),
        &media_title,
    )?;
    player.observe()?;
    let edl = preview::build_composite_edl(&project)?;
    player.load(&edl, Some(secs_f64(&project.playhead)));
    tui::run(project, project_path.to_path_buf(), player)?;
    Ok(())
}

fn cmd_edl(project_path: &Path, track: usize) -> Result<()> {
    // 1-based, and 0 is not a track number (it used to mean "track 1").
    let ti = cli_index(track, "--track").map_err(anyhow::Error::msg)?;
    let project = Project::load(project_path)?;
    if project.timeline.track(ti).is_none() {
        bail!("no track #{track}");
    }
    say!("{}", preview::build_edl(&project, ti)?);
    Ok(())
}

fn cmd_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    save_and_reset_history(&Project::new(), path)?;
    say!("✓ created {}", path.display());
    Ok(())
}

/// Save the project after an out-of-band write and drop the undo sidecar: a
/// CLI write re-baselines the project, so any persisted stack no longer
/// matches it (the next TUI session starts fresh rather than refusing every
/// undo). See AGENTS.md "Compatibility: none, by design".
fn save_and_reset_history(project: &Project, path: &Path) -> Result<()> {
    project.save(path)?;
    let _ = std::fs::remove_file(edit::History::sidecar_path(path));
    log::line(&format!("saved {} (undo history reset)", path.display()));
    Ok(())
}

fn cmd_import(project_path: &Path, files: &[PathBuf]) -> Result<()> {
    let mut project = Project::load(project_path)?;
    let mut history =
        edit::History::load(&edit::History::sidecar_path(project_path)).unwrap_or_default();
    let playhead = frame_of(project.fps.as_ref(), secs_f64(&project.playhead));
    let mut added = 0usize;
    for file in files {
        let report = probe(file).with_context(|| format!("importing {}", file.display()))?;
        let canonical = file
            .canonicalize()
            .with_context(|| format!("resolving {}", file.display()))?;
        if let Some(a) = project.asset_by_path(&canonical) {
            say!("· {} already imported (asset #{})", file.display(), a.id);
            continue;
        }
        // Recorded: `nle undo` removes the asset again.
        let (id, _) = history.add_asset(&mut project, canonical, report, playhead);
        added += 1;
        let asset = project.asset(id).context("asset vanished")?;
        say!(
            "✓ asset #{id}  {}  {}",
            file.display(),
            asset_summary(asset)
        );
    }
    if added > 0 {
        project.save(project_path)?;
        history.save(&edit::History::sidecar_path(project_path))?;
    }
    let clip_count = |p: &Project| {
        p.timeline
            .tracks
            .iter()
            .flat_map(|t| t.segments.iter())
            .filter(|s| matches!(s, Segment::Clip(_)))
            .count()
    };
    say!(
        "· {} — {} assets, {} clip(s)",
        project_path.display(),
        project.media.len(),
        clip_count(&project)
    );
    Ok(())
}

fn cmd_add(
    project_path: &Path,
    file: &Path,
    track: usize,
    at: Option<&str>,
    in_point: Option<&str>,
    out_point: Option<&str>,
) -> Result<()> {
    let mut project = Project::load(project_path)?;
    // `--track` is 1-based (matching the `v1`/`a1` labels); 0 internally.
    // 0 is not a track number — it used to silently mean "track 1".
    let ti = cli_index(track, "--track").map_err(anyhow::Error::msg)?;
    if ti >= project.timeline.tracks.len() {
        bail!("no track #{track}");
    }
    let mut history =
        edit::History::load(&edit::History::sidecar_path(project_path)).unwrap_or_default();
    let playhead = frame_of(project.fps.as_ref(), secs_f64(&project.playhead));
    let kind = project
        .timeline
        .track(ti)
        .map(|t| t.kind)
        .with_context(|| format!("no track #{track}"))?;
    let (scratch, imported) = stage_clip(
        &mut project,
        &mut history,
        file,
        in_point,
        out_point,
        playhead,
        kind,
    )?;
    if imported && let Segment::Clip(c) = &scratch {
        say!("· auto-imported as asset #{}", c.asset);
    }

    // Ripple paste: inserts at --at (splits clips/gaps) or appends.
    let append_at = match at {
        Some(s) => parse_time(s).with_context(|| format!("invalid --at time: {s:?}"))?,
        None => project
            .timeline
            .track(ti)
            .map(|t| t.end())
            .unwrap_or_else(|| Seconds::from_integer(0)),
    };
    // The pool/settings writes land first, with their history: a recorded
    // pool addition (undo removes the asset again) plus the first-clip
    // settings. The insert itself goes through the catalog below — the
    // same `Paste` the TUI's pool insert runs.
    project.save(project_path)?;
    if imported {
        history.save(&edit::History::sidecar_path(project_path))?;
    }
    run_cli_action(
        project_path,
        &Action::Paste {
            track: ti,
            at: secs_f64(&append_at),
            ripple: true,
            segment: Some(scratch),
        },
    )
}

/// Probe `file` into the pool if it is new, validate the source range,
/// and stage the clip to insert — shared by `nle add` and the TUI's
/// `:add`. Returns the clip and whether the pool was written (a recorded
/// pool addition, so it is undoable). The first clip defines the sequence
/// settings, like a real NLE.
fn stage_clip(
    project: &mut Project,
    history: &mut edit::History,
    file: &Path,
    in_point: Option<&str>,
    out_point: Option<&str>,
    playhead: i64,
    kind: TrackKind,
) -> Result<(Segment, bool)> {
    let canonical = file
        .canonicalize()
        .with_context(|| format!("{} not found", file.display()))?;

    let existing = project.asset_by_path(&canonical).map(|a| a.id);
    let (asset_id, imported) = match existing {
        Some(id) => {
            // Refuse a kind mismatch *before* touching the pool, so the
            // error isn't preceded by a successful import.
            if let Some(asset) = project.asset(id)
                && let Some(reason) = asset.wrong_kind(kind)
            {
                bail!("{reason}");
            }
            (id, false)
        }
        None => {
            let report = probe(file).with_context(|| format!("importing {}", file.display()))?;
            if let Some(reason) = wrong_kind(&canonical, &report, kind) {
                bail!("{reason}");
            }
            let (id, _) = history.add_asset(project, canonical, report, playhead);
            (id, true)
        }
    };

    let in_t = match in_point {
        Some(s) => parse_time(s).with_context(|| format!("invalid --in time: {s:?}"))?,
        None => Seconds::from_integer(0),
    };
    let out_t = match out_point {
        Some(s) => parse_time(s).with_context(|| format!("invalid --out time: {s:?}"))?,
        None => project
            .asset(asset_id)
            .and_then(|a| a.duration())
            .ok_or_else(|| anyhow::anyhow!("asset has unknown duration — pass --out"))?,
    };

    let asset = project
        .asset(asset_id)
        .context("asset vanished after import")?;
    if in_t >= out_t {
        bail!("--in must be before --out");
    }
    if let Some(d) = asset.duration()
        && out_t > d
    {
        bail!("--out {out_point:?} is beyond the asset's duration {d}");
    }
    // The first clip defines the sequence settings (like a real NLE).
    let asset_fps = asset.fps();
    let asset_size = asset.video_size();
    if project.fps.is_none() {
        project.fps = asset_fps;
    }
    if project.video.is_none() {
        project.video = asset_size.map(|(w, h)| VideoSettings {
            width: w,
            height: h,
        });
    }

    let scratch = Segment::Clip(Clip {
        id: 0,
        asset: asset_id,
        asset_in: in_t,
        asset_out: out_t,
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    Ok((scratch, imported))
}

fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

fn cmd_show(project_path: &Path, json: bool) -> Result<()> {
    let project = Project::load(project_path)?;
    if json {
        say!("{}", serde_json::to_string_pretty(&project)?);
        return Ok(());
    }
    say!(
        "{} — {}",
        project_path.display(),
        show_report(&project).trim_end()
    );
    Ok(())
}

/// `nle seek [TIME]` — move the stored playhead (snapped to the frame grid,
/// clamped into the timeline) or print it. The TUI opens where it lands.
fn cmd_seek(project_path: &Path, at: Option<&str>) -> Result<()> {
    let mut project = Project::load(project_path)?;
    let Some(s) = at else {
        say!("{}", format_seconds(&project.playhead));
        return Ok(());
    };
    let t = parse_signed_time(s).with_context(|| format!("invalid time {s:?}"))?;
    let landed = project.set_playhead(secs_f64(&t));
    project.save(project_path)?;
    log::line(&format!("seek -> {landed} s"));
    let frame = frame_of(project.fps.as_ref(), landed);
    match project.fps {
        Some(_) => say!(
            "✓ playhead → {} s (f{frame})",
            format_seconds(&project.playhead)
        ),
        None => say!("✓ playhead → {} s", format_seconds(&project.playhead)),
    }
    Ok(())
}

/// The human timeline report behind `nle show` (and the TUI's `:show`):
/// a summary line, then one line per segment (`[i]` is the segment index
/// `--seg` addresses). A trailing newline.
pub(crate) fn show_report(project: &Project) -> String {
    let clip_count: usize = project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .filter(|s| matches!(s, Segment::Clip(_)))
        .count();

    let mut out = format!(
        "{} clip(s) · {} s",
        clip_count,
        format_seconds(&project.timeline.end())
    );
    if let Some(fps) = &project.fps {
        out += &format!(" · {}/{} fps", fps.numer(), fps.denom());
    }
    if let Some(v) = &project.video {
        out += &format!(" · {}×{}", v.width, v.height);
    }
    out += &format!(" · at {} s", format_seconds(&project.playhead));
    out.push('\n');

    for (ti, track) in project.timeline.tracks.iter().enumerate() {
        let mute = if track.muted { " (muted)" } else { "" };
        out += &format!("  {}{mute}\n", track_label(project, ti));

        let mut cursor = Seconds::from_integer(0);
        for (si, seg) in track.segments.iter().enumerate() {
            match seg {
                Segment::Transition(t) => {
                    let end = cursor + t.dur;
                    out += &format!(
                        "      [{si}] ⟶ transition #{} {}–{}\n",
                        t.id,
                        format_seconds(&cursor),
                        format_seconds(&end)
                    );
                    cursor = end;
                }
                Segment::Gap(g) => {
                    let end = cursor + g.duration;
                    out += &format!(
                        "      [{si}] ┄ gap {}–{}\n",
                        format_seconds(&cursor),
                        format_seconds(&end)
                    );
                    cursor = end;
                }
                Segment::Clip(clip) => {
                    let asset = project
                        .asset(clip.asset)
                        .map(|a| a.path.display().to_string())
                        .unwrap_or_else(|| "?".into());
                    let start = cursor;
                    let end = start + clip.duration();
                    let speed = if clip.speed != Seconds::from_integer(1) {
                        format!(" @{}×", format_seconds(&clip.speed))
                    } else {
                        String::new()
                    };
                    out += &format!(
                        "    [{si}] #{:<4}{:>9} – {:<9} {}{speed}  (src {}–{})\n",
                        clip.id,
                        format_seconds(&start),
                        format_seconds(&end),
                        asset,
                        format_seconds(&clip.asset_in),
                        format_seconds(&clip.asset_out)
                    );
                    cursor = end;
                }
            }
        }
    }
    out
}

/// `nle media ls` — the pool listing. (`nle media rm` is a catalog
/// action; `main` routes it there, undoably.)
fn cmd_media_ls(project_path: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    if project.media.is_empty() {
        say!("media pool is empty — `nle import <file>`");
        return Ok(());
    }
    say!(
        "{} — {} asset(s)",
        project_path.display(),
        project.media.len()
    );
    for a in &project.media {
        let used = project.asset_usage(a.id);
        let usage = if used.is_empty() {
            "unused".to_string()
        } else {
            format!("used by {used:?}")
        };
        say!("  #{:<4} {:<9} {}", a.id, asset_summary(a), usage);
    }
    Ok(())
}

/// `nle track ls` — the track listing. (add/mute/rm are catalog actions.)
fn cmd_track_ls(project_path: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    say!(
        "{} — {} track(s)",
        project_path.display(),
        project.timeline.tracks.len()
    );
    for (i, t) in project.timeline.tracks.iter().enumerate() {
        let kind = match t.kind {
            TrackKind::Video => "video",
            TrackKind::Audio => "audio",
        };
        let mute = if t.muted { " (muted)" } else { "" };
        say!(
            "  {:<3} {:<6} {} segment(s){}",
            i + 1,
            kind,
            t.segments.len(),
            mute
        );
    }
    Ok(())
}

/// Media extensions probed when importing a `--media` directory.
const MEDIA_EXTS: &[&str] = &[
    "mp4", "mov", "mkv", "m4v", "avi", "webm", "wav", "mp3", "m4a", "aac", "flac", "ogg",
];

/// `--fps` accepts `n/d`, a decimal NTSC rate, or a whole number.
fn parse_fps_arg(s: &str) -> Result<Seconds> {
    if let Some(r) = parse_rate(s) {
        return Ok(r);
    }
    match s {
        "23.976" => Ok(Seconds::new(24000, 1001)),
        "29.97" => Ok(Seconds::new(30000, 1001)),
        "59.94" => Ok(Seconds::new(60000, 1001)),
        _ => {
            let n: i64 = s
                .parse()
                .with_context(|| format!("invalid --fps {s:?} (try 30000/1001, 29.97, or 24)"))?;
            Ok(Seconds::from_integer(n))
        }
    }
}

/// A speed accepts `n/d` or a positive decimal (`1`, `0.5`, `0.8`, `2`).
fn parse_speed_arg(s: &str) -> Result<Seconds> {
    if let Some(r) = parse_rate(s) {
        return Ok(r);
    }
    let v: f64 = s
        .parse()
        .with_context(|| format!("invalid speed {s:?} (try 1, 0.5, 2, or 24/25)"))?;
    if !v.is_finite() || v <= 0.0 {
        bail!("speed must be positive (got {s:?})");
    }
    Seconds::approximate_float(v)
        .filter(|r| *r > Seconds::from_integer(0))
        .with_context(|| format!("speed {s:?} is not representable"))
}

/// Match a reel/clip name to a pooled asset in three tiers — exact
/// filename, exact stem, then a case-insensitive substring at a stem
/// **edge** — and only when a tier binds **exactly one** asset. Two
/// constraints keep generic reels from silently binding an unrelated
/// file: `AX` (the placeholder for unnamed aux media) is a substring of
/// stems like `taxi` but aligns with none of their edges, and a tie (e.g.
/// `red_a` + `red_b` for needle `red`) stays unresolved so the caller can
/// report the real problem. The uniqueness rule applies to the exact
/// tiers too: with `/a/red.mp4` and `/b/red.mp4` pooled, a bare
/// `red.mp4` is ambiguous — first-match-in-pool-order is a decision, not
/// evidence.
fn match_asset(project: &Project, needle: &str) -> Option<u32> {
    let needle = needle.to_lowercase();
    let needle_stem = Path::new(&needle)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| needle.clone());
    let mut by_file: Vec<u32> = Vec::new();
    let mut by_stem: Vec<u32> = Vec::new();
    let mut fuzzy: Vec<u32> = Vec::new();
    for a in &project.media {
        let file = a
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let stem = a
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        // Filename is stronger evidence than stem: `red.mp4` must prefer
        // the `.mp4` even when a `red.mkv` is pooled alongside.
        if file == needle {
            by_file.push(a.id);
            continue;
        }
        if stem == needle || file == needle_stem || stem == needle_stem {
            by_stem.push(a.id);
            continue;
        }
        // An empty stem (or needle stem) matches everything — that is not
        // information, don't match on it.
        if !stem.is_empty() && !needle_stem.is_empty() && (fuzzy_edge(&stem, &needle_stem)) {
            fuzzy.push(a.id);
        }
    }
    unique(by_file)
        .or_else(|| unique(by_stem))
        .or_else(|| unique(fuzzy))
}

/// The one id in `ids`, or `None` when zero or several — an ambiguous
/// match is not a match.
fn unique(mut ids: Vec<u32>) -> Option<u32> {
    if ids.len() == 1 { ids.pop() } else { None }
}

/// Substring match at an edge: `red` matches `red_start` (as a prefix) and
/// `out_red` (as a suffix), but not inside `taxi` for needle `ax`.
fn fuzzy_edge(needle: &str, hay: &str) -> bool {
    hay.starts_with(needle)
        || hay.ends_with(needle)
        || needle.starts_with(hay)
        || needle.ends_with(hay)
}

/// Resolve an EDL event to an asset id: an explicit `--map` reel, then the
/// `SOURCE FILE`/`FROM CLIP NAME` filename, then the reel field.
fn resolve_reel(project: &Project, map: &HashMap<String, PathBuf>, ev: &EdlEvent) -> Option<u32> {
    if let Some(path) = map.get(&ev.reel)
        && let Some(a) = project.asset_by_path(path)
    {
        return Some(a.id);
    }
    for cand in [ev.source_file.as_deref(), ev.clip_name.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(id) = match_asset(project, cand) {
            return Some(id);
        }
    }
    match_asset(project, &ev.reel)
}

fn cmd_import_edl(
    project_path: &Path,
    edl_path: &Path,
    media_dir: &Path,
    fps_arg: Option<&str>,
    src_offset: &str,
    maps: &[String],
    force: bool,
) -> Result<()> {
    import_edl_work(
        project_path,
        edl_path,
        media_dir,
        fps_arg,
        src_offset,
        maps,
        force,
    )
    .map(|r| r.print())
}

/// The work behind `import-edl` (and the TUI's `:edl-import`).
fn import_edl_work(
    project_path: &Path,
    edl_path: &Path,
    media_dir: &Path,
    fps_arg: Option<&str>,
    src_offset: &str,
    maps: &[String],
    force: bool,
) -> Result<CommandReport> {
    if project_path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            project_path.display()
        );
    }
    let text = std::fs::read_to_string(edl_path)
        .with_context(|| format!("reading {}", edl_path.display()))?;
    let doc = edl::parse(&text).with_context(|| format!("parsing {}", edl_path.display()))?;

    // Probe + pool every media file, deduped by path.
    let mut project = Project::new();
    let add = |project: &mut Project, p: &Path| -> Result<PathBuf> {
        let canonical = p
            .canonicalize()
            .with_context(|| format!("{} not found", p.display()))?;
        if project.asset_by_path(&canonical).is_none() {
            let report = probe(&canonical)?;
            project.add_asset(canonical.clone(), report);
        }
        Ok(canonical)
    };

    let mut imported = 0usize;
    let mut notes: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(media_dir)
        .with_context(|| format!("reading media dir {}", media_dir.display()))?
    {
        let path = entry?.path();
        let is_media = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| MEDIA_EXTS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false);
        if path.is_file() && is_media {
            // A directory scan is a *candidate* list: one unreadable file
            // (a partial copy, a stray text file with a media extension)
            // must not sink the whole import — warn and keep looking.
            match add(&mut project, &path) {
                Ok(_) => imported += 1,
                Err(e) => notes.push(format!("warning: skipping {} — {e}", path.display())),
            }
        }
    }
    if imported == 0 {
        bail!("no media files found in {}", media_dir.display());
    }

    let mut map: HashMap<String, PathBuf> = HashMap::new();
    for m in maps {
        let (reel, path) = m
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--map expects REEL=PATH, got {m:?}"))?;
        let canonical = add(&mut project, Path::new(path))?;
        map.insert(reel.to_string(), canonical);
    }

    // Prefer the first video event's asset for settings; fall back to any.
    let (fps, size) = {
        let first_asset = doc
            .events
            .iter()
            .filter(|e| e.channel.starts_with('V'))
            .find_map(|e| resolve_reel(&project, &map, e))
            .and_then(|id| project.asset(id));
        let fps = match fps_arg {
            Some(s) => parse_fps_arg(s)?,
            None => first_asset
                .and_then(|a| a.fps())
                .or_else(|| project.media.iter().find_map(|a| a.fps()))
                .ok_or_else(|| anyhow::anyhow!("no frame rate known — pass --fps"))?,
        };
        let size = first_asset
            .and_then(|a| a.video_size())
            .or_else(|| project.media.iter().find_map(|a| a.video_size()));
        (fps, size)
    };
    project.fps = Some(fps);
    project.video = size.map(|(width, height)| VideoSettings { width, height });

    let offset_frames = timecode_to_frames(parse_timecode(src_offset)?, fps, doc.drop_frame);
    let timeline = edl::build(&doc, fps, offset_frames, &|ev| {
        resolve_reel(&project, &map, ev)
    })?;
    project.timeline = timeline;
    project.validate()?;
    save_and_reset_history(&project, project_path)?;
    let mut report = CommandReport::new(format!(
        "✓ {} — {} event(s) → {} track(s), {} asset(s)",
        project_path.display(),
        doc.events.len(),
        project.timeline.tracks.len(),
        project.media.len()
    ));
    report.warnings = notes;
    Ok(report)
}

/// Probe every media file in `dir` into the pool; returns how many files were
/// seen (whether or not they were already pooled).
fn pool_media_dir(project: &mut Project, dir: &Path) -> Result<(usize, Vec<String>)> {
    let mut seen = 0usize;
    let mut notes = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading media dir {}", dir.display()))?
    {
        let path = entry?.path();
        let is_media = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| MEDIA_EXTS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false);
        if !path.is_file() || !is_media {
            continue;
        }
        // A candidate list, not a promise: skip what can't be read.
        let Ok(canonical) = path.canonicalize() else {
            notes.push(format!("warning: skipping {} — not found", path.display()));
            continue;
        };
        if project.asset_by_path(&canonical).is_none() {
            match probe(&canonical) {
                Ok(report) => {
                    project.add_asset(canonical, report);
                }
                Err(e) => {
                    notes.push(format!("warning: skipping {} — {e}", path.display()));
                    continue;
                }
            }
        }
        seen += 1;
    }
    Ok((seen, notes))
}

/// Locate a clip's media: an explicit `--remap OLD=NEW`, then the path
/// itself (relative paths resolved against the `.otio` file), then a
/// same-named file already pooled from `--media`.
fn resolve_otio_path(
    project: &Project,
    raw: &Path,
    remap: &HashMap<String, PathBuf>,
    base: &Path,
) -> Result<PathBuf> {
    if let Some(new) = remap.get(&raw.to_string_lossy().into_owned()) {
        return Ok(new.clone());
    }
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    if candidate.exists() {
        return Ok(candidate);
    }
    if let Some(name) = raw.file_name() {
        let needle = name.to_string_lossy();
        for a in &project.media {
            if a.path
                .file_name()
                .map(|n| n.to_string_lossy() == needle)
                .unwrap_or(false)
            {
                return Ok(a.path.clone());
            }
        }
    }
    bail!(
        "{} not found (use --remap or --media to relocate it)",
        candidate.display()
    )
}

fn cmd_import_otio(
    project_path: &Path,
    otio_path: &Path,
    media_dir: Option<&Path>,
    fps_arg: Option<&str>,
    remaps: &[String],
    force: bool,
) -> Result<()> {
    import_otio_work(project_path, otio_path, media_dir, fps_arg, remaps, force).map(|r| r.print())
}

/// The work behind `import-otio` (and the TUI's `:otio-import`).
fn import_otio_work(
    project_path: &Path,
    otio_path: &Path,
    media_dir: Option<&Path>,
    fps_arg: Option<&str>,
    remaps: &[String],
    force: bool,
) -> Result<CommandReport> {
    if project_path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            project_path.display()
        );
    }
    let text = std::fs::read_to_string(otio_path)
        .with_context(|| format!("reading {}", otio_path.display()))?;
    let doc = otio::parse(&text).with_context(|| format!("parsing {}", otio_path.display()))?;

    let mut project = Project::new();
    let mut scan_notes = Vec::new();
    if let Some(dir) = media_dir {
        let (_, notes) = pool_media_dir(&mut project, dir)?;
        scan_notes = notes;
    }

    let remap: HashMap<String, PathBuf> = remaps
        .iter()
        .map(|m| {
            let (old, new) = m
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--remap expects OLD=NEW, got {m:?}"))?;
            Ok((old.to_string(), PathBuf::from(new)))
        })
        .collect::<Result<_>>()?;
    let base = otio_path.parent().unwrap_or_else(|| Path::new("."));

    // Resolve + probe every distinct clip path.
    let mut by_path: HashMap<PathBuf, u32> = HashMap::new();
    for track in &doc.tracks {
        for item in &track.items {
            let OtioItem::Clip(c) = item else { continue };
            if by_path.contains_key(&c.path) {
                continue;
            }
            let located = resolve_otio_path(&project, &c.path, &remap, base)
                .with_context(|| format!("clip {:?}", c.name))?;
            let canonical = located
                .canonicalize()
                .with_context(|| format!("clip {:?}: {} not found", c.name, located.display()))?;
            let report = probe(&canonical)?;
            let (id, _) = project.add_asset(canonical, report);
            by_path.insert(c.path.clone(), id);
        }
    }

    let fps = match fps_arg {
        Some(s) => parse_fps_arg(s)?,
        None => doc.fps,
    };
    project.fps = Some(fps);
    project.video = doc
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .flat_map(|t| &t.items)
        .find_map(|item| match item {
            OtioItem::Clip(c) => by_path
                .get(&c.path)
                .and_then(|id| project.asset(*id))
                .and_then(|a| a.video_size())
                .map(|(width, height)| VideoSettings { width, height }),
            OtioItem::Gap(_) => None,
        });

    let timeline = otio::build(&doc, &|p| by_path.get(p).copied())
        .with_context(|| format!("building timeline from {}", otio_path.display()))?;
    project.timeline = timeline;
    // Keep every clip inside its media and on the frame grid.
    let clamp_notes = clamp_project_to_media(&mut project);
    project.validate()?;
    save_and_reset_history(&project, project_path)?;

    let mut report = CommandReport::new(String::new());
    report.warnings = scan_notes;
    for w in doc.warnings.iter().chain(clamp_notes.iter()) {
        report.warn(format!("warning: {w}"));
    }
    let clips = project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .filter(|s| matches!(s, Segment::Clip(_)))
        .count();
    report.summary = format!(
        "✓ {} — {} track(s), {} asset(s), {} clip(s)",
        project_path.display(),
        project.timeline.tracks.len(),
        project.media.len(),
        clips
    );
    Ok(report)
}

/// Arguments for [`cmd_detect_edits`]. A parameter struct keeps the call
/// site readable and clippy's `too_many_arguments` quiet — the Rust
/// equivalent of "pass an options object" instead of a long positional list.
struct DetectArgs<'a> {
    project: &'a Path,
    file: &'a Path,
    threshold: f32,
    min_shot: &'a str,
    black_min: &'a str,
    no_gaps: bool,
    fps: Option<&'a str>,
    video_only: bool,
    list: bool,
    append: bool,
    track: Option<usize>,
    force: bool,
}

fn cmd_detect_edits(args: DetectArgs) -> Result<()> {
    detect_edits_work(args).map(|r| r.print())
}

/// The work behind `detect-edits` (and the TUI's `:detect`): warnings +
/// summary, no printing.
fn detect_edits_work(args: DetectArgs) -> Result<CommandReport> {
    let mut out = CommandReport::new(String::new());
    let DetectArgs {
        project: project_path,
        file,
        threshold,
        min_shot,
        black_min,
        no_gaps,
        fps: fps_arg,
        video_only,
        list,
        append,
        track,
        force,
    } = args;
    let min_shot =
        parse_time(min_shot).with_context(|| format!("invalid --min-shot {min_shot:?}"))?;
    let black_min =
        parse_time(black_min).with_context(|| format!("invalid --black-min {black_min:?}"))?;
    // `select='gt(scene,T)'`: T is a frame-difference score in 0..1. A typo
    // like `-1` or `40` otherwise yields a degenerate-but-"successful" run
    // (every frame a cut, or none).
    if !(threshold > 0.0 && threshold < 1.0) {
        bail!(
            "--threshold must be strictly between 0 and 1 (got {threshold}; the scene score is 0..1)"
        );
    }

    let canonical = file
        .canonicalize()
        .with_context(|| format!("{} not found", file.display()))?;
    let report = probe(&canonical).with_context(|| format!("probing {}", file.display()))?;
    let duration = report
        .duration()
        .context("could not determine the file's duration")?;
    let fps = match fps_arg {
        Some(s) => parse_fps_arg(s)?,
        None => report.fps().context("no frame rate known — pass --fps")?,
    };

    // `--append` targets an existing timeline: its frame grid is what cuts
    // must snap to. A 24 fps project can only cut on 24 fps frames, so a
    // 59.94 fps source snapped to the source grid would export with a
    // fraction of a frame of error per cut (and the playhead would step
    // off-grid). Load it up front so `reconstruct` uses the right grid.
    let mut append_project = if append {
        Some(Project::load(project_path).with_context(|| {
            format!(
                "--append needs an existing project at {}",
                project_path.display()
            )
        })?)
    } else {
        None
    };
    let snap_fps = snap_fps_for(append_project.as_ref(), fps);
    if append && snap_fps != fps {
        out.warn(format!(
            "note: snapping cuts to the project grid {}/{} fps (source is {}/{})",
            snap_fps.numer(),
            snap_fps.denom(),
            fps.numer(),
            fps.denom()
        ));
    }

    // The reconstruction references one asset, and export reads the first
    // stream of each kind — say so rather than silently dropping the rest.
    let count_of = |kind: &str| {
        report
            .streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some(kind))
            .count()
            .saturating_sub(1)
    };
    let extra_video = count_of("video");
    let extra_audio = count_of("audio");
    if extra_video > 0 {
        out.warn(format!(
            "warning: {extra_video} extra video stream(s) ignored — only the first is reconstructed"
        ));
    }
    if extra_audio > 0 {
        out.warn(format!(
            "warning: {extra_audio} extra audio stream(s) ignored — only the first is used"
        ));
    }

    let opts = DetectOptions {
        threshold,
        black_min,
        ..DetectOptions::default()
    };
    let detection = detect(&canonical, &opts)
        .with_context(|| format!("detecting cuts in {}", file.display()))?;
    let blacks: &[(Seconds, Seconds)] = if no_gaps { &[] } else { &detection.blacks };
    let spans = reconstruct(&detection.cuts, blacks, duration, snap_fps, min_shot);

    let clips = spans
        .iter()
        .filter(|s| matches!(s, Span::Media { .. }))
        .count();
    let gaps = spans
        .iter()
        .filter(|s| matches!(s, Span::Gap { .. }))
        .count();
    if list {
        let mut summary = format!(
            "{} cut(s), {gaps} gap(s), {} total",
            detection.cuts.len(),
            format_seconds(&duration)
        );
        for c in &detection.cuts {
            summary.push_str(&format!("\n  cut {}", format_seconds(c)));
        }
        out.summary = summary;
        return Ok(out);
    }

    let has_audio = report.has_audio();

    // --append: add the detected shots to an existing project, starting at
    // the timeline end. Pad each target track up to that common point so the
    // appended video and audio stay aligned.
    if append {
        let mut project = append_project
            .take()
            .context("--append project was not loaded")?;
        if project.fps.is_none() {
            project.fps = Some(fps);
        }
        if project.video.is_none() {
            project.video = report
                .size()
                .map(|(width, height)| VideoSettings { width, height });
        }
        let (asset_id, _) = project.add_asset(canonical, report);

        let start = project.timeline.end();
        let video_ti = match track {
            Some(n) => {
                let i = n.saturating_sub(1);
                let t = project
                    .timeline
                    .track(i)
                    .with_context(|| format!("no track #{n}"))?;
                if t.kind != TrackKind::Video {
                    bail!("track #{n} is not a video track");
                }
                i
            }
            None => project
                .timeline
                .tracks
                .iter()
                .position(|t| t.kind == TrackKind::Video)
                .context("project has no video track")?,
        };

        let mut next_id = next_clip_id(&project);
        let video_segments = span_segments(&spans, asset_id, &mut next_id);
        append_spans(&mut project, video_ti, start, video_segments);

        if has_audio && !video_only {
            let audio_ti = match project
                .timeline
                .tracks
                .iter()
                .position(|t| t.kind == TrackKind::Audio)
            {
                Some(i) => i,
                None => project.timeline.add_track(TrackKind::Audio),
            };
            let audio_segments = span_segments(&spans, asset_id, &mut next_id);
            append_spans(&mut project, audio_ti, start, audio_segments);
        }

        for n in clamp_project_to_media(&mut project) {
            out.warn(format!("warning: {n}"));
        }
        project.validate()?;
        save_and_reset_history(&project, project_path)?;
        out.summary = format!(
            "✓ {} — appended {} clip(s), {} gap(s) @ {}",
            project_path.display(),
            clips,
            gaps,
            format_seconds(&fps),
        );
        return Ok(out);
    }

    if project_path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite, or --append)",
            project_path.display()
        );
    }

    let mut project = Project::new();
    let (asset_id, _) = project.add_asset(canonical, report);
    project.fps = Some(fps);
    project.video = project
        .asset(asset_id)
        .and_then(|a| a.video_size())
        .map(|(width, height)| VideoSettings { width, height });

    // Both tracks follow the SAME spans: in the rendered file a gap is black
    // + silence, and splitting audio at the video cuts keeps each shot a
    // movable pair. (No a/v linking yet, so reordering only the video still
    // desyncs — a model limit, not a detector bug.)
    let mut next_id = 1u32;
    let mut tracks = vec![Track {
        kind: TrackKind::Video,
        muted: false,
        segments: span_segments(&spans, asset_id, &mut next_id),
    }];
    if has_audio && !video_only {
        tracks.push(Track {
            kind: TrackKind::Audio,
            muted: false,
            segments: span_segments(&spans, asset_id, &mut next_id),
        });
    }
    project.timeline = Timeline { tracks };
    for n in clamp_project_to_media(&mut project) {
        out.warn(format!("warning: {n}"));
    }
    project.validate()?;
    save_and_reset_history(&project, project_path)?;

    out.summary = format!(
        "✓ {} — {} cut(s) → {} clip(s), {} gap(s) @ {}",
        project_path.display(),
        detection.cuts.len(),
        clips,
        gaps,
        format_seconds(&fps),
    );
    Ok(out)
}

/// Keep every clip inside its media and on the frame grid (see
/// [`edit::clamp_to_media`]). Returns one note per adjustment; applied by both
/// import paths and by `purge-stumps`, so no write path leaves a clip reading
/// past EOF or ending off the project's frame grid.
fn clamp_project_to_media(project: &mut Project) -> Vec<String> {
    let Some(fps) = project.fps else {
        return Vec::new();
    };
    let durations: HashMap<u32, Seconds> = project
        .media
        .iter()
        .filter_map(|a| a.duration().map(|d| (a.id, d)))
        .collect();
    let media = |id: u32| durations.get(&id).copied();
    project
        .timeline
        .tracks
        .iter_mut()
        .flat_map(|t| edit::clamp_to_media(t, &media, fps))
        .collect()
}

/// Build the clip/gap segments for a run of spans, allocating clip ids from
/// `next_id` (unique across tracks).
fn span_segments(spans: &[Span], asset: u32, next_id: &mut u32) -> Vec<Segment> {
    let mut segments = Vec::with_capacity(spans.len());
    for span in spans {
        match span {
            Span::Gap { duration } => segments.push(Segment::Gap(Gap {
                duration: *duration,
            })),
            Span::Media { start, end } => {
                segments.push(Segment::Clip(Clip::new(*next_id, asset, *start, *end)));
                *next_id += 1;
            }
        }
    }
    segments
}

/// Append `segments` to track `ti`, padding with a gap up to `start` so the
/// appended block begins at a common point on every track (keeps a/v aligned).
/// A padding gap coalesces with a leading gap in the run, so we never leave
/// two adjacent `Gap` segments.
fn append_spans(project: &mut Project, ti: usize, start: Seconds, mut segments: Vec<Segment>) {
    let Some(track) = project.timeline.track_mut(ti) else {
        return;
    };
    let end = track.end();
    if end < start {
        track.segments.push(Segment::Gap(Gap {
            duration: start - end,
        }));
    }
    if matches!(segments.first(), Some(Segment::Gap(_)))
        && let Some(Segment::Gap(prev)) = track.segments.last_mut()
        && let Segment::Gap(first) = segments.remove(0)
    {
        prev.duration += first.duration;
    }
    track.segments.extend(segments);
}

/// The frame grid to snap reconstructed cuts to: an append target's own fps
/// (the timeline can only cut on its frames), else the source's.
fn snap_fps_for(append_project: Option<&Project>, file_fps: Seconds) -> Seconds {
    append_project.and_then(|p| p.fps).unwrap_or(file_fps)
}

/// The next free clip id: one past the highest in use. Mirrors
/// `Project::next_clip_id`, which is crate-private (this is a separate crate).
fn next_clip_id(project: &Project) -> u32 {
    project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| &t.segments)
        .filter_map(|s| match s {
            Segment::Clip(c) => Some(c.id),
            Segment::Transition(t) => Some(t.id),
            Segment::Gap(_) => None,
        })
        .max()
        .map_or(1, |m| m + 1)
}

fn cmd_export(
    project_path: &Path,
    output: &Path,
    encoder: Encoder,
    crf: u8,
    preset: &str,
    bitrate: &str,
) -> Result<()> {
    say!(
        "{}",
        run_export(project_path, output, encoder, crf, preset, bitrate)?
    );
    Ok(())
}

/// Render the timeline to `output`; returns the summary line. Split from
/// [`cmd_export`] so the TUI's `:export` can run the same code.
#[allow(clippy::too_many_arguments)]
fn run_export(
    project_path: &Path,
    output: &Path,
    encoder: Encoder,
    crf: u8,
    preset: &str,
    bitrate: &str,
) -> Result<String> {
    let project = Project::load(project_path)?;
    let spec = ExportSpec {
        output: output.to_path_buf(),
        encoder,
        crf,
        preset: preset.to_string(),
        bitrate: bitrate.to_string(),
    };
    log::line(&format!(
        "export -> {} ({:?})",
        output.display(),
        spec.encoder
    ));
    let plan = nle_ffmpeg::export(&project, &spec)?;
    let actual = nle_ffmpeg::probe(output)?;
    let shown = actual
        .format
        .duration
        .as_deref()
        .and_then(parse_f64)
        .map(|d| format!("{d:.2}"))
        .unwrap_or_else(|| "?".into());
    Ok(format!(
        "✓ {}  ({} s · {} frames @ {}/{})",
        output.display(),
        shown,
        plan.frames,
        plan.fps.numer(),
        plan.fps.denom()
    ))
}

/// What a work function produced: warnings for stderr (or the log) and
/// the summary line the caller shows. Keeps the TUI free of stray prints
/// — a `say!` would draw over the terminal UI.
struct CommandReport {
    warnings: Vec<String>,
    summary: String,
}

impl CommandReport {
    fn new(summary: String) -> Self {
        Self {
            warnings: Vec::new(),
            summary,
        }
    }

    fn warn(&mut self, msg: String) {
        self.warnings.push(msg);
    }

    /// The CLI's rendering: warnings on stderr, summary on stdout.
    fn print(&self) {
        for w in &self.warnings {
            say_err!("{w}");
        }
        say!("{}", self.summary);
    }
}

fn asset_summary(a: &Asset) -> String {
    let mut s = match a.duration() {
        Some(d) => format!("{} s", format_seconds(&d)),
        None => "? s".to_string(),
    };
    if let Some((w, h)) = a.video_size() {
        s += &format!(" · {w}×{h}");
    }
    if let Some(fps) = a.fps() {
        s += &format!(" @ {}/{}", fps.numer(), fps.denom());
    }
    s += if a.probe.has_audio() {
        " · audio ✓"
    } else {
        " · no audio"
    };
    s
}

// ------------------------------------------------------------ media cmds (M0)

fn cmd_probe(file: &Path, json: bool) -> Result<()> {
    let report = probe(file)?;
    if json {
        say!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print_summary(file, &report);
    Ok(())
}

fn print_summary(path: &Path, report: &RawProbe) {
    say!("{}", path.display());

    let f = &report.format;
    let mut container = f.format_name.clone();
    if let Some(brand) = f.tags.as_ref().and_then(|t| t.get("major_brand")) {
        container.push_str(&format!(" ({brand})"));
    }
    let mut meta = vec![container];
    if let Some(d) = f.duration.as_deref().and_then(parse_f64) {
        meta.push(format!("{d:.2} s"));
    }
    if let Some(b) = f.size.as_deref().and_then(parse_u64) {
        meta.push(human_bytes(b));
    }
    if let Some(b) = f.bit_rate.as_deref().and_then(parse_u64) {
        meta.push(format!("{} kb/s", b / 1000));
    }
    say!("  container  {}", meta.join("  ·  "));

    for s in &report.streams {
        match s.codec_type.as_deref() {
            Some("video") => say!("  video #{}   {}", s.index, video_desc(s)),
            Some("audio") => say!("  audio #{}   {}", s.index, audio_desc(s)),
            other => say!(
                "  {} #{}   {}",
                other.unwrap_or("data"),
                s.index,
                s.codec_name.as_deref().unwrap_or("?")
            ),
        }
    }
}

fn video_desc(s: &RawStream) -> String {
    let mut bits = Vec::new();
    let codec = s.codec_name.clone().unwrap_or_else(|| "?".into());
    bits.push(match &s.profile {
        Some(p) => format!("{codec} ({p})"),
        None => codec,
    });
    if let (Some(w), Some(h)) = (s.width, s.height) {
        bits.push(format!("{w}×{h}"));
    }
    if let Some(rate) = s.r_frame_rate.as_deref().and_then(parse_rate) {
        let decimal = *rate.numer() as f64 / *rate.denom() as f64;
        bits.push(format!(
            "{}/{} fps ({decimal:.2})",
            rate.numer(),
            rate.denom()
        ));
    }
    if let Some(p) = &s.pix_fmt {
        bits.push(p.clone());
    }
    if let Some(b) = s.bit_rate.as_deref().and_then(parse_u64) {
        bits.push(format!("{} kb/s", b / 1000));
    }
    bits.join("  ·  ")
}

fn audio_desc(s: &RawStream) -> String {
    let mut bits = Vec::new();
    bits.push(s.codec_name.clone().unwrap_or_else(|| "?".into()));
    if let Some(sr) = &s.sample_rate {
        bits.push(format!("{sr} Hz"));
    }
    if let Some(layout) = &s.channel_layout {
        bits.push(layout.clone());
    } else if let Some(c) = s.channels {
        bits.push(format!("{c} ch"));
    }
    if let Some(b) = s.bit_rate.as_deref().and_then(parse_u64) {
        bits.push(format!("{} kb/s", b / 1000));
    }
    bits.join("  ·  ")
}

fn parse_f64(s: &str) -> Option<f64> {
    s.parse().ok()
}

fn parse_u64(s: &str) -> Option<u64> {
    s.parse().ok()
}

fn human_bytes(b: u64) -> String {
    const MIB: f64 = (1 << 20) as f64;
    if b >= 1 << 20 {
        format!("{:.1} MiB", b as f64 / MIB)
    } else {
        format!("{:.0} KiB", b as f64 / 1024.0)
    }
}

// ------------------------------------------------------------ trim (M0)

#[allow(clippy::too_many_arguments)]
fn cmd_trim(
    input: &Path,
    in_point: &str,
    out_point: &str,
    output: &Path,
    encoder: Encoder,
    copy: bool,
    crf: u8,
    preset: &str,
    bitrate: &str,
) -> Result<()> {
    let start = parse_time(in_point).with_context(|| format!("invalid --in time: {in_point:?}"))?;
    let end =
        parse_time(out_point).with_context(|| format!("invalid --out time: {out_point:?}"))?;
    let duration = end - start;
    if duration <= Seconds::from_integer(0) {
        bail!("--out ({out_point}) must be later than --in ({in_point})");
    }
    // ffmpeg happily cuts past the end and hands back a shorter file; say so
    // instead of silently writing something else than what was asked for.
    let report = probe(input).with_context(|| format!("probing {}", input.display()))?;
    if let Some(file_dur) = report.duration()
        && end > file_dur
    {
        bail!(
            "--out ({}) is beyond the file's duration ({})",
            format_seconds(&end),
            format_seconds(&file_dur)
        );
    }

    let spec = TrimSpec {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        start,
        end,
        encoder,
        copy,
        crf,
        preset: preset.to_string(),
        bitrate: bitrate.to_string(),
    };
    log::line(&format!(
        "trim {} -> {} [{}..{}]",
        input.display(),
        output.display(),
        format_seconds(&start),
        format_seconds(&end)
    ));
    nle_ffmpeg::trim(&spec)?;
    // Report the ACTUAL duration of what we wrote, not what we asked for —
    // stream copy lands within a frame or two of the request.
    let actual = nle_ffmpeg::probe(&spec.output)?;
    let shown = match actual.format.duration.as_deref().and_then(parse_f64) {
        Some(d) => format!("{d:.2} s"),
        None => "?".to_string(),
    };
    say!("✓ {}  ({})", output.display(), shown);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every catalog action is reachable from the CLI — through the same
    /// [`to_action`] mapping `main` dispatches with. A new `Action`
    /// variant without a CLI spelling fails this.
    #[test]
    fn every_action_is_reachable_from_the_cli() {
        let cmds = [
            Cmd::TrimIn {
                at: "1".into(),
                track: 1,
                ripple: false,
            },
            Cmd::TrimOut {
                at: "1".into(),
                track: 1,
                ripple: false,
            },
            Cmd::Split {
                at: "1".into(),
                track: 1,
            },
            Cmd::Gap {
                at: "1".into(),
                frames: Some(3),
                dur: None,
                track: 1,
            },
            Cmd::Delete {
                at: Some("1".into()),
                clip: None,
                seg: None,
                track: 1,
                ripple: false,
            },
            Cmd::Yank {
                at: Some("1".into()),
                clip: None,
                seg: None,
                track: 1,
            },
            Cmd::Paste {
                at: "1".into(),
                track: 1,
                ripple: false,
            },
            Cmd::Speed {
                id: 1,
                rate: "2".into(),
                track: 1,
            },
            Cmd::Extend {
                clip: Some(1),
                seg: None,
                at: None,
                edge: "in".into(),
                to: "2".into(),
                track: 1,
                ripple: false,
            },
            Cmd::PurgeStumps { track: None },
            Cmd::Track {
                action: TrackCmd::Add {
                    kind: TrackKindArg::Video,
                },
            },
            Cmd::Track {
                action: TrackCmd::Mute { index: None },
            },
            Cmd::Track {
                action: TrackCmd::Rm { index: 1 },
            },
            Cmd::Media {
                action: MediaCmd::Rm { id: 1 },
            },
            Cmd::Undo {},
            Cmd::Redo {},
        ];
        let mut seen = vec![false; nle_core::action::SLOTS];
        for cmd in &cmds {
            let a = to_action(cmd)
                .unwrap_or_else(|| panic!("{cmd:?} is an edit command"))
                .unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
            let s = a.slot();
            assert!(!seen[s], "{cmd:?} duplicates slot {s}");
            seen[s] = true;
        }
        assert!(seen.iter().all(|&s| s), "CLI-unreachable actions: {seen:?}");
    }

    #[test]
    fn cli_flags_map_to_the_expected_action() {
        let cmd = Cmd::Delete {
            at: Some("1.5".into()),
            clip: None,
            seg: None,
            track: 2,
            ripple: true,
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::Delete {
                track: 1,
                target: Target::At(1.5),
                ripple: true,
            }
        );
        // --dur resolves against the project's frame rate at run time.
        let cmd = Cmd::Gap {
            at: "0".into(),
            frames: None,
            dur: Some("2".into()),
            track: 1,
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::InsertGap {
                track: 0,
                at: 0.0,
                len: GapLen::Seconds(2.0),
            }
        );
        // Conflicting addressing is refused, not silently picked.
        let cmd = Cmd::Yank {
            at: Some("1".into()),
            clip: Some(3),
            seg: None,
            track: 1,
        };
        assert!(to_action(&cmd).unwrap().is_err());
    }

    /// A CLI edit records history — `nle undo` undoes it, and the
    /// playhead attribution comes back with the entry.
    /// Gap lengths are validated with our own message (a negative or zero
    /// length used to fall through to "shorter than one frame").
    #[test]
    fn gap_lengths_are_validated_with_a_clear_message() {
        let gap = |frames: Option<i64>, dur: Option<&str>| {
            to_action(&Cmd::Gap {
                at: "1".into(),
                frames,
                dur: dur.map(String::from),
                track: 1,
            })
            .unwrap()
        };
        assert!(
            gap(Some(0), None)
                .unwrap_err()
                .contains("at least one frame")
        );
        assert!(
            gap(Some(-5), None)
                .unwrap_err()
                .contains("at least one frame")
        );
        assert!(
            gap(None, Some("-2"))
                .unwrap_err()
                .contains("positive length")
        );
        assert!(gap(Some(3), None).is_ok());
    }

    /// A negative `--to` is real (an edge reaching back past time 0) and
    /// must survive clap into the action; other negatives get our own
    /// error, not clap's "unexpected argument '-1'".
    #[test]
    fn negative_times_reach_the_action() {
        let cmd = Cmd::Extend {
            clip: None,
            seg: Some(1),
            at: None,
            edge: "in".into(),
            to: "-2".into(),
            track: 1,
            ripple: false,
        };
        assert_eq!(
            to_action(&cmd).unwrap().unwrap(),
            Action::Extend {
                track: 0,
                target: Target::Segment(1),
                edge: Edge::In,
                to: -2.0,
                ripple: false,
            }
        );
    }

    /// The model counts tracks from 0; the message must speak the number
    /// you typed (1-based, like `v1`/`a1`).
    #[test]
    fn track_errors_report_the_number_you_typed() {
        let dir = std::env::temp_dir().join(format!("nle-tracknum-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        Project::new().save(&path).unwrap();

        let err = run_cli_action(&path, &Action::TrackMute { track: 4 })
            .unwrap_err()
            .to_string();
        assert!(err.contains("#5"), "mute track 5 → {err}");
        let err = run_cli_action(&path, &Action::TrackRm { track: 9 })
            .unwrap_err()
            .to_string();
        assert!(err.contains("#10"), "rm track 10 → {err}");
    }

    /// Track numbers are 1-based everywhere; `0` is not a track (it used
    /// to silently mean "track 1" in `add` and `edl`).
    #[test]
    fn track_numbers_are_one_based() {
        let dir = std::env::temp_dir().join(format!("nle-track0-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        Project::new().save(&path).unwrap();

        let err = cmd_edl(&path, 0).unwrap_err().to_string();
        assert!(err.contains("1-based"), "{err}");
        let err = cmd_add(&path, Path::new("nope.mp4"), 0, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("1-based"), "{err}");
        // …and the valid path still works.
        assert!(cmd_edl(&path, 1).is_err(), "an empty timeline has no EDL");
    }

    /// `nle seek` stores a snapped, clamped playhead — the TUI opens there.
    #[test]
    fn seek_stores_a_clamped_playhead() {
        let dir = std::env::temp_dir().join(format!("nle-seek-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset(PathBuf::from("/media/red.mp4"), RawProbe::default());
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            1,
            a,
            Seconds::from_integer(0),
            Seconds::from_integer(4),
        ))];
        p.save(&path).unwrap();

        cmd_seek(&path, Some("2")).unwrap();
        assert_eq!(
            Project::load(&path).unwrap().playhead,
            Seconds::from_integer(2)
        );
        // Past the end clamps into the timeline…
        cmd_seek(&path, Some("99")).unwrap();
        assert_eq!(
            Project::load(&path).unwrap().playhead,
            Seconds::from_integer(4)
        );
        // …and an off-grid request snaps to a frame (1.02 s @ 30 = f30.6).
        cmd_seek(&path, Some("1.02")).unwrap();
        assert_eq!(Project::load(&path).unwrap().playhead, Seconds::new(31, 30));
    }

    #[test]
    fn cli_edits_go_through_the_undo_history() {
        let dir = std::env::temp_dir().join(format!("nle-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.json");
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset(PathBuf::from("/media/red.mp4"), RawProbe::default());
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip::new(
            1,
            a,
            Seconds::from_integer(0),
            Seconds::from_integer(4),
        ))];
        p.save(&path).unwrap();

        run_cli_action(&path, &Action::Split { track: 0, at: 2.0 }).unwrap();
        assert_eq!(
            Project::load(&path)
                .unwrap()
                .timeline
                .video_track()
                .unwrap()
                .segments
                .len(),
            2
        );
        run_cli_action(&path, &Action::Undo {}).unwrap();
        let after = Project::load(&path).unwrap();
        assert_eq!(
            after.timeline.video_track().unwrap().segments.len(),
            1,
            "undo restores the pre-split timeline"
        );
        run_cli_action(&path, &Action::Redo {}).unwrap();
        assert_eq!(
            Project::load(&path)
                .unwrap()
                .timeline
                .video_track()
                .unwrap()
                .segments
                .len(),
            2,
            "redo reapplies it"
        );
    }

    fn gap(secs: i64) -> Segment {
        Segment::Gap(Gap {
            duration: Seconds::from_integer(secs),
        })
    }

    #[test]
    fn append_spans_coalesces_a_padding_gap_into_a_leading_gap() {
        let mut p = Project::new();
        let ai = p.timeline.add_track(TrackKind::Audio);
        // Appending at 5s pads 0..5, and the run starts with a 3s gap: the two
        // must merge into one 8s gap, not sit adjacent.
        append_spans(&mut p, ai, Seconds::from_integer(5), vec![gap(3), gap(2)]);
        let segs = &p.timeline.track(ai).unwrap().segments;
        assert_eq!(segs, &vec![gap(8), gap(2)], "{segs:?}");
    }

    #[test]
    fn append_spans_keeps_the_padding_when_the_run_leads_with_media() {
        let mut p = Project::new();
        let ai = p.timeline.add_track(TrackKind::Audio);
        let clip = Segment::Clip(Clip::new(
            1,
            1,
            Seconds::from_integer(0),
            Seconds::from_integer(2),
        ));
        append_spans(&mut p, ai, Seconds::from_integer(5), vec![clip.clone()]);
        let segs = &p.timeline.track(ai).unwrap().segments;
        assert_eq!(segs, &vec![gap(5), clip], "{segs:?}");
    }

    #[test]
    fn append_snaps_to_the_project_grid_not_the_source() {
        let file = Seconds::new(60000, 1001); // 59.94 source
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(24)); // 24 fps timeline
        assert_eq!(snap_fps_for(Some(&p), file), Seconds::from_integer(24));
        // A fresh project (no target) uses the source's fps.
        assert_eq!(snap_fps_for(None, file), file);
        // An append target with no fps falls back to the source's.
        assert_eq!(snap_fps_for(Some(&Project::new()), file), file);
    }

    fn pool(names: &[&str]) -> Project {
        let mut p = Project::new();
        for n in names {
            let (_, _) = p.add_asset(PathBuf::from(n), RawProbe::default());
        }
        p
    }

    // Only exact tiers bind when several candidates contain the needle —
    // a first-match would pick by pool order, not by evidence.
    #[test]
    fn fuzzy_matches_must_be_unique() {
        let p = pool(&["/media/red_a.mp4", "/media/red_b.mp4"]);
        assert_eq!(match_asset(&p, "red_a"), Some(1));
        assert_eq!(match_asset(&p, "red"), None, "two contain 'red'");
    }

    // A filename match is stronger evidence than a stem match.
    #[test]
    fn a_filename_beats_a_stem_match() {
        let p = pool(&["/media/red.mkv", "/media/red.mp4"]);
        assert_eq!(
            match_asset(&p, "red.mp4"),
            Some(2),
            "the .mp4, not the .mkv"
        );
    }

    // ... but an ambiguous exact match is refused by the same rule the
    // fuzzy tier follows.
    #[test]
    fn an_ambiguous_exact_name_stays_unresolved() {
        let p = pool(&["/a/red.mp4", "/b/red.mp4"]);
        assert_eq!(match_asset(&p, "red.mp4"), None, "two files named red.mp4");
        let p = pool(&["/a/red.mp4", "/b/red.mkv"]);
        assert_eq!(match_asset(&p, "red"), None, "two stems are 'red'");
    }

    #[test]
    fn a_generic_reel_stays_unresolved() {
        // AX is a substring of unrelated stems; without hints it must not
        // silently bind any of them (This was the "taxi" trap: reel AX +
        // taxi.mp4 pooled resolves... nothing, and import-edl says so.)
        let p = pool(&["/media/taxi.mp4"]);
        assert_eq!(match_asset(&p, "AX"), None);
        assert_eq!(match_asset(&p, "ax"), None);
    }

    #[test]
    fn unique_fuzzy_and_exact_hint_still_bind() {
        let p = pool(&["/media/red_only.mp4"]);
        assert_eq!(match_asset(&p, "red"), Some(1), "unique substring");
        // A whole-path SOURCE FILE hint matches its basename stem.
        let p = pool(&["/media/blue.mp4"]);
        assert_eq!(match_asset(&p, "/media/blue.mp4"), Some(1));
    }

    #[test]
    fn an_empty_needle_never_fuzzy_matches() {
        let p = pool(&["/media/a.mp4", "/media/b.mp4"]);
        assert_eq!(match_asset(&p, ""), None);
    }
}
