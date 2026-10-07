//! nle — terminal-native video editor.
//!
//! M2 status: interactive editor — TUI dashboard, mpv monitor, vim keys,
//! instant EDL preview. Project file, media pool, timeline, and export
//! landed in M0/M1.

mod mpv;
mod preview;
mod tui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use nle_core::edit;
use nle_core::timeline::{Clip, Segment};
use nle_core::{Asset, Project, Seconds, VideoSettings, format_seconds, parse_rate, parse_time};
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
    Edit {},

    /// Print the mpv preview URL for the timeline (debugging)
    Edl {},

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
        Cmd::Edit {} => cmd_edit(&cli.project),
        Cmd::Edl {} => cmd_edl(&cli.project),
        Cmd::Init { force } => cmd_init(&cli.project, force),
        Cmd::Import { files } => cmd_import(&cli.project, &files),
        Cmd::Add {
            file,
            at,
            in_point,
            out_point,
        } => cmd_add(
            &cli.project,
            &file,
            at.as_deref(),
            in_point.as_deref(),
            out_point.as_deref(),
        ),
        Cmd::Remove { id } => cmd_remove(&cli.project, id),
        Cmd::Show { json } => cmd_show(&cli.project, json),
        Cmd::Media { action } => cmd_media(&cli.project, action),
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

fn cmd_edit(project_path: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    let empty = project
        .timeline
        .video_track()
        .map(|t| t.segments.iter().all(|s| matches!(s, Segment::Gap(_))))
        .unwrap_or(true);
    if empty {
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
    let edl = preview::build_edl(&project)?;
    player.load(&edl, None);
    tui::run(project, project_path.to_path_buf(), player)?;
    Ok(())
}

fn cmd_edl(project_path: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    println!("{}", preview::build_edl(&project)?);
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
            .video_track()
            .map(|t| {
                t.segments
                    .iter()
                    .filter(|s| matches!(s, Segment::Clip(_)))
                    .count()
            })
            .unwrap_or(0)
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
        None => project.timeline.end(),
    };
    let scratch = Segment::Clip(Clip {
        id: 0,
        asset: asset_id,
        asset_in: in_t,
        asset_out: out_t,
    });
    let mut history = edit::History::default();
    let batch = edit::paste_ripple(&project, &scratch, secs_f64(&append_at))?;
    history.apply(&mut project, batch, 0)?;
    save_and_reset_history(&project, project_path)?;
    Ok(())
}

fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

fn cmd_remove(project_path: &Path, id: u32) -> Result<()> {
    let mut project = Project::load(project_path)?;
    let batch = edit::delete_clip_by_id(&project, id)?;
    let mut history = edit::History::default();
    history.apply(&mut project, batch, 0)?;
    save_and_reset_history(&project, project_path)?;
    println!("✓ removed clip #{id}");
    Ok(())
}

fn cmd_show(project_path: &Path, json: bool) -> Result<()> {
    let project = Project::load(project_path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&project)?);
        return Ok(());
    }
    let track = project.timeline.video_track().context("no video track")?;
    let clip_count = track
        .segments
        .iter()
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
    println!("  v1");

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
                println!(
                    "    #{:<4}{:>9} – {:<9} {}  (src {}–{})",
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
    Ok(())
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
