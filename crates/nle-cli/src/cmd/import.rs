//! `nile import`, `nile import-edl` and `nile import-otio`: probe media into
//! the pool and rebuild a timeline from an EDL or an OTIO document.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use nle_core::edit;
use nle_core::edl::{self, EdlEvent};
use nle_core::otio;
use nle_core::otio::OtioItem;
use nle_core::time::{parse_timecode, timecode_to_frames};
use nle_core::timeline::{Segment, TrackKind};
use nle_core::{Document, Project, VideoSettings, frame_of};
use nle_ffmpeg::probe;

use super::project::{save_and_reset_history, secs_f64};
use super::{CommandReport, asset_summary};
use crate::{parse_fps_arg, say};
pub(crate) fn cmd_import(project_path: &Path, files: &[PathBuf]) -> Result<()> {
    let mut doc = Document::with_history(
        Project::load(project_path)?,
        edit::History::load_project(project_path).unwrap_or_default(),
    );
    let playhead = frame_of(doc.fps.as_ref(), secs_f64(&doc.playhead));
    let mut added = 0usize;
    for file in files {
        let report = probe(file).with_context(|| format!("importing {}", file.display()))?;
        let canonical = file
            .canonicalize()
            .with_context(|| format!("resolving {}", file.display()))?;
        if let Some(a) = doc.asset_by_path(&canonical) {
            say!("· {} already imported (asset #{})", file.display(), a.id);
            continue;
        }
        // Recorded: `nile undo` removes the asset again.
        let (id, _) = doc.add_asset(canonical, report, playhead);
        added += 1;
        let asset = doc.asset(id).context("asset vanished")?;
        say!(
            "✓ asset #{id}  {}  {}",
            file.display(),
            asset_summary(asset)
        );
    }
    if added > 0 {
        doc.save(project_path)?;
        doc.save_history(&edit::History::sidecar_path(project_path))?;
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
        doc.media.len(),
        clip_count(&doc)
    );
    Ok(())
}

/// Media extensions probed when importing a `--media` directory.
pub(crate) const MEDIA_EXTS: &[&str] = &[
    "mp4", "mov", "mkv", "m4v", "avi", "webm", "wav", "mp3", "m4a", "aac", "flac", "ogg",
];

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
pub(crate) fn match_asset(project: &Project, needle: &str) -> Option<u32> {
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
pub(crate) fn unique(mut ids: Vec<u32>) -> Option<u32> {
    if ids.len() == 1 { ids.pop() } else { None }
}

/// Substring match at an edge: `red` matches `red_start` (as a prefix) and
/// `out_red` (as a suffix), but not inside `taxi` for needle `ax`.
pub(crate) fn fuzzy_edge(needle: &str, hay: &str) -> bool {
    hay.starts_with(needle)
        || hay.ends_with(needle)
        || needle.starts_with(hay)
        || needle.ends_with(hay)
}

/// Resolve an EDL event to an asset id: an explicit `--map` reel, then the
/// `SOURCE FILE`/`FROM CLIP NAME` filename, then the reel field.
pub(crate) fn resolve_reel(
    project: &Project,
    map: &HashMap<String, PathBuf>,
    ev: &EdlEvent,
) -> Option<u32> {
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
    // A black reel is empty space, not a clip: the fuzzy edge rule would
    // latch `BL` onto `blue.mp4`. Only an explicit `--map` or a
    // `SOURCE FILE`/`FROM CLIP NAME` hint resolves it; `build` turns the
    // unresolved black reel into a gap.
    if edl::is_black_reel(&ev.reel) {
        return None;
    }
    match_asset(project, &ev.reel)
}

pub(crate) fn cmd_import_edl(
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
pub(crate) fn import_edl_work(
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
            .filter(|e| edl::channel_has(&e.channel, 'V'))
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
    // A non-cut event (a dissolve, wipe, key) was imported as a cut: say so,
    // rather than silently dropping what the EDL asked for.
    for w in &doc.warnings {
        notes.push(format!("warning: {w}"));
    }
    // Keep every clip inside its media and on the frame grid: an EDL's source
    // out-point can run past the file, and the model must not disagree with
    // what export can render (see `edit::clamp_project`).
    for n in edit::clamp_project(&mut project) {
        notes.push(format!("warning: {n}"));
    }
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
pub(crate) fn pool_media_dir(project: &mut Project, dir: &Path) -> Result<(usize, Vec<String>)> {
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
pub(crate) fn resolve_otio_path(
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

pub(crate) fn cmd_import_otio(
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
pub(crate) fn import_otio_work(
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
    let clamp_notes = edit::clamp_project(&mut project);
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
