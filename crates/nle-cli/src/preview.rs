//! Instant preview: translate the timeline into an mpv `edl://` URL.
//!
//! mpv's EDL protocol concatenates trimmed segments of real files into one
//! virtual timeline — so cuts preview instantly, with no rendering, and the
//! playhead mpv reports back is already in *timeline* coordinates.
//!
//! Gaps become segments of a cached black file: mpv refuses protocol URLs
//! (like `av://lavfi:...`) inside EDL entries for security reasons, but
//! plain files are always allowed. Long gaps are split into chunks no
//! longer than the black file (mpv can't address past a segment's source
//! length), so one black file per geometry serves every gap length.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use nle_core::timeline::{Effect, Segment as TimelineSegment, Track, TransitionSource};
use nle_core::{Project, Seconds, format_seconds};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PreviewError {
    #[error("timeline is empty — add clips first")]
    Empty,
    #[error("no video track in timeline")]
    NoVideoTrack,
    #[error("asset #{0} is referenced but missing from the media pool")]
    MissingAsset(u32),
    #[error("timeline has gaps but no output size set — add a clip first")]
    NoSize,
    #[error("no frame rate set — add a clip first")]
    NoFps,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Proxy(#[from] crate::proxy::ProxyError),
}

/// Quote an EDL value with the `%len%` scheme. Allowed (and required) when
/// the value contains `,` or `;` — we always quote, so nothing is special.
pub fn edl_quote(value: &str) -> String {
    format!("%{}%{}", value.len(), value)
}

/// One preview segment in timeline order.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Clip {
        path: PathBuf,
        /// Source in-point (seconds).
        start: Seconds,
        /// Segment duration (seconds).
        length: Seconds,
    },
    Gap {
        length: Seconds,
    },
}

/// Flatten track `ti` into ordered preview segments. A track shorter than
/// the timeline is padded with a trailing gap, so the monitor spans the
/// whole timeline instead of stopping at the track's own end.
pub fn segments(project: &Project, ti: usize) -> Result<Vec<Segment>, PreviewError> {
    let track = project
        .timeline
        .track(ti)
        .ok_or(PreviewError::NoVideoTrack)?;
    let total = project.timeline.end();
    if track.segments.is_empty() {
        // An empty track is all gap — preview the whole timeline as black
        // (unless the timeline itself is empty).
        return if total > Seconds::from_integer(0) {
            Ok(vec![Segment::Gap { length: total }])
        } else {
            Err(PreviewError::Empty)
        };
    }

    let mut segs = Vec::new();
    for seg in &track.segments {
        // Effects, a retime, a transition and an off-sequence size can't be
        // expressed by `edl://`: play the rendered proxy instead of the raw
        // source.
        if crate::proxy::needs_proxy(project, seg) {
            let p = crate::proxy::proxy_for(project, seg)?;
            segs.push(Segment::Clip {
                path: p,
                start: Seconds::from_integer(0),
                length: seg.duration(),
            });
            continue;
        }
        match seg {
            TimelineSegment::Clip(c) => {
                let asset = project
                    .asset(c.asset)
                    .ok_or(PreviewError::MissingAsset(c.asset))?;
                segs.push(Segment::Clip {
                    path: asset.path.clone(),
                    start: c.asset_in,
                    length: c.duration(),
                });
            }
            TimelineSegment::Gap(g) => segs.push(Segment::Gap { length: g.duration }),
            // `needs_proxy` covers transitions; keep a safe fallback.
            TimelineSegment::Transition(tr) => segs.push(Segment::Gap { length: tr.dur }),
        }
    }
    let tail = total - track.end();
    if tail > Seconds::from_integer(0) {
        segs.push(Segment::Gap { length: tail });
    }
    Ok(segs)
}

/// One interval of the composite: `[start, end)` and what plays there.
#[derive(Debug)]
struct Window<'a> {
    start: Seconds,
    end: Seconds,
    kind: WindowKind<'a>,
}

#[derive(Debug)]
enum WindowKind<'a> {
    /// One track's material: `edl://` plays the segment (or its proxy),
    /// offset into it.
    Segment(&'a TimelineSegment, Seconds),
    /// More than one track contributes audio, **or** the top layer is masked
    /// (a crop) with picture below it — both need the rendered composite
    /// rather than one source's EDL entry.
    Mix,
    /// Every track is a gap: black.
    Gap,
}

