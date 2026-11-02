//! Commands that read or write the project itself: init, add, show, seek,
//! the listings, the cache controls and the TUI launcher.

use std::path::Path;

use anyhow::{Context, Result, bail};

use nle_core::edit;
use nle_core::project::wrong_kind;
use nle_core::state;
use nle_core::time::{parse_signed_time, parse_time};
use nle_core::timeline::{Clip, Segment, TrackKind};
use nle_core::{
    Action, Document, Project, Seconds, VideoSettings, format_seconds, frame_of, track_label,
};
use nle_ffmpeg::probe;

use super::asset_summary;
use super::{cli_index, run_cli_action};
use crate::CacheCmd;
#[cfg(feature = "tui")]
use crate::mpv;
use crate::preview;
use crate::proxy;
#[cfg(feature = "tui")]
use crate::tui;
use crate::{log, say};
#[cfg(feature = "tui")]
pub(crate) fn cmd_edit(
    project_path: &Path,
    dump: bool,
    dump_size: &str,
    dump_keys: &str,
) -> Result<()> {
    let project = Project::load(project_path)?;
    // `--dump`: render once and print the state + grid, no monitor needed —
    // the diagnostic path (a rendering or placement question needs no mpv).
    if dump {
        let (w, h) = dump_size
            .split_once(['x', 'X'])
            .map_or((100, 30), |(w, h)| {
                (
                    w.trim().parse().unwrap_or(100),
                    h.trim().parse().unwrap_or(30),
                )
            });
        print!(
            "{}",
            tui::dump(project, project_path.to_path_buf(), (w, h), dump_keys)
        );
        return Ok(());
    }
    // The monitor plays the composite, so any video track with a clip counts.
    let has_clip = project
        .timeline
        .video_tracks()
        .any(|t| t.segments.iter().any(|s| matches!(s, Segment::Clip(_))));
    if !has_clip {
        bail!("timeline is empty — add clips first (the monitor needs content)");
    }
    let media_title = format!(
        "nile — {}",
        project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_path.display().to_string())
    );
    let player = mpv::MpvPlayer::spawn(
        &format!("nile monitor — {}", project_path.display()),
        &media_title,
    )?;
    player.observe()?;
    // The TUI builds the monitor's EDL off-thread (proxy renders can be
    // seconds), so it appears immediately with progress instead of hanging
    // here before the terminal is initialized.
    tui::run(project, project_path.to_path_buf(), player)?;
    Ok(())
}

/// Print the `edl://` URL the monitor plays (or one track's, for debugging).
pub(crate) fn cmd_edl(project_path: &Path, track: Option<usize>) -> Result<()> {
    let project = Project::load(project_path)?;
    let Some(track) = track else {
        say!("{}", preview::build_composite_edl(&project, project_path)?);
        return Ok(());
    };
    // 1-based, and 0 is not a track number (it used to mean "track 1").
    let ti = cli_index(track, "--track").map_err(anyhow::Error::msg)?;
    if project.timeline.track(ti).is_none() {
        bail!("no track #{track}");
    }
    say!("{}", preview::build_edl(&project, project_path, ti)?);
    Ok(())
}

/// The project used when `-p` is absent: `.nile/config.json`, else
/// `nile.json` in the current directory.
pub(crate) fn resolve_default_project(cwd: &Path) -> Result<std::path::PathBuf> {
    Ok(state::load_default_project(cwd)?.unwrap_or_else(|| std::path::PathBuf::from("nile.json")))
}

/// `nile default [FILE]`: show or set the directory's default project.
pub(crate) fn cmd_default(cwd: &Path, file: Option<&Path>, unset: bool) -> Result<()> {
    if unset && file.is_some() {
        bail!("use either a FILE or --unset, not both");
    }
    let config = state::config_path(cwd);
    if unset {
        state::save_default_project(cwd, None)?;
        say!("default project: nile.json (fallback)");
        return Ok(());
    }
    if let Some(file) = file {
        state::save_default_project(cwd, Some(file))?;
        say!("✓ default project: {}", file.display());
        return Ok(());
    }
    match state::load_default_project(cwd)? {
        Some(project) => say!("default project: {}", project.display()),
        None => say!("default project: nile.json (fallback)"),
    }
    say!("config: {}", config.display());
    Ok(())
}

