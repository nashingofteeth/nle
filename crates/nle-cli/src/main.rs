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

// Declared after the macros: a child module only sees a macro_rules! that
// precedes the `mod` declaration.
mod cmd;

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
pub(crate) use say_err;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use nle_core::edit;
use nle_core::{Seconds, parse_rate};
use nle_ffmpeg::Encoder;
#[cfg(test)]
use nle_ffmpeg::RawProbe;

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
        /// New speed in percent: 100 = normal, 50 = half, 200 = double
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
    /// Add a crossfade at a boundary (TUI `X`; the default kind is a dissolve)
    Transition {
        /// Which shape: dissolve (default) or fade (through black)
        #[arg(value_name = "KIND")]
        kind: Option<String>,
        /// Where the cut is (seconds or timecode)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        at: String,
        /// Length in frames (default: one second)
        #[arg(long, allow_negative_numbers = true)]
        frames: Option<i64>,
        /// Length as a time (alternative to --frames)
        #[arg(long, value_name = "TIME", allow_negative_numbers = true)]
        dur: Option<String>,
        /// Retime an existing transition: rate percent (50 = twice as long)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        percent: Option<f64>,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
    },
    /// Add, remove, or clear a visual effect on a clip (TUI `x`)
    Effect {
        /// Which effect: crop | scale | eq | lut | rm | clear
        #[arg(value_name = "KIND")]
        kind: String,
        /// The clip to attach it to
        #[arg(long)]
        clip: u32,
        /// Track number (1-based; default 1)
        #[arg(long, default_value_t = 1)]
        track: usize,
        /// `rm`: which effect to drop; add: which row to replace (0-based;
        /// default: rm's last row, add appends)
        #[arg(long, value_name = "N")]
        index: Option<usize>,
        /// scale: width as a percentage of the source (100 = unchanged)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        w: Option<f64>,
        /// scale: height as a percentage of the source (100 = unchanged)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        h: Option<f64>,
        /// crop: inset from the left edge (0 = the edge, 100 = the opposite)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        left: Option<f64>,
        /// crop: inset from the right edge (percent)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        right: Option<f64>,
        /// crop: inset from the top edge (percent)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        top: Option<f64>,
        /// crop: inset from the bottom edge (percent)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        bottom: Option<f64>,
        /// offset: horizontal shift (percent of the distance to the edge)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        x: Option<f64>,
        /// offset: vertical shift (percent of the distance to the edge)
        #[arg(long, value_name = "P", allow_negative_numbers = true)]
        y: Option<f64>,
        /// eq brightness (0 is neutral)
        #[arg(long, value_name = "F", allow_negative_numbers = true)]
        brightness: Option<f64>,
        /// eq contrast (1 is neutral)
        #[arg(long, value_name = "F", allow_negative_numbers = true)]
        contrast: Option<f64>,
        /// eq saturation (1 is neutral)
        #[arg(long, value_name = "F", allow_negative_numbers = true)]
        saturation: Option<f64>,
        /// lut3d file
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
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
    /// On-disk caches: prune old proxy renders (explicit, never automatic)
    Cache {
        #[command(subcommand)]
        action: CacheCmd,
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
    Edit {
        /// Render the TUI once, print the state and grid, and exit (diagnostics)
        #[arg(long)]
        dump: bool,
        /// Grid size for --dump
        #[arg(long, value_name = "WxH", default_value = "100x30")]
        dump_size: String,
        /// Keys to feed before dumping (`\n` = Enter, `\e` = Esc)
        #[arg(long, value_name = "KEYS", default_value = "")]
        dump_keys: String,
    },

    /// Print the mpv preview URL the monitor plays (the composite), or one track's
    /// with `--track N` (debugging)
    Edl {
        /// Print one track instead of the composite the monitor plays
        /// (1-based)
        #[arg(long)]
        track: Option<usize>,
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

#[derive(Debug, Subcommand)]
enum CacheCmd {
    /// Delete proxy renders older than N days
    Prune {
        /// Maximum age in days (default 30)
        #[arg(long, default_value_t = 30)]
        days: u64,
        /// List what would go; remove nothing
        #[arg(long)]
        dry_run: bool,
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

use cmd::detect::{DetectArgs, cmd_detect_edits};
use cmd::export::cmd_export;
use cmd::import::{cmd_import, cmd_import_edl, cmd_import_otio};
use cmd::media::{cmd_probe, cmd_trim};
#[cfg(feature = "tui")]
use cmd::project::cmd_edit;
use cmd::project::{
    cmd_add, cmd_cache, cmd_edl, cmd_init, cmd_media_ls, cmd_seek, cmd_show, cmd_track_ls,
};
use cmd::{run_cli_action, to_action};

// ------------------------------------------------------------ action mapping

/// The user-facing text for a model error. Track numbers are 1-based in
/// the CLI and the TUI (`v1`, `a1`), so the model's 0-based index is
/// translated here — one place, so the two front-ends agree.
pub(crate) fn edit_message(e: &edit::EditError) -> String {
    match e {
        edit::EditError::TrackNotFound(n) => format!("no track #{}", n + 1),
        other => other.to_string(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Process-bound commands and queries dispatch here; every edit-shaped
    // command falls through to the action catalog below — the same
    // executor (and undo history) the TUI uses.
    match &cli.command {
        #[cfg(feature = "tui")]
        Cmd::Edit {
            dump,
            dump_size,
            dump_keys,
        } => return cmd_edit(&cli.project, *dump, dump_size, dump_keys),
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
        Cmd::Cache { action } => return cmd_cache(action),
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
    let bad = || format!("invalid speed {s:?} (try 100, 200, or 50)");
    let percent: f64 = s.parse().with_context(bad)?;
    if !percent.is_finite() || percent <= 0.0 {
        bail!("speed must be positive (got {s:?})");
    }
    Seconds::approximate_float(percent / 100.0)
        .filter(|r| *r > Seconds::from_integer(0))
        .with_context(bad)
}

// ------------------------------------------------------------ media cmds (M0)

// ------------------------------------------------------------ trim (M0)

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cmd::detect::{append_spans, snap_fps_for};
    use crate::cmd::import::match_asset;
    use crate::cmd::project::show_report;
    use std::path::Path;

    use nle_core::Edge;
    use nle_core::timeline::{Clip, Gap, Segment, TrackKind};
    use nle_core::{Action, GapLen, Project, Target};

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
            Cmd::Transition {
                kind: None,
                at: "1".into(),
                frames: Some(3),
                dur: None,
                percent: None,
                track: 1,
            },
            Cmd::Effect {
                kind: "crop".into(),
                clip: 1,
                track: 1,
                index: None,
                w: None,
                h: None,
                left: Some(10.0),
                right: Some(10.0),
                top: None,
                bottom: None,
                x: None,
                y: None,
                brightness: None,
                contrast: None,
                saturation: None,
                path: None,
            },
            Cmd::Effect {
                kind: "rm".into(),
                clip: 1,
                track: 1,
                index: Some(0),
                w: None,
                h: None,
                left: None,
                right: None,
                top: None,
                bottom: None,
                x: None,
                y: None,
                brightness: None,
                contrast: None,
                saturation: None,
                path: None,
            },
            Cmd::Effect {
                kind: "clear".into(),
                clip: 1,
                track: 1,
                index: None,
                w: None,
                h: None,
                left: None,
                right: None,
                top: None,
                bottom: None,
                x: None,
                y: None,
                brightness: None,
                contrast: None,
                saturation: None,
                path: None,
            },
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

    // `nle show` names a clip's effects, in the order the export applies
    // them, so a shell-only session can see what is on a clip.
    #[test]
    fn show_report_names_a_clips_effects() {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(30));
        let (a, _) = p.add_asset(
            "/media/red.mp4".into(),
            nle_core::media::RawProbe::default(),
        );
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: vec![
                nle_core::timeline::Effect::Eq {
                    brightness: 0.0,
                    contrast: 1.0,
                    saturation: 1.0,
                },
                nle_core::timeline::Effect::Lut {
                    path: "/luts/warm.cube".into(),
                },
            ],
        })];
        let report = show_report(&p);
        assert!(report.contains("· fx: eq, lut"), "{report}");
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

        let err = cmd_edl(&path, Some(0)).unwrap_err().to_string();
        assert!(err.contains("1-based"), "{err}");
        let err = cmd_add(&path, Path::new("nope.mp4"), 0, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("1-based"), "{err}");
        // …and the valid paths still work (the composite has nothing to play
        // on an empty timeline either).
        assert!(
            cmd_edl(&path, Some(1)).is_err(),
            "an empty timeline has no EDL"
        );
        assert!(
            cmd_edl(&path, None).is_err(),
            "an empty timeline has no EDL"
        );
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
        // Past the end clamps onto the last frame: 4 s spans [0, 4), so the
        // final frame is 119/30, not the 4 s boundary.
        cmd_seek(&path, Some("99")).unwrap();
        assert_eq!(
            Project::load(&path).unwrap().playhead,
            Seconds::new(119, 30)
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
