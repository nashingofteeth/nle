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
#[cfg(feature = "tui")]
mod tui;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use nle_core::detect::{Span, reconstruct};
use nle_core::edit;
use nle_core::edl::{self, EdlEvent};
use nle_core::otio::{self, OtioItem};
use nle_core::time::{parse_timecode, timecode_to_frames};
use nle_core::timeline::{Clip, Gap, Segment, Timeline, Track, TrackKind};
use nle_core::{Asset, Project, Seconds, VideoSettings, format_seconds, parse_rate, parse_time};
use nle_ffmpeg::detect::{DetectOptions, detect};
use nle_ffmpeg::{Encoder, ExportSpec, RawProbe, RawStream, TrimSpec, probe};

#[derive(Parser)]
#[command(name = "nle", version, about = "Terminal-native video editor")]
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

#[derive(Subcommand)]
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
        /// Track index to place on (0-based; default 0)
        #[arg(long, default_value_t = 0)]
        track: usize,
        /// Timeline position (default: end of track)
        #[arg(long, value_name = "TIME")]
        at: Option<String>,
        /// Source in-point (default: 0)
        #[arg(long = "in", value_name = "TIME")]
        in_point: Option<String>,
        /// Source out-point (default: end of file)
        #[arg(long = "out", value_name = "TIME")]
        out_point: Option<String>,
    },
    /// Remove a clip by id (see `nle show`)
    Remove {
        /// Clip id
        id: u32,
    },
    /// Change a clip's playback speed (the track ripples to fit)
    Speed {
        /// Clip id (from `nle show`)
        id: u32,
        /// New speed: 1, 2, 0.5, 0.8, 24/25 … (a rate, not a duration)
        rate: String,
        /// Track index (0-based; default 0)
        #[arg(long, default_value_t = 0)]
        track: usize,
    },
    /// Show the timeline
    Show {
        /// Print the raw project JSON
        #[arg(long)]
        json: bool,
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
        #[arg(long, value_name = "TIME", default_value = "0.5")]
        min_shot: String,
        /// Shortest black interval to treat as a gap
        #[arg(long, value_name = "TIME", default_value = "0.1")]
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
        /// Track index to preview (0-based; default 0)
        #[arg(long, default_value_t = 0)]
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
        #[arg(long = "in", value_name = "TIME")]
        in_point: String,
        /// End (same formats, exclusive)
        #[arg(long = "out", value_name = "TIME")]
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
#[derive(Subcommand)]
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
#[derive(Subcommand)]
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
    /// Remove an empty track by index (1-based)
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        #[cfg(feature = "tui")]
        Cmd::Edit {} => cmd_edit(&cli.project),
        Cmd::Edl { track } => cmd_edl(&cli.project, track),
        Cmd::Init { force } => cmd_init(&cli.project, force),
        Cmd::Import { files } => cmd_import(&cli.project, &files),
        Cmd::Add {
            file,
            track,
            at,
            in_point,
            out_point,
        } => cmd_add(
            &cli.project,
            &file,
            track,
            at.as_deref(),
            in_point.as_deref(),
            out_point.as_deref(),
        ),
        Cmd::Remove { id } => cmd_remove(&cli.project, id),
        Cmd::Speed { id, rate, track } => cmd_speed(&cli.project, id, &rate, track),
        Cmd::Show { json } => cmd_show(&cli.project, json),
        Cmd::Media { action } => cmd_media(&cli.project, action),
        Cmd::Track { action } => cmd_track(&cli.project, action),
        Cmd::ImportEdl {
            file,
            media,
            fps,
            src_offset,
            map,
            force,
        } => cmd_import_edl(
            &cli.project,
            &file,
            &media,
            fps.as_deref(),
            &src_offset,
            &map,
            force,
        ),
        Cmd::ImportOtio {
            file,
            media,
            fps,
            remap,
            force,
        } => cmd_import_otio(
            &cli.project,
            &file,
            media.as_deref(),
            fps.as_deref(),
            &remap,
            force,
        ),
        Cmd::DetectEdits {
            file,
            threshold,
            min_shot,
            black_min,
            no_gaps,
            fps,
            video_only,
            list,
            force,
        } => cmd_detect_edits(DetectArgs {
            project: &cli.project,
            file: &file,
            threshold,
            min_shot: &min_shot,
            black_min: &black_min,
            no_gaps,
            fps: fps.as_deref(),
            video_only,
            list,
            force,
        }),
        Cmd::Export {
            output,
            encoder,
            crf,
            preset,
            bitrate,
        } => cmd_export(
            &cli.project,
            &output,
            encoder.into(),
            crf,
            &preset,
            &bitrate,
        ),
        Cmd::Probe { file, json } => cmd_probe(&file, json),
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
        } => cmd_trim(
            &input,
            &in_point,
            &out_point,
            &output,
            encoder.into(),
            copy,
            crf,
            &preset,
            &bitrate,
        ),
    }
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
    player.load(&edl, None);
    tui::run(project, project_path.to_path_buf(), player)?;
    Ok(())
}