/// The composite plan: the union of every unmuted track's cut points, with the
/// topmost segment picked per interval. A clip **or a transition** wins over
/// the layers below it — a transition is rendered material (its proxy), not a
/// hole; a gap falls through.
fn composite_windows(project: &Project) -> Result<Vec<Window<'_>>, PreviewError> {
    let total = project.timeline.end();
    if total <= Seconds::from_integer(0) {
        return Err(PreviewError::Empty);
    }
    // Stacking order is bottom→top, so the last video track is on top.
    let video: Vec<&Track> = project
        .timeline
        .video_tracks()
        .filter(|t| !t.muted)
        .collect();
    let audio: Vec<&Track> = project
        .timeline
        .audio_tracks()
        .filter(|t| !t.muted)
        .collect();
    // Union of every unmuted track's segment boundaries. Audio tracks are in
    // the cut set too: a composite span starts and ends where their material
    // does. A fully-muted video timeline is not an error — it plays black, as
    // a traditional editor shows.
    let mut cuts = vec![Seconds::from_integer(0), total];
    for t in video.iter().chain(audio.iter()) {
        let mut acc = Seconds::from_integer(0);
        for seg in &t.segments {
            cuts.push(acc);
            acc += seg.duration();
            cuts.push(acc);
        }
    }
    cuts.sort();
    cuts.dedup();

    let mut windows = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b <= a {
            continue;
        }
        // The topmost track with material (a clip or a transition) covering
        // `[a, b)`; a gap falls through to the layer below.
        let picked = video.iter().enumerate().rev().find_map(|(vi, t)| {
            let si = t.index_at(a)?;
            match &t.segments[si] {
                TimelineSegment::Clip(_) | TimelineSegment::Transition(_) => {
                    Some((vi, &t.segments[si], t.start_of(si)))
                }
                TimelineSegment::Gap(_) => None,
            }
        });
        // The monitor's sound for a window is the picked source's own audio;
        // the export sums every track's. A window where any *other* track is
        // audible needs the rendered composite (or the mix is wrong — or, with
        // an audio track, silent).
        let mixed = other_track_has_audio(
            project,
            &video,
            &audio,
            a,
            picked.as_ref().map(|(vi, ..)| *vi),
        );
        // A cropped or scaled-down layer is transparent at its edges, so a
        // lower picture must show through: only the export's overlay can do it.
        let masked = match &picked {
            Some((vi, seg, _)) => {
                masks_edges(project, seg) && lower_track_has_video(&video, *vi, a)
            }
            None => false,
        };
        let kind = match picked {
            _ if mixed || masked => WindowKind::Mix,
            Some((_, seg, seg_start)) => WindowKind::Segment(seg, a - seg_start),
            None => WindowKind::Gap,
        };
        windows.push(Window {
            start: a,
            end: b,
            kind,
        });
    }
    Ok(windows)
}

/// Does this segment carry sound? (A transition carries its sides'.)
fn audible(project: &Project, seg: &TimelineSegment) -> bool {
    let heard = |id: u32| project.asset(id).is_some_and(|a| a.probe.has_audio());
    match seg {
        TimelineSegment::Clip(c) => heard(c.asset),
        TimelineSegment::Transition(t) => {
            t.a.asset().is_some_and(heard) || t.b.asset().is_some_and(heard)
        }
        TimelineSegment::Gap(_) => false,
    }
}

/// Is any track other than the picked video track audible at `at`? The picked
/// one is what the cheap path already plays.
fn other_track_has_audio(
    project: &Project,
    video: &[&Track],
    audio: &[&Track],
    at: Seconds,
    picked: Option<usize>,
) -> bool {
    let audible_at = |t: &Track| {
        t.index_at(at)
            .is_some_and(|si| audible(project, &t.segments[si]))
    };
    video
        .iter()
        .enumerate()
        .any(|(vi, t)| Some(vi) != picked && audible_at(t))
        || audio.iter().any(|t| audible_at(t))
}

/// Does this segment leave transparent canvas (a crop, or a scale-down)? The
/// monitor then needs the layers below composited, not the clip's own proxy.
fn masks_edges(project: &Project, seg: &TimelineSegment) -> bool {
    match seg {
        TimelineSegment::Clip(c) => project
            .asset(c.asset)
            .and_then(|a| a.video_size())
            .is_some_and(|size| Effect::masks(&c.effects, size)),
        // A transition side can carry its neighbour's effects; the blend is
        // rendered material, so ask the same question of both sides.
        TimelineSegment::Transition(t) => [&t.a, &t.b].into_iter().any(|side| match side {
            TransitionSource::Clip { asset, effects, .. } => project
                .asset(*asset)
                .and_then(|a| a.video_size())
                .is_some_and(|size| Effect::masks(effects, size)),
            TransitionSource::Black => false,
        }),
        TimelineSegment::Gap(_) => false,
    }
}

/// Is any track *below* `picked` showing picture at `at`? `video` is in
/// stacking order (bottom first), so everything before `picked` is underneath.
fn lower_track_has_video(video: &[&Track], picked: usize, at: Seconds) -> bool {
    video[..picked].iter().any(|t| {
        t.index_at(at).is_some_and(|si| {
            matches!(
                t.segments[si],
                TimelineSegment::Clip(_) | TimelineSegment::Transition(_)
            )
        })
    })
}