pub(crate) fn cmd_init(path: &Path, force: bool) -> Result<()> {
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
pub(crate) fn save_and_reset_history(project: &Project, path: &Path) -> Result<()> {
    // Through Document so the clamp-before-save rule holds on this path too;
    // callers that print clamp notes have already done the same clamp.
    let (mut doc, _) = Document::rebuild(project.clone());
    doc.save(path)?;
    doc.reset_history(path);
    log::line(&format!("saved {} (undo history reset)", path.display()));
    Ok(())
}

pub(crate) fn cmd_add(
    project_path: &Path,
    file: &Path,
    track: usize,
    at: Option<&str>,
    in_point: Option<&str>,
    out_point: Option<&str>,
) -> Result<()> {
    let mut doc = Document::with_history(
        Project::load(project_path)?,
        edit::History::load_project(project_path).unwrap_or_default(),
    );
    // `--track` is 1-based (matching the `v1`/`a1` labels); 0 internally.
    // 0 is not a track number — it used to silently mean "track 1".
    let ti = cli_index(track, "--track").map_err(anyhow::Error::msg)?;
    if ti >= doc.timeline.tracks.len() {
        bail!("no track #{track}");
    }
    let playhead = frame_of(doc.fps.as_ref(), secs_f64(&doc.playhead));
    let kind = doc
        .timeline
        .track(ti)
        .map(|t| t.kind)
        .with_context(|| format!("no track #{track}"))?;
    let (scratch, imported) = stage_clip(&mut doc, file, in_point, out_point, playhead, kind)?;
    if imported && let Segment::Clip(c) = &scratch {
        say!("· auto-imported as asset #{}", c.asset);
    }

    // Ripple paste: inserts at --at (splits clips/gaps) or appends.
    let append_at = match at {
        Some(s) => parse_time(s).with_context(|| format!("invalid --at time: {s:?}"))?,
        None => doc
            .timeline
            .track(ti)
            .map(|t| t.end())
            .unwrap_or_else(|| Seconds::from_integer(0)),
    };
    // The pool/settings writes land first, with their history: a recorded
    // pool addition (undo removes the asset again) plus the first-clip
    // settings. The insert itself goes through the catalog below — the
    // same `Paste` the TUI's pool insert runs.
    doc.save(project_path)?;
    if imported {
        doc.save_history(&edit::History::sidecar_path(project_path))?;
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
/// and stage the clip to insert — shared by `nile add` and the TUI's
/// `:add`. Returns the clip and whether the pool was written (a recorded
/// pool addition, so it is undoable). The first clip defines the sequence
/// settings, like a real NLE.
pub(crate) fn stage_clip(
    doc: &mut Document,
    file: &Path,
    in_point: Option<&str>,
    out_point: Option<&str>,
    playhead: i64,
    kind: TrackKind,
) -> Result<(Segment, bool)> {
    let canonical = file
        .canonicalize()
        .with_context(|| format!("{} not found", file.display()))?;

    let existing = doc.asset_by_path(&canonical).map(|a| a.id);
    let (asset_id, imported) = match existing {
        Some(id) => {
            // Refuse a kind mismatch *before* touching the pool, so the
            // error isn't preceded by a successful import.
            if let Some(asset) = doc.asset(id)
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
            let (id, _) = doc.add_asset(canonical, report, playhead);
            (id, true)
        }
    };

    let in_t = match in_point {
        Some(s) => parse_time(s).with_context(|| format!("invalid --in time: {s:?}"))?,
        None => Seconds::from_integer(0),
    };
    let out_t = match out_point {
        Some(s) => parse_time(s).with_context(|| format!("invalid --out time: {s:?}"))?,
        None => doc
            .asset(asset_id)
            .and_then(|a| a.duration())
            .ok_or_else(|| anyhow::anyhow!("asset has unknown duration — pass --out"))?,
    };

    let asset = doc.asset(asset_id).context("asset vanished after import")?;
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
    doc.set_sequence(
        asset_fps,
        asset_size.map(|(w, h)| VideoSettings {
            width: w,
            height: h,
        }),
    );

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

pub(crate) fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

pub(crate) fn cmd_show(project_path: &Path, json: bool) -> Result<()> {
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

/// `nile seek [TIME]` — move the stored playhead (snapped to the frame grid,
/// clamped into the timeline) or print it. The TUI opens where it lands.
pub(crate) fn cmd_seek(project_path: &Path, at: Option<&str>) -> Result<()> {
    let mut project = Project::load(project_path)?;
    let Some(s) = at else {
        say!("{}", format_seconds(&project.playhead));
        return Ok(());
    };
    let t = parse_signed_time(s).with_context(|| format!("invalid time {s:?}"))?;
    let landed = project.set_playhead(secs_f64(&t));
    let mut doc = Document::new(project);
    doc.save(project_path)?;
    log::line(&format!("seek -> {landed} s"));
    let frame = frame_of(doc.fps.as_ref(), landed);
    match doc.fps {
        Some(_) => say!(
            "✓ playhead → {} s (f{frame})",
            format_seconds(&doc.playhead)
        ),
        None => say!("✓ playhead → {} s", format_seconds(&doc.playhead)),
    }
    Ok(())
}

/// `nile media ls` — the pool listing. (`nile media rm` is a catalog
/// action; `main` routes it there, undoably.)
pub(crate) fn cmd_media_ls(project_path: &Path) -> Result<()> {
    let project = Project::load(project_path)?;
    if project.media.is_empty() {
        say!("media pool is empty — `nile import <file>`");
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

/// `nile track ls` — the track listing. (add/mute/rm are catalog actions.)
pub(crate) fn cmd_track_ls(project_path: &Path) -> Result<()> {
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

pub(crate) fn cmd_cache(project_path: &Path, action: &CacheCmd) -> Result<()> {
    match action {
        CacheCmd::Prune { days, dry_run } => {
            if *days == 0 {
                bail!("--days must be at least 1 (there is no --all)");
            }
            let max_age = std::time::Duration::from_secs(days.saturating_mul(86_400));
            let report = proxy::prune(project_path, max_age, *dry_run)?;
            let export = super::schedule::prune_export(project_path, max_age, *dry_run)?;
            let prefix = if *dry_run { "prune --dry-run" } else { "prune" };
            say!(
                "{prefix}: {}; export: {}",
                report.summary_named(*dry_run, "proxy render"),
                export.summary_named(*dry_run, "export chunk")
            );
            Ok(())
        }
    }
}

/// The human timeline report behind `nile show` (and the TUI's `:show`):
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
                    let frames = project
                        .fps
                        .map_or(0, |fps| (t.dur * fps).round().to_integer());
                    out += &format!(
                        "      [{si}] ⟶ transition #{} {}–{} ({frames}f)\n",
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
                    // A clip's effects apply in list order; show the kinds
                    // (the parameters are one `X` away in the panel).
                    let fx = if clip.effects.is_empty() {
                        String::new()
                    } else {
                        let kinds: Vec<&str> = clip.effects.iter().map(|e| e.kind()).collect();
                        format!("  · fx: {}", kinds.join(", "))
                    };
                    out += &format!(
                        "    [{si}] #{:<4}{:>9} – {:<9} {}{speed}  (src {}–{}){fx}\n",
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