fn cmd_edl(project_path: &Path, track: usize) -> Result<()> {
    let project = Project::load(project_path)?;
    println!("{}", preview::build_edl(&project, track)?);
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
    println!("✓ created {}", path.display());
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
    for file in files {
        let report = probe(file).with_context(|| format!("importing {}", file.display()))?;
        let canonical = file
            .canonicalize()
            .with_context(|| format!("resolving {}", file.display()))?;
        if let Some(a) = project.asset_by_path(&canonical) {
            println!("· {} already imported (asset #{})", file.display(), a.id);
            continue;
        }
        let (id, _) = project.add_asset(canonical, report);
        let asset = project.asset(id).context("asset vanished")?;
        println!(
            "✓ asset #{id}  {}  {}",
            file.display(),
            asset_summary(asset)
        );
    }
    save_and_reset_history(&project, project_path)?;
    let clip_count = |p: &Project| {
        p.timeline
            .tracks
            .iter()
            .flat_map(|t| t.segments.iter())
            .filter(|s| matches!(s, Segment::Clip(_)))
            .count()
    };
    println!(
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
    let canonical = file
        .canonicalize()
        .with_context(|| format!("{} not found", file.display()))?;

    let asset_id = match project.asset_by_path(&canonical) {
        Some(a) => a.id,
        None => {
            let report = probe(file).with_context(|| format!("importing {}", file.display()))?;
            let (id, _) = project.add_asset(canonical, report);
            println!("· auto-imported as asset #{id}");
            id
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
    let at_t: Option<Seconds> = match at {
        Some(s) => Some(parse_time(s).with_context(|| format!("invalid --at time: {s:?}"))?),
        None => None,
    };

    // Validate the source range against the asset.

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

    // Ripple paste: inserts at --at (splits clips/gaps) or appends.
    let append_at = match at_t {
        Some(t) => t,
        None => project
            .timeline
            .track(track)
            .map(|t| t.end())
            .unwrap_or_else(|| Seconds::from_integer(0)),
    };
    let scratch = Segment::Clip(Clip {
        id: 0,
        asset: asset_id,
        asset_in: in_t,
        asset_out: out_t,
        speed: Seconds::from_integer(1),
    });
    let mut history = edit::History::default();
    let batch = edit::paste_ripple_on(&project, track, &scratch, secs_f64(&append_at))?;
    history.apply(&mut project, batch, 0)?;
    save_and_reset_history(&project, project_path)?;
    Ok(())
}

fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

fn cmd_remove(project_path: &Path, id: u32) -> Result<()> {
    let mut project = Project::load(project_path)?;
    // Clip ids are unique across tracks; find the one that holds it.
    let Some(ti) = project
        .timeline
        .tracks
        .iter()
        .position(|t| t.clip_index(id).is_some())
    else {
        bail!("clip #{id} not found");
    };
    let batch = edit::delete_clip_by_id_on(&project, ti, id)?;
    let mut history = edit::History::default();
    history.apply(&mut project, batch, 0)?;
    save_and_reset_history(&project, project_path)?;
    println!("✓ removed clip #{id}");
    Ok(())
}

fn cmd_speed(project_path: &Path, id: u32, rate: &str, track: usize) -> Result<()> {
    let mut project = Project::load(project_path)?;
    let speed = parse_speed_arg(rate)?;
    let batch = edit::set_speed_on(&project, track, id, speed)?;
    let mut history = edit::History::default();
    history.apply(&mut project, batch, 0)?;
    save_and_reset_history(&project, project_path)?;
    println!("✓ clip #{id} → {}× (track rippled)", format_seconds(&speed));
    Ok(())
}

fn cmd_show(project_path: &Path, json: bool) -> Result<()> {
    let project = Project::load(project_path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&project)?);
        return Ok(());
    }
    let clip_count: usize = project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .filter(|s| matches!(s, Segment::Clip(_)))
        .count();

    let mut head = format!(
        "{} — {} clip(s) · {} s",
        project_path.display(),
        clip_count,
        format_seconds(&project.timeline.end())
    );
    if let Some(fps) = &project.fps {
        head += &format!(" · {}/{} fps", fps.numer(), fps.denom());
    }
    if let Some(v) = &project.video {
        head += &format!(" · {}×{}", v.width, v.height);
    }
    println!("{head}");

    for (ti, track) in project.timeline.tracks.iter().enumerate() {
        let mute = if track.muted { " (muted)" } else { "" };
        println!("  {}{mute}", track_label(&project, ti));

        let mut cursor = Seconds::from_integer(0);
        for seg in &track.segments {
            match seg {
                Segment::Gap(g) => {
                    let end = cursor + g.duration;
                    println!(
                        "      ┄ gap {}–{}",
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
                    println!(
                        "    #{:<4}{:>9} – {:<9} {}{speed}  (src {}–{})",
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
    Ok(())
}

/// Short label for a track: `v1..` for video, `a1..` for audio, numbered
/// within their kind by stacking order (matches the TUI gutter).
fn track_label(project: &Project, ti: usize) -> String {
    let (mut v, mut a) = (0, 0);
    for (i, t) in project.timeline.tracks.iter().enumerate() {
        match t.kind {
            TrackKind::Video => v += 1,
            TrackKind::Audio => a += 1,
        }
        if i == ti {
            return match t.kind {
                TrackKind::Video => format!("v{v}"),
                TrackKind::Audio => format!("a{a}"),
            };
        }
    }
    "—".to_string()
}

fn cmd_media(project_path: &Path, action: MediaCmd) -> Result<()> {
    match action {
        MediaCmd::Ls => {
            let project = Project::load(project_path)?;
            if project.media.is_empty() {
                println!("media pool is empty — `nle import <file>`");
                return Ok(());
            }
            println!(
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
                println!("  #{:<4} {:<9} {}", a.id, asset_summary(a), usage);
            }
            Ok(())
        }
        MediaCmd::Rm { id } => {
            let mut project = Project::load(project_path)?;
            let removed = project.remove_asset(id)?;
            save_and_reset_history(&project, project_path)?;
            println!("✓ removed asset #{id} ({})", removed.path.display());
            Ok(())
        }
    }
}

fn cmd_track(project_path: &Path, action: TrackCmd) -> Result<()> {
    let mut project = Project::load(project_path)?;
    match action {
        TrackCmd::Ls => {
            println!(
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
                println!(
                    "  {:<3} {:<6} {} segment(s){}",
                    i + 1,
                    kind,
                    t.segments.len(),
                    mute
                );
            }
            Ok(())
        }
        TrackCmd::Add { kind } => {
            let kind = match kind {
                TrackKindArg::Video => TrackKind::Video,
                TrackKindArg::Audio => TrackKind::Audio,
            };
            let idx = project.timeline.add_track(kind);
            save_and_reset_history(&project, project_path)?;
            println!("✓ added track #{}", idx + 1);
            Ok(())
        }
        TrackCmd::Mute { index } => {
            let idx = index.map(|n| n.saturating_sub(1)).unwrap_or(0);
            let Some(track) = project.timeline.track_mut(idx) else {
                bail!("no track #{}", idx + 1);
            };
            track.muted = !track.muted;
            let muted = track.muted;
            save_and_reset_history(&project, project_path)?;
            println!(
                "✓ track #{} {}",
                idx + 1,
                if muted { "muted" } else { "unmuted" }
            );
            Ok(())
        }
        TrackCmd::Rm { index } => {
            let idx = index.saturating_sub(1);
            if idx >= project.timeline.tracks.len() {
                bail!("no track #{index}");
            }
            if project.timeline.tracks[idx].kind == TrackKind::Video
                && project.timeline.video_tracks().count() <= 1
            {
                bail!("can't remove the only video track");
            }
            if !project.timeline.tracks[idx].segments.is_empty() {
                bail!("track #{index} isn't empty — delete its clips first");
            }
            project.timeline.tracks.remove(idx);
            save_and_reset_history(&project, project_path)?;
            println!("✓ removed track #{index}");
            Ok(())
        }
    }
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

/// Match a reel/clip name to a pooled asset: exact filename, then stem,
/// then a case-insensitive substring (unique best).
fn match_asset(project: &Project, needle: &str) -> Option<u32> {
    let needle = needle.to_lowercase();
    let needle_stem = Path::new(&needle)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| needle.clone());
    let mut fuzzy = None;
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
        if file == needle || stem == needle || file == needle_stem || stem == needle_stem {
            return Some(a.id);
        }
        if fuzzy.is_none() && (stem.contains(&needle_stem) || needle_stem.contains(&stem)) {
            fuzzy = Some(a.id);
        }
    }
    fuzzy
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
            add(&mut project, &path)?;
            imported += 1;
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
    println!(
        "✓ {} — {} event(s) → {} track(s), {} asset(s)",
        project_path.display(),
        doc.events.len(),
        project.timeline.tracks.len(),
        project.media.len()
    );
    Ok(())
}

/// Probe every media file in `dir` into the pool; returns how many files were
/// seen (whether or not they were already pooled).
fn pool_media_dir(project: &mut Project, dir: &Path) -> Result<usize> {
    let mut seen = 0usize;
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading media dir {}", dir.display()))?
    {
        let path = entry?.path();
        let is_media = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| MEDIA_EXTS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false);
        if path.is_file() && is_media {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("{} not found", path.display()))?;
            if project.asset_by_path(&canonical).is_none() {
                let report = probe(&canonical)?;
                project.add_asset(canonical, report);
            }
            seen += 1;
        }
    }
    Ok(seen)
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
    if let Some(dir) = media_dir {
        pool_media_dir(&mut project, dir)?;
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
    project.validate()?;
    save_and_reset_history(&project, project_path)?;

    for w in &doc.warnings {
        eprintln!("warning: {w}");
    }
    let clips = project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .filter(|s| matches!(s, Segment::Clip(_)))
        .count();
    println!(
        "✓ {} — {} track(s), {} asset(s), {} clip(s)",
        project_path.display(),
        project.timeline.tracks.len(),
        project.media.len(),
        clips
    );
    Ok(())
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
    force: bool,
}

fn cmd_detect_edits(args: DetectArgs) -> Result<()> {
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
        force,
    } = args;
    let min_shot =
        parse_time(min_shot).with_context(|| format!("invalid --min-shot {min_shot:?}"))?;
    let black_min =
        parse_time(black_min).with_context(|| format!("invalid --black-min {black_min:?}"))?;

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
        eprintln!(
            "warning: {extra_video} extra video stream(s) ignored — only the first is reconstructed"
        );
    }
    if extra_audio > 0 {
        eprintln!("warning: {extra_audio} extra audio stream(s) ignored — only the first is used");
    }

    let opts = DetectOptions {
        threshold,
        black_min,
        ..DetectOptions::default()
    };
    let detection = detect(&canonical, &opts)
        .with_context(|| format!("detecting cuts in {}", file.display()))?;
    let blacks: &[(Seconds, Seconds)] = if no_gaps { &[] } else { &detection.blacks };
    let spans = reconstruct(&detection.cuts, blacks, duration, fps, min_shot);

    let gaps = spans
        .iter()
        .filter(|s| matches!(s, Span::Gap { .. }))
        .count();
    if list {
        println!(
            "{} cut(s), {gaps} gap(s), {} total",
            detection.cuts.len(),
            format_seconds(&duration)
        );
        for c in &detection.cuts {
            println!("  cut {}", format_seconds(c));
        }
        return Ok(());
    }

    if project_path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            project_path.display()
        );
    }

    let has_audio = report.has_audio();
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
    let mut build_track = |kind: TrackKind| {
        let mut segments = Vec::new();
        for span in &spans {
            match span {
                Span::Gap { duration } => segments.push(Segment::Gap(Gap {
                    duration: *duration,
                })),
                Span::Media { start, end } => {
                    segments.push(Segment::Clip(Clip {
                        id: next_id,
                        asset: asset_id,
                        asset_in: *start,
                        asset_out: *end,
                    }));
                    next_id += 1;
                }
            }
        }
        Track {
            kind,
            muted: false,
            segments,
        }
    };
    let mut tracks = vec![build_track(TrackKind::Video)];
    if has_audio && !video_only {
        tracks.push(build_track(TrackKind::Audio));
    }
    project.timeline = Timeline { tracks };
    project.validate()?;
    save_and_reset_history(&project, project_path)?;

    let clips = spans
        .iter()
        .filter(|s| matches!(s, Span::Media { .. }))
        .count();
    println!(
        "✓ {} — {} cut(s) → {} clip(s), {} gap(s) @ {}",
        project_path.display(),
        detection.cuts.len(),
        clips,
        gaps,
        format_seconds(&fps),
    );
    Ok(())
}

fn cmd_export(
    project_path: &Path,
    output: &Path,
    encoder: Encoder,
    crf: u8,
    preset: &str,
    bitrate: &str,
) -> Result<()> {
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
    println!(
        "✓ {}  ({} s · {} frames @ {}/{})",
        output.display(),
        shown,
        plan.frames,
        plan.fps.numer(),
        plan.fps.denom()
    );
    Ok(())
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
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print_summary(file, &report);
    Ok(())
}

fn print_summary(path: &Path, report: &RawProbe) {
    println!("{}", path.display());

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
    println!("  container  {}", meta.join("  ·  "));

    for s in &report.streams {
        match s.codec_type.as_deref() {
            Some("video") => println!("  video #{}   {}", s.index, video_desc(s)),
            Some("audio") => println!("  audio #{}   {}", s.index, audio_desc(s)),
            other => println!(
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
    println!("✓ {}  ({})", output.display(), shown);
    Ok(())
}