/// Spans that must be rendered as a composite: runs of `Mix` windows, widened
/// to contain any transition they straddle — a partial crossfade isn't
/// expressible (`xfade` always blends 0→1) — then merged.
///
/// Widening only moves a bound to a transition's edge, which is itself a cut
/// point, so every span still starts and ends on a window boundary.
fn composite_spans(
    windows: &[Window<'_>],
    transitions: &[(Seconds, Seconds)],
) -> Vec<(Seconds, Seconds)> {
    let mut spans: Vec<(Seconds, Seconds)> = Vec::new();
    for w in windows {
        if !matches!(w.kind, WindowKind::Mix) {
            continue;
        }
        match spans.last_mut() {
            Some(last) if last.1 == w.start => last.1 = w.end,
            _ => spans.push((w.start, w.end)),
        }
    }
    loop {
        let mut grew = false;
        for span in spans.iter_mut() {
            for &(ts, te) in transitions {
                if ts < span.1 && te > span.0 && (ts < span.0 || te > span.1) {
                    span.0 = span.0.min(ts);
                    span.1 = span.1.max(te);
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    spans.sort_by_key(|s| s.0);
    let mut merged: Vec<(Seconds, Seconds)> = Vec::new();
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.0 <= last.1 => last.1 = last.1.max(span.1),
            _ => merged.push(span),
        }
    }
    merged
}

/// Every transition's `[start, end)` on an unmuted video track.
fn transition_spans(project: &Project) -> Vec<(Seconds, Seconds)> {
    let mut out = Vec::new();
    for track in project.timeline.video_tracks().filter(|t| !t.muted) {
        let mut pos = Seconds::from_integer(0);
        for seg in &track.segments {
            let end = pos + seg.duration();
            if matches!(seg, TimelineSegment::Transition(_)) {
                out.push((pos, end));
            }
            pos = end;
        }
    }
    out
}

/// Flatten the video tracks into one preview sequence, topmost first: at
/// each instant the highest unmuted video track with material wins, and only
/// where every track is a gap does the preview show black. This is what the
/// monitor plays, so a gap in an upper layer reveals the layer below rather
/// than stopping playback.
///
/// Two things `edl://` can't express go through the proxy cache:
/// - a segment with effects, a non-1× retime, a transition, or an
///   off-sequence picture size — that window plays the rendered segment,
///   offset into it like any source;
/// - a window where another track is audible — that span plays the rendered
///   *composite* (video `overlay`ed, audio `amix`ed), merged with its
///   neighbours so one render covers the run.
///
/// Render progress: `(finished, total, fraction of the render in flight)`.
pub type ProgressFn<'a> = &'a mut dyn FnMut(usize, usize, f64);

pub fn composite_segments(project: &Project) -> Result<Vec<Segment>, PreviewError> {
    composite_segments_with(project, &mut |_, _, _| {})
}

/// One proxy plan step. `Window` may or may not need a proxy; `Span` always
/// renders a composite. Kept as a list so the caller can count the renders
/// before starting (the status row needs "2/5").
enum Step<'a> {
    Window(Window<'a>),
    Span(Seconds, Seconds),
}

/// [`composite_segments`] with render progress: `(finished, total, fraction of
/// the one in flight)`.
pub fn composite_segments_with(
    project: &Project,
    progress: ProgressFn<'_>,
) -> Result<Vec<Segment>, PreviewError> {
    let steps = composite_steps(project)?;
    let total = steps
        .iter()
        .filter(|s| step_needs_proxy(project, s))
        .count();
    // Announce the plan before the first render, so a fully cached build still
    // shows "0/2" instead of "building…" until it finishes.
    progress(0, total, 0.0);
    let mut done = 0;
    let mut segs: Vec<Segment> = Vec::new();
    for step in steps {
        match step {
            Step::Window(window) => {
                push_window(
                    project,
                    &window,
                    &mut segs,
                    &mut *progress,
                    &mut done,
                    total,
                )?;
            }
            Step::Span(start, end) => {
                let mut report = |f: f64| progress(done, total, f);
                let path = crate::proxy::composite_proxy_with(project, start, end, &mut report)?;
                done += 1;
                segs.push(Segment::Clip {
                    path,
                    start: Seconds::from_integer(0),
                    length: end - start,
                });
            }
        }
    }
    Ok(segs)
}

/// Build the ordered plan without rendering anything: windows outside a mix
/// span, and the spans themselves.
fn composite_steps(project: &Project) -> Result<Vec<Step<'_>>, PreviewError> {
    let windows = composite_windows(project)?;
    let spans = composite_spans(&windows, &transition_spans(project));
    let mut it = windows.into_iter().peekable();
    let mut steps = Vec::new();
    for (start, end) in spans {
        while it.peek().is_some_and(|w| w.start < start) {
            if let Some(w) = it.next() {
                steps.push(Step::Window(w));
            }
        }
        steps.push(Step::Span(start, end));
        while it.peek().is_some_and(|w| w.end <= end) {
            it.next();
        }
    }
    for w in it {
        steps.push(Step::Window(w));
    }
    Ok(steps)
}

fn step_needs_proxy(project: &Project, step: &Step<'_>) -> bool {
    match step {
        Step::Span(..) => true,
        Step::Window(w) => window_needs_proxy(project, w),
    }
}

fn window_needs_proxy(project: &Project, window: &Window<'_>) -> bool {
    match &window.kind {
        WindowKind::Segment(seg, _) => crate::proxy::needs_proxy(project, seg),
        WindowKind::Mix | WindowKind::Gap => false,
    }
}

/// One non-composite window: its own source, its proxy, or black.
fn push_window(
    project: &Project,
    window: &Window<'_>,
    segs: &mut Vec<Segment>,
    progress: ProgressFn<'_>,
    done: &mut usize,
    total: usize,
) -> Result<(), PreviewError> {
    let len = window.end - window.start;
    match &window.kind {
        WindowKind::Gap => push_gap(segs, len),
        WindowKind::Segment(seg, offset) => {
            if crate::proxy::needs_proxy(project, seg) {
                let mut report = |f: f64| progress(*done, total, f);
                let path = crate::proxy::proxy_for_with(project, seg, &mut report)?;
                *done += 1;
                segs.push(Segment::Clip {
                    path,
                    start: *offset,
                    length: len,
                });
            } else if let TimelineSegment::Clip(c) = seg {
                let asset = project
                    .asset(c.asset)
                    .ok_or(PreviewError::MissingAsset(c.asset))?;
                segs.push(Segment::Clip {
                    path: asset.path.clone(),
                    // `offset` is inside the clip: offset into its source.
                    start: c.asset_in + *offset,
                    length: len,
                });
            }
            // Nothing else reaches here: only a clip gets through without a
            // proxy, since every transition needs one.
        }
        WindowKind::Mix => {
            debug_assert!(false, "a mix window is always inside a composite span");
            push_gap(segs, len);
        }
    }
    Ok(())
}

/// Append a gap, fusing it with a preceding gap so a black span is one
/// entry.
fn push_gap(segs: &mut Vec<Segment>, length: Seconds) {
    if let Some(Segment::Gap { length: last }) = segs.last_mut() {
        *last += length;
    } else {
        segs.push(Segment::Gap { length });
    }
}

/// Length of the cached black filler file in seconds. Gaps longer than
/// this are emitted as multiple EDL entries (mpv can't seek past a
/// segment's source length).
pub const BLACK_FILE_SECS: i64 = 10;
/// Render segments to EDL entries (the part after `edl://`).
/// `black` is required only when a gap exists. Gaps longer than the
/// black file are split into `BLACK_FILE_SECS`-sized chunks.
pub fn assemble_edl(segments: &[Segment], black: Option<&Path>) -> Result<String, PreviewError> {
    let mut entries = Vec::new();
    for seg in segments {
        match seg {
            Segment::Clip {
                path,
                start,
                length,
            } => entries.push(format!(
                "{},{}",
                edl_quote(&path.display().to_string()),
                fmt_entry(*start, *length)
            )),
            Segment::Gap { length } => {
                let black = black.ok_or(PreviewError::NoSize)?;
                let quoted = edl_quote(&black.display().to_string());
                let chunk = Seconds::from_integer(BLACK_FILE_SECS);
                let mut remaining = *length;
                while remaining > chunk {
                    entries.push(format!("{quoted},0,{}", format_seconds(&chunk)));
                    remaining -= chunk;
                }
                entries.push(format!("{quoted},0,{}", format_seconds(&remaining)));
            }
        }
    }
    Ok(entries.join(";"))
}

fn fmt_entry(start: Seconds, length: Seconds) -> String {
    format!("{},{}", format_seconds(&start), format_seconds(&length))
}

/// A single-source preview for extend mode: mpv position 0 corresponds
/// to `origin` in timeline coordinates, so seeks translate by
/// subtracting it (`mpv = playhead - origin`). Clip previews show the
/// armed asset's real frames across the whole workable range; black
/// previews (gap work, which has no material) show blank.
pub struct ExtendPreview {
    pub url: String,
    /// Timeline position of mpv position 0.
    pub origin: f64,
}

/// Material preview for a clip-armed edge: the asset trimmed to the
/// source span covering the workable range. `origin` is the timeline
/// position mpv 0 maps to; `src_start`/`length` are asset-relative.
pub fn build_clip_preview(
    path: &Path,
    src_start: Seconds,
    length: Seconds,
    origin: f64,
) -> ExtendPreview {
    ExtendPreview {
        url: format!(
            "edl://{},{}",
            edl_quote(&path.display().to_string()),
            fmt_entry(src_start, length)
        ),
        origin,
    }
}

/// Blank preview for gap-armed edges: the cached black file, repeated
/// in `BLACK_FILE_SECS` chunks to cover `length` (mpv can't address
/// past a segment's source length, so — like timeline gaps — long spans
/// become multiple entries). No loop flag needed: seeks stay valid
/// across the whole span.
pub fn build_black_preview(
    project: &Project,
    origin: f64,
    length: Seconds,
) -> Result<ExtendPreview, PreviewError> {
    let size = project.video.as_ref().ok_or(PreviewError::NoSize)?;
    let fps = project.fps.ok_or(PreviewError::NoFps)?;
    let black = ensure_black_file(size.width, size.height, &fps)?;
    let url = format!(
        "edl://{}",
        assemble_edl(&[Segment::Gap { length }], Some(&black))?
    );
    Ok(ExtendPreview { url, origin })
}

/// How far past the timeline end a gap-armed preview reaches. Repeating
/// the 10s black file costs ~40 bytes per chunk in the URL, so an hour
/// of navigable black is cheap; the terminal row projects beyond it
/// anyway (mpv simply holds black past the last chunk).
pub const EXTEND_PREVIEW_SECS: i64 = 3600;

/// Build the full `edl://` URL for track `ti`, generating the black gap
/// file if the timeline has gaps. A single track is the debugging view
/// (`nile edl --track N`); the monitor — and bare `nile edl` — plays the
/// composite (see `build_composite_edl`).
pub fn build_edl(project: &Project, ti: usize) -> Result<String, PreviewError> {
    edl_from_segments(project, &segments(project, ti)?)
}

/// Build the `edl://` URL for the whole timeline — the composite of every
/// unmuted video track, topmost clip winning. This is what the monitor
/// plays, so a gap falls through to the layer below.
pub fn build_composite_edl(project: &Project) -> Result<String, PreviewError> {
    edl_from_segments(project, &composite_segments(project)?)
}

/// [`build_composite_edl`] with render progress.
pub fn build_composite_edl_with(
    project: &Project,
    progress: ProgressFn<'_>,
) -> Result<String, PreviewError> {
    edl_from_segments(project, &composite_segments_with(project, progress)?)
}

/// Render preview segments to an `edl://` URL, generating the cached black
/// filler file when a gap is present.
fn edl_from_segments(project: &Project, segs: &[Segment]) -> Result<String, PreviewError> {
    let has_gap = segs.iter().any(|s| matches!(s, Segment::Gap { .. }));
    let black = if has_gap {
        let size = project.video.as_ref().ok_or(PreviewError::NoSize)?;
        let fps = project.fps.ok_or(PreviewError::NoFps)?;
        Some(ensure_black_file(size.width, size.height, &fps)?)
    } else {
        None
    };
    Ok(format!("edl://{}", assemble_edl(segs, black.as_deref())?))
}

/// Cache dir for black gap files: `$XDG_CACHE_HOME/nle/black` or `~/.cache/nle/black`.
pub fn black_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("nile").join("black")
}

/// Ensure a silent black clip of `BLACK_FILE_SECS` exists for this
/// geometry and return its path. EDL entries reference it in chunks of at
/// most that length, so one file per WxH+fps serves all projects on this
/// machine.
pub fn ensure_black_file(
    width: u32,
    height: u32,
    fps: &Seconds,
) -> Result<PathBuf, std::io::Error> {
    let dir = black_cache_dir();
    fs::create_dir_all(&dir)?;
    let name = format!("{width}x{height}_{}_{}.mp4", fps.numer(), fps.denom());
    let path = dir.join(name.clone());
    // A cache hit must be a *complete* file (see the proxy cache): a
    // zero-length one would be served forever.
    if fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
        return Ok(path);
    }
    // Write to a temp name (still ending in .mp4 so ffmpeg can infer the
    // muxer), then rename — a crash mid-generate can't leave a half-written
    // file that later loads "successfully".
    let tmp = dir.join(crate::proxy::tmp_file(&name));
    let fps_s = format!("{}/{}", fps.numer(), fps.denom());
    let len_s = BLACK_FILE_SECS.to_string();
    // Captured, not inherited: this runs from the monitor's reload too, and
    // ffmpeg must not paint over the TUI. The reason rides in the error.
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=black:s={width}x{height}:r={fps_s}"),
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-t",
            &len_s,
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "40",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
        ])
        .arg("--")
        .arg(&tmp)
        .output()?;
    if !out.status.success() {
        let _ = fs::remove_file(&tmp);
        return Err(std::io::Error::other(format!(
            "ffmpeg failed to generate the black gap file: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::VideoSettings;
    use nle_core::media::{RawFormat, RawProbe, RawStream};
    use nle_core::timeline::{
        Clip as TimelineClip, Effect, Gap as TimelineGap, Segment as TimelineSegment, Transition,
        TransitionKind, TransitionSource,
    };

    fn seg_clip(id: u32, ain: i64, aout: i64) -> TimelineSegment {
        TimelineSegment::Clip(TimelineClip {
            id,
            asset: 1,
            asset_in: Seconds::from_integer(ain),
            asset_out: Seconds::from_integer(aout),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    }

    // A fully-muted video timeline previews as black, not an error — the
    // monitor keeps working and shows nothing rather than the muted composite.
    #[test]
    fn a_muted_video_timeline_previews_as_black() {
        let secs = Seconds::from_integer;
        let mut p = Project::new();
        p.fps = Some(secs(30));
        let probe = RawProbe {
            streams: vec![RawStream {
                index: 0,
                codec_type: Some("video".into()),
                width: Some(320),
                height: Some(240),
                ..Default::default()
            }],
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some("10".into()),
                ..Default::default()
            },
        };
        p.add_asset("/media/red.mp4".into(), probe);
        p.timeline.tracks[0].segments = vec![seg_clip(1, 0, 2)];
        p.timeline.tracks[0].muted = true;
        let windows = composite_windows(&p).unwrap();
        assert!(!windows.is_empty(), "the timeline still previews");
        assert!(
            windows.iter().all(|w| matches!(w.kind, WindowKind::Gap)),
            "every window is black"
        );
    }

    #[test]
    fn a_composite_span_never_cuts_a_transition() {
        let secs = Seconds::from_integer;
        let w = |start: Seconds, end: Seconds, mix: bool| Window {
            start,
            end,
            kind: if mix {
                WindowKind::Mix
            } else {
                WindowKind::Gap
            },
        };

        // A mix run [1,3) with a transition [1.5,2.5) inside it: already
        // contained, so the span is unchanged.
        let windows = vec![
            w(secs(0), secs(1), false),
            w(secs(1), secs(2), true),
            w(secs(2), secs(3), true),
            w(secs(3), secs(4), false),
        ];
        assert_eq!(
            composite_spans(&windows, &[(Seconds::new(3, 2), Seconds::new(5, 2))]),
            vec![(secs(1), secs(3))]
        );

        // A transition straddling the run's edge widens it (the windows are
        // cut at the transition's edges too, since those are cut points).
        let windows = vec![
            w(secs(0), Seconds::new(1, 2), false),
            w(Seconds::new(1, 2), secs(1), false),
            w(secs(1), secs(2), true),
            w(secs(2), Seconds::new(5, 2), false),
            w(Seconds::new(5, 2), secs(3), false),
        ];
        assert_eq!(
            composite_spans(&windows, &[(Seconds::new(1, 2), Seconds::new(5, 2))]),
            vec![(Seconds::new(1, 2), Seconds::new(5, 2))]
        );

        // One transition straddling two runs pulls them into a single span.
        let windows = vec![
            w(secs(0), secs(1), false),
            w(secs(1), secs(2), true),
            w(secs(2), secs(3), false),
            w(secs(3), secs(4), true),
        ];
        assert_eq!(
            composite_spans(&windows, &[(Seconds::new(3, 2), Seconds::new(7, 2))]),
            vec![(secs(1), secs(4))]
        );
    }

    // The composite picks a transition over the layer below (it is rendered
    // material) and routes it through the proxy cache.
    #[test]
    fn composite_plan_treats_a_transition_as_material_to_proxy() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (low, _) = p.add_asset("/media/low.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![seg_clip(1, 0, 4)];
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![TimelineSegment::Transition(Transition {
            id: 9,
            dur: Seconds::from_integer(1),
            kind: TransitionKind::Dissolve,
            a: TransitionSource::Clip {
                asset: low,
                src_in: Seconds::from_integer(0),
                src_out: Seconds::from_integer(1),
                effects: Vec::new(),
            },
            b: TransitionSource::Black,
        })];

        let windows = composite_windows(&p).unwrap();
        assert_eq!(windows.len(), 2, "the transition's cuts split the low clip");
        match &windows[0] {
            Window {
                start,
                end,
                kind: WindowKind::Segment(seg, offset),
            } => {
                assert!(matches!(seg, TimelineSegment::Transition(_)));
                assert_eq!(
                    (*start, *end),
                    (Seconds::from_integer(0), Seconds::from_integer(1))
                );
                assert_eq!(*offset, Seconds::from_integer(0));
                assert!(
                    crate::proxy::needs_proxy(&p, seg),
                    "a transition is rendered to a proxy"
                );
            }
            other => panic!("expected the transition to win, got {other:?}"),
        }
        // Below the top track's end, the low clip shows through.
        match &windows[1] {
            Window {
                kind: WindowKind::Segment(TimelineSegment::Clip(c), offset),
                ..
            } => {
                assert_eq!((c.asset, *offset), (low, Seconds::from_integer(1)));
            }
            other => panic!("expected the low clip, got {other:?}"),
        }
    }

    #[test]
    fn a_window_with_another_audible_track_is_a_composite() {
        // `probe_report` is video-only, so an overlap of two *video* layers is
        // still one source's sound. Give the assets audio and the overlap needs
        // the rendered mix.
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let mut with_audio = probe_report("10");
        with_audio.streams.push(RawStream {
            index: 1,
            codec_type: Some("audio".into()),
            codec_name: Some("aac".into()),
            ..Default::default()
        });
        let (low, _) = p.add_asset("/media/low.mp4".into(), with_audio.clone());
        let (high, _) = p.add_asset("/media/high.mp4".into(), with_audio);
        p.timeline.tracks[0].segments = vec![seg_clip(1, 0, 4)];
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(1),
            }),
            TimelineSegment::Clip(TimelineClip {
                id: 2,
                asset: high,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(1),
            }),
        ];
        assert_eq!(low, 1);
        let mixed: Vec<bool> = composite_windows(&p)
            .unwrap()
            .iter()
            .map(|w| matches!(w.kind, WindowKind::Mix))
            .collect();
        assert_eq!(
            mixed,
            vec![false, true, false],
            "[0,1) one source, [1,3) both layers audible, [3,4) one source"
        );

        // Muting the overlay leaves one audible track: the cheap path again.
        p.timeline.tracks[ti].muted = true;
        assert!(
            composite_windows(&p)
                .unwrap()
                .iter()
                .all(|w| !matches!(w.kind, WindowKind::Mix)),
            "a muted layer is silent and invisible"
        );
    }

    #[test]
    fn an_audio_track_makes_its_span_a_composite() {
        // A separate audio track is inaudible through `edl://` (one source per
        // entry), so its span must be rendered and mixed.
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let mut with_audio = probe_report("10");
        with_audio.streams.push(RawStream {
            index: 1,
            codec_type: Some("audio".into()),
            codec_name: Some("aac".into()),
            ..Default::default()
        });
        let (_a, _) = p.add_asset("/media/red.mp4".into(), with_audio);
        p.timeline.tracks[0].segments = vec![seg_clip(1, 0, 4)];
        let ai = p.timeline.add_track(nle_core::timeline::TrackKind::Audio);
        p.timeline.tracks[ai].segments = vec![
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(1),
            }),
            seg_clip(2, 0, 1),
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
        ];
        let mixed: Vec<bool> = composite_windows(&p)
            .unwrap()
            .iter()
            .map(|w| matches!(w.kind, WindowKind::Mix))
            .collect();
        assert_eq!(
            mixed,
            vec![false, true, false],
            "[1,2) is where the audio track has material"
        );
    }

    #[test]
    fn composite_plan_proxies_effect_and_speed_clips() {
        // An effect clip…
        let mut p = composite_project();
        if let TimelineSegment::Clip(c) = &mut p.timeline.tracks[1].segments[1] {
            c.effects = vec![Effect::Eq {
                brightness: 0.1,
                contrast: 1.0,
                saturation: 1.0,
            }];
        }
        let windows = composite_windows(&p).unwrap();
        match &windows[1] {
            Window {
                kind: WindowKind::Segment(seg, _),
                ..
            } => assert!(crate::proxy::needs_proxy(&p, seg), "{seg:?}"),
            other => panic!("expected the top clip window, got {other:?}"),
        }

        // …and a retimed clip (an EDL entry would play it at 1×).
        let mut p = composite_project();
        if let TimelineSegment::Clip(c) = &mut p.timeline.tracks[1].segments[1] {
            c.speed = Seconds::new(1, 2);
        }
        let windows = composite_windows(&p).unwrap();
        match &windows[1] {
            Window {
                kind: WindowKind::Segment(seg, _),
                ..
            } => assert!(crate::proxy::needs_proxy(&p, seg), "{seg:?}"),
            other => panic!("expected the top clip window, got {other:?}"),
        }
    }

    // A crop or a scale-down is transparent at its edges: with picture below,
    // the monitor needs the export's overlay (a rendered composite), not the
    // clip's own proxy.
    #[test]
    fn a_masked_layer_over_picture_is_a_composite() {
        for effect in [
            Effect::Crop {
                left: 10.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            },
            Effect::Scale { w: 50.0, h: 50.0 },
            Effect::Offset { x: 10.0, y: 0.0 },
        ] {
            let mut p = composite_project();
            if let TimelineSegment::Clip(c) = &mut p.timeline.tracks[1].segments[1] {
                c.effects = vec![effect];
            }
            let mixed: Vec<bool> = composite_windows(&p)
                .unwrap()
                .iter()
                .map(|w| matches!(w.kind, WindowKind::Mix))
                .collect();
            assert_eq!(
                mixed,
                vec![false, true, false],
                "the masked high clip must reveal the low clip"
            );
        }

        // A zoom (above 100%) still covers the canvas: no composite.
        let mut p = composite_project();
        if let TimelineSegment::Clip(c) = &mut p.timeline.tracks[1].segments[1] {
            c.effects = vec![Effect::Scale { w: 200.0, h: 200.0 }];
        }
        let mixed: Vec<bool> = composite_windows(&p)
            .unwrap()
            .iter()
            .map(|w| matches!(w.kind, WindowKind::Mix))
            .collect();
        assert_eq!(mixed, vec![false, false, false]);

        // With nothing below (the low track is a gap), the clip's own proxy
        // already shows black margins — no composite render is needed.
        let mut p = composite_project();
        if let TimelineSegment::Clip(c) = &mut p.timeline.tracks[1].segments[1] {
            c.effects = vec![Effect::Crop {
                left: 10.0,
                right: 0.0,
                top: 0.0,
                bottom: 0.0,
            }];
        }
        p.timeline.tracks[0].segments = vec![TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(4),
        })];
        let mixed: Vec<bool> = composite_windows(&p)
            .unwrap()
            .iter()
            .map(|w| matches!(w.kind, WindowKind::Mix))
            .collect();
        assert_eq!(mixed, vec![false, false, false]);
    }

    // The plan is built before any render so the status row can say "2/5":
    // count the windows/spans that need a proxy without running ffmpeg.
    #[test]
    fn the_proxy_plan_counts_renders_before_starting() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings {
            width: 320,
            height: 240,
        });
        let _ = p.add_asset("/media/red.mp4".into(), probe_report("10"));
        p.timeline.video_track_mut().unwrap().segments = vec![seg_clip(1, 0, 2), seg_clip(2, 0, 2)];
        for seg in &mut p.timeline.video_track_mut().unwrap().segments {
            if let TimelineSegment::Clip(c) = seg {
                c.effects = vec![Effect::Scale { w: 2.0, h: 2.0 }];
            }
        }
        let steps = composite_steps(&p).unwrap();
        assert_eq!(
            steps.iter().filter(|s| step_needs_proxy(&p, s)).count(),
            2,
            "one render per effect clip"
        );

        // Without effects the plan still exists but needs no renders.
        let mut q = Project::new();
        q.fps = p.fps;
        q.video = p.video.clone();
        let _ = q.add_asset("/media/red.mp4".into(), probe_report("10"));
        q.timeline.video_track_mut().unwrap().segments = vec![seg_clip(1, 0, 2), seg_clip(2, 0, 2)];
        let steps = composite_steps(&q).unwrap();
        assert_eq!(steps.iter().filter(|s| step_needs_proxy(&q, s)).count(), 0);
    }

    fn project_with_gap() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings {
            width: 320,
            height: 240,
        });
        let (_a, _) = p.add_asset("/media/red.mp4".into(), probe_report("5"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            seg_clip(1, 1, 3), // [0..2) src 1..3
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(3),
            }),
            seg_clip(2, 0, 3), // [5..8) src 0..3
        ];
        p
    }

    fn probe_report(duration: &str) -> nle_core::RawProbe {
        RawProbe {
            streams: vec![RawStream {
                index: 0,
                codec_type: Some("video".into()),
                codec_name: Some("h264".into()),
                width: Some(320),
                height: Some(240),
                r_frame_rate: Some("30/1".into()),
                ..Default::default()
            }],
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn segments_flatten_clips_and_gaps_in_order() {
        let p = project_with_gap();
        let segs = segments(&p, 0).unwrap();
        assert_eq!(
            segs,
            vec![
                Segment::Clip {
                    path: "/media/red.mp4".into(),
                    start: Seconds::from_integer(1),
                    length: Seconds::from_integer(2),
                },
                Segment::Gap {
                    length: Seconds::from_integer(3)
                },
                Segment::Clip {
                    path: "/media/red.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(3),
                },
            ]
        );
    }

    #[test]
    fn shorter_track_previews_its_tail_as_a_gap() {
        let mut p = project_with_gap(); // track 0 ends at 8 s
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![seg_clip(9, 0, 2)]; // 2 s
        let segs = segments(&p, ti).unwrap();
        assert_eq!(
            segs,
            vec![
                Segment::Clip {
                    path: "/media/red.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(2),
                },
                // Padded to the 8 s timeline so playback doesn't stop at 2 s.
                Segment::Gap {
                    length: Seconds::from_integer(6)
                },
            ]
        );
    }

    #[test]
    fn empty_track_previews_the_whole_timeline_as_black() {
        let mut p = project_with_gap(); // timeline ends at 8 s
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        let segs = segments(&p, ti).unwrap();
        assert_eq!(
            segs,
            vec![Segment::Gap {
                length: Seconds::from_integer(8)
            }]
        );
    }

    // Two video tracks: low holds a 4 s clip; high is gap / 1 s clip / gap.
    fn composite_project() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (low, _) = p.add_asset("/media/low.mp4".into(), probe_report("10"));
        let (high, _) = p.add_asset("/media/high.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![TimelineSegment::Clip(TimelineClip {
            id: 1,
            asset: low,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(1),
            }),
            TimelineSegment::Clip(TimelineClip {
                id: 2,
                asset: high,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(1),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
        ];
        p
    }

    #[test]
    fn composite_picks_the_topmost_clip_and_falls_through_gaps() {
        let p = composite_project();
        let segs = composite_segments(&p).unwrap();
        assert_eq!(
            segs,
            vec![
                // [0,1) top is a gap → the low clip shows, source 0..1.
                Segment::Clip {
                    path: "/media/low.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(1),
                },
                // [1,2) top clip wins.
                Segment::Clip {
                    path: "/media/high.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(1),
                },
                // [2,4) top is a gap again → low clip, source 2..4.
                Segment::Clip {
                    path: "/media/low.mp4".into(),
                    start: Seconds::from_integer(2),
                    length: Seconds::from_integer(2),
                },
            ]
        );
    }

    #[test]
    fn composite_top_clip_wins_over_a_lower_clip() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let _ = p.add_asset("/media/low.mp4".into(), probe_report("10"));
        let (high, _) = p.add_asset("/media/high.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![seg_clip(1, 0, 2)]; // asset 1 (low)
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![TimelineSegment::Clip(TimelineClip {
            id: 2,
            asset: high,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        let segs = composite_segments(&p).unwrap();
        assert_eq!(
            segs,
            vec![Segment::Clip {
                path: "/media/high.mp4".into(),
                start: Seconds::from_integer(0),
                length: Seconds::from_integer(2),
            }]
        );
    }

    #[test]
    fn composite_is_black_only_when_every_track_is_a_gap() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let _ = p.add_asset("/media/low.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(3),
        })];
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(2),
        })];
        // Timeline end is the longer track (3 s); both gaps read black.
        let segs = composite_segments(&p).unwrap();
        assert_eq!(
            segs,
            vec![Segment::Gap {
                length: Seconds::from_integer(3)
            }]
        );
    }

    #[test]
    fn quote_always_uses_length_prefix() {
        assert_eq!(edl_quote("plain.mp4"), "%9%plain.mp4");
        assert_eq!(edl_quote("a,b;c"), "%5%a,b;c");
    }

    #[test]
    fn assemble_uses_length_not_end() {
        let segs = vec![
            Segment::Clip {
                path: "/a.mp4".into(),
                start: Seconds::from_integer(1),
                length: Seconds::from_integer(2),
            },
            Segment::Gap {
                length: Seconds::new(3, 2),
            },
        ];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        // third field is LENGTH: red src [1..3), then 1.5s of black
        assert_eq!(edl, "%6%/a.mp4,1,2;%16%/cache/black.mp4,0,1.5");
    }

    #[test]
    fn gap_without_black_file_is_an_error() {
        let segs = vec![Segment::Gap {
            length: Seconds::from_integer(1),
        }];
        assert!(matches!(
            assemble_edl(&segs, None),
            Err(PreviewError::NoSize)
        ));
    }

    #[test]
    fn long_gaps_split_into_black_file_chunks() {
        let segs = vec![Segment::Gap {
            length: Seconds::from_integer(25),
        }];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        assert_eq!(
            edl,
            "%16%/cache/black.mp4,0,10;%16%/cache/black.mp4,0,10;%16%/cache/black.mp4,0,5"
        );
        // exactly one black file long stays a single entry
        let segs = vec![Segment::Gap {
            length: Seconds::from_integer(BLACK_FILE_SECS),
        }];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        assert_eq!(edl, "%16%/cache/black.mp4,0,10");
    }

    #[test]
    fn extend_clip_preview_maps_origin_to_zero() {
        let pv = build_clip_preview(
            Path::new("/media/red.mp4"),
            Seconds::from_integer(1),
            Seconds::from_integer(4),
            2.0,
        );
        // asset trimmed to the material span; mpv 0 == timeline 2.0
        assert_eq!(pv.url, "edl://%14%/media/red.mp4,1,4");
        assert_eq!(pv.origin, 2.0);
    }

    #[test]
    fn extend_black_preview_needs_geometry() {
        // geometry errors surface without touching the cache
        let mut bare = Project::new();
        assert!(matches!(
            build_black_preview(&bare, 5.0, Seconds::from_integer(25)),
            Err(PreviewError::NoSize)
        ));
        bare.video = Some(VideoSettings {
            width: 320,
            height: 240,
        });
        assert!(matches!(
            build_black_preview(&bare, 5.0, Seconds::from_integer(25)),
            Err(PreviewError::NoFps)
        ));
    }

    #[test]
    fn edl_starts_with_prefix() {
        let p = project_with_gap();
        let segs = segments(&p, 0).unwrap();
        let edl = assemble_edl(&segs, Some(Path::new("/b.mp4"))).unwrap();
        assert!(edl.starts_with("%14%/media/red.mp4,1,2;"), "{edl}");
    }
}
