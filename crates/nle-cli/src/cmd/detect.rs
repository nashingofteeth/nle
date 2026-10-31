//! `nile detect-edits`: probe a rendered file for its cuts and rebuild the
//! timeline from the spans.

use std::path::Path;

use anyhow::{Context, Result, bail};

use nle_core::detect::{Span, reconstruct};
use nle_core::edit;
use nle_core::timeline::{Clip, Gap, Segment, Timeline, Track, TrackKind};
use nle_core::{Project, Seconds, VideoSettings, format_seconds, parse_time};
use nle_ffmpeg::detect::{DetectOptions, detect};
use nle_ffmpeg::probe;

use super::CommandReport;
use super::cli_index;
use super::project::save_and_reset_history;
use crate::parse_fps_arg;
/// Arguments for [`cmd_detect_edits`]. A parameter struct keeps the call
/// site readable and clippy's `too_many_arguments` quiet — the Rust
/// equivalent of "pass an options object" instead of a long positional list.
pub(crate) struct DetectArgs<'a> {
    pub(crate) project: &'a Path,
    pub(crate) file: &'a Path,
    pub(crate) threshold: f32,
    pub(crate) min_shot: &'a str,
    pub(crate) black_min: &'a str,
    pub(crate) no_gaps: bool,
    pub(crate) fps: Option<&'a str>,
    pub(crate) video_only: bool,
    pub(crate) list: bool,
    pub(crate) append: bool,
    pub(crate) track: Option<usize>,
    pub(crate) force: bool,
}

pub(crate) fn cmd_detect_edits(args: DetectArgs) -> Result<()> {
    detect_edits_work(args).map(|r| r.print())
}

/// The work behind `detect-edits` (and the TUI's `:detect`): warnings +
/// summary, no printing.
pub(crate) fn detect_edits_work(args: DetectArgs) -> Result<CommandReport> {
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
                let i = cli_index(n, "--track").map_err(anyhow::Error::msg)?;
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

        for n in edit::clamp_project(&mut project) {
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
    for n in edit::clamp_project(&mut project) {
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

/// Build the clip/gap segments for a run of spans, allocating clip ids from
/// `next_id` (unique across tracks).
pub(crate) fn span_segments(spans: &[Span], asset: u32, next_id: &mut u32) -> Vec<Segment> {
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
pub(crate) fn append_spans(
    project: &mut Project,
    ti: usize,
    start: Seconds,
    mut segments: Vec<Segment>,
) {
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
pub(crate) fn snap_fps_for(append_project: Option<&Project>, file_fps: Seconds) -> Seconds {
    append_project.and_then(|p| p.fps).unwrap_or(file_fps)
}

/// The next free clip id: one past the highest in use. Mirrors
/// `Project::next_clip_id`, which is crate-private (this is a separate crate).
pub(crate) fn next_clip_id(project: &Project) -> u32 {
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
