//! Segment-parallel export for the local machine.
//!
//! The timeline is split at cut points and transition midpoints. A chunk that
//! touches a transition renders the whole transition (the graph needs both
//! sides), then trims its output to the chunk's half-open time range. Chunks
//! are encoded independently and cached. The video streams concatenate with a
//! stream copy; chunk audio is PCM and is encoded AAC once in the final mux,
//! which keeps sample boundaries exact.
//!
//! This is deliberately not a portable scheduler: it targets the local CPU
//! and VAAPI setup and reuses the same graph compiler as the single export.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use nle_core::state;
use nle_core::time::secs_f64;
use nle_core::timeline::{Segment, Track, Transition};
use nle_core::{Project, Seconds};

use nle_ffmpeg::trim::run_ffmpeg_opt;
use nle_ffmpeg::{Encoder, ExportSpec, export_range_quiet};

use crate::cmd::export::ExportOptions;
use crate::cmd::media::parse_f64;
use crate::log;
use crate::proxy::{self, PruneReport};

const EXPORT_CACHE_VERSION: u64 = 3;

struct Chunk {
    a: Seconds,
    b: Seconds,
    pre: Seconds,
    post: Seconds,
}

/// Try a parallel export. `Ok(None)` means the timeline did not produce a
/// useful split; the caller should do the ordinary single export.
pub(crate) fn run(
    project: &Project,
    project_path: &Path,
    output: &Path,
    options: &ExportOptions,
    workers: usize,
    quiet: bool,
    skipped: &mut Option<&'static str>,
) -> Result<Option<String>> {
    let total = project.timeline.end();
    if total <= Seconds::from_integer(0) || workers < 2 {
        *skipped = Some("nothing to split");
        return Ok(None);
    }
    // The serial graph rounds sub-frame segment spans to frames. Chunk trims
    // use exact wall-clock windows, so a sub-frame segment (or a source cut
    // between source frames) can make the two paths differ by a frame at a
    // boundary. Only chunk when both the timeline and the source cuts land on
    // their frame grids.
    if !frame_aligned(project) {
        log::line("chunked export needs frame-aligned segments; using one encoder");
        *skipped = Some("chunks need frame-aligned cuts");
        return Ok(None);
    }
    let cuts = choose_cuts(project, workers);
    if cuts.len() < 2 {
        *skipped = Some("no useful split point");
        return Ok(None);
    }
    let chunks = chunks(project, &cuts)?;
    if chunks.len() < 2 {
        *skipped = Some("no useful split point");
        return Ok(None);
    }
    // A cut inside a clip makes `slice_range` start from a fractional source
    // frame, so the chunk renders frames the single export wouldn't.
    if !boundaries_on_frames(project, &chunks) {
        log::line("chunked export needs every cut on a source frame; using one encoder");
        *skipped = Some("chunks need frame-aligned cuts");
        return Ok(None);
    }

    let temporary_cache;
    let cache = if options.use_cache {
        state::cache_dir(project_path).join("export")
    } else {
        temporary_cache = tempfile::tempdir().context("bench export cache")?;
        temporary_cache.path().join("export")
    };
    fs::create_dir_all(&cache).context("create export cache")?;
    let cache = cache.as_path();

    // Each chunk is independent; the cache is keyed by the project, encode
    // settings and exact render window. A completed chunk from an interrupted
    // run is reused as-is.
    //
    // Chunk audio is PCM: lossless chunks carry no AAC priming, so boundaries
    // stay sample-exact. The final mux encodes AAC once.
    let mut spec = options.spec(&PathBuf::new());
    spec.audio = nle_ffmpeg::AudioCodec::Pcm;
    // A partial hardware failure must not leave the rest of the chunks on the
    // GPU: `concat` stream-copies, so mixing two encoder heads (VAAPI + x264)
    // keeps only the first stream's headers. Retry the whole run on CPU, and
    // key those chunks by their own spec so they are never reused as GPU ones.
    let (paths, encoder, fallback) = match render_all(project, cache, &spec, &chunks) {
        Ok(paths) => (paths, spec.encoder, false),
        Err(e) if matches!(spec.encoder, Encoder::Vaapi | Encoder::Qsv) => {
            log::line(&format!("chunked export fell back to CPU: {e}"));
            let cpu = options.cpu_fallback(&spec);
            (
                render_all(project, cache, &cpu, &chunks)?,
                cpu.encoder,
                true,
            )
        }
        Err(e) => return Err(e),
    };

    concat(&paths, output, &project.timeline.end(), quiet, encoder)?;
    let shown = nle_ffmpeg::probe(output)?
        .format
        .duration
        .as_deref()
        .and_then(parse_f64)
        .map(|d| format!("{d:.2}"))
        .unwrap_or_else(|| "?".into());
    let fps = project.fps.unwrap_or(Seconds::from_integer(30));
    let frames = nle_ffmpeg::frame_count(output)
        .unwrap_or_else(|| (secs_f64(&project.timeline.end()) * secs_f64(&fps)).round() as i64);
    let fallback = if fallback { " · CPU fallback" } else { "" };
    Ok(Some(format!(
        "✓ {}  ({} s · {} frames @ {}/{}) · {}{fallback} · {workers}-way",
        output.display(),
        shown,
        frames,
        fps.numer(),
        fps.denom(),
        options.preset.label(),
    )))
}

/// Render every chunk concurrently; the first error aborts the run (the other
/// workers are joined before it is returned).
fn render_all(
    project: &Project,
    cache: &Path,
    spec: &ExportSpec,
    chunks: &[Chunk],
) -> Result<Vec<PathBuf>> {
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in chunks {
            handles.push(scope.spawn(move || render_chunk(project, cache, spec, chunk)));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("export worker panicked"))
            .collect()
    })
}

fn render_chunk(
    project: &Project,
    cache: &Path,
    spec: &ExportSpec,
    chunk: &Chunk,
) -> Result<PathBuf> {
    let hash = chunk_hash(project, spec, chunk);
    let path = cache.join(format!("{hash:016x}.mkv"));
    if usable_chunk(&path) {
        return Ok(path);
    }

    let timeline = project
        .timeline
        .slice_range(chunk.pre, chunk.post)
        .with_context(|| format!("slice {}..{}", fmt(&chunk.pre), fmt(&chunk.post)))?;
    let mut sub = project.clone();
    sub.timeline = timeline;
    sub.set_playhead(0.0);
    let offset = chunk.a - chunk.pre;
    let duration = chunk.b - chunk.a;
    // Render into a unique temp file and rename only on success: a killed
    // export must not leave a short file that a resume mistake treats as done.
    let temp = tempfile::Builder::new()
        .prefix(&format!("{hash:016x}.tmp-"))
        .suffix(".mkv")
        .tempfile_in(cache)
        .context("chunk temp file")?;
    let render = ExportSpec {
        output: temp.path().to_path_buf(),
        ..spec.clone()
    };

    export_range_quiet(&sub, &render, offset, duration)?;
    fs::rename(temp.path(), &path).with_context(|| format!("publish chunk {}", path.display()))?;
    Ok(path)
}

fn usable_chunk(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.len() > 0) && nle_ffmpeg::probe(path).is_ok()
}

fn concat(
    paths: &[PathBuf],
    output: &Path,
    total: &Seconds,
    quiet: bool,
    encoder: Encoder,
) -> Result<()> {
    let video = tempfile::Builder::new()
        .suffix(".mkv")
        .tempfile()
        .context("concat temp file")?;
    let mut list = tempfile::NamedTempFile::new()?;
    for path in paths {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let escaped = path.display().to_string().replace('\'', r"'\''");
        writeln!(list, "file '{escaped}'")?;
    }
    list.flush()?;
    let args = vec![
        "-y".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list.path().display().to_string(),
        "-c".into(),
        "copy".into(),
        "--".into(),
        video.path().display().to_string(),
    ];
    run_ffmpeg_opt(&args, !quiet).context("concat export chunks")?;

    let mut args = vec![
        "-y".into(),
        "-i".into(),
        video.path().display().to_string(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0".into(),
        "-c:v".into(),
        "copy".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-t".into(),
        format!("{:.6}", secs_f64(total)),
    ];
    if matches!(
        output.extension().and_then(|e| e.to_str()),
        Some("mp4" | "mov" | "m4v")
    ) {
        // The chunks are Matroska and their tags don't survive the stream
        // copy; re-tag HEVC as `hvc1` (Apple players reject `hev1`).
        if encoder == Encoder::X265 {
            args.push("-tag:v".into());
            args.push("hvc1".into());
        }
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    args.push("--".into());
    args.push(output.display().to_string());
    run_ffmpeg_opt(&args, !quiet).context("mux export chunks")?;
    Ok(())
}

fn frame_aligned(project: &Project) -> bool {
    let Some(fps) = project.fps else {
        return false;
    };
    if fps <= Seconds::from_integer(0) {
        return false;
    }
    project.timeline.tracks.iter().all(|track| {
        track.segments.iter().all(|seg| {
            if !(seg.duration() * fps).is_integer() {
                return false;
            }
            match seg {
                Segment::Clip(c) => {
                    if track.kind == nle_core::TrackKind::Video {
                        let Some(asset) = project.asset(c.asset) else {
                            return false;
                        };
                        let Some(source_fps) = asset.probe.fps() else {
                            return false;
                        };
                        if !(c.asset_in * source_fps).is_integer()
                            || !(c.asset_out * source_fps).is_integer()
                        {
                            return false;
                        }
                    }
                }
                Segment::Transition(tr) => {
                    for side in [&tr.a, &tr.b] {
                        if let nle_core::timeline::TransitionSource::Clip {
                            asset,
                            src_in,
                            src_out,
                            ..
                        } = side
                        {
                            let Some(asset) = project.asset(*asset) else {
                                return false;
                            };
                            let Some(source_fps) = asset.probe.fps() else {
                                return false;
                            };
                            if !(src_in * source_fps).is_integer()
                                || !(src_out * source_fps).is_integer()
                            {
                                return false;
                            }
                        }
                    }
                }
                Segment::Gap(_) => {}
            }
            true
        })
    })
}

/// Every chunk boundary must start every video track on a source frame. The
/// per-clip check in [`frame_aligned`] covers each clip's existing in/out, but
/// a cut derived from one track can fall mid-clip on another; slicing there
/// invents a source cut that is not on the source's frame grid.
fn boundaries_on_frames(project: &Project, chunks: &[Chunk]) -> bool {
    chunks.iter().all(|chunk| {
        [chunk.pre, chunk.post].iter().all(|at| {
            project
                .timeline
                .video_tracks()
                .all(|track| track_starts_on_a_frame(project, track, *at))
        })
    })
}

fn track_starts_on_a_frame(project: &Project, track: &Track, at: Seconds) -> bool {
    // A boundary exactly on a segment edge is the case `frame_aligned` already
    // covers (the clip's own `asset_in`); only a cut inside a clip needs the
    // source position at that offset.
    let Some(i) = track.index_at(at) else {
        return true;
    };
    let start = track.start_of(i);
    if start == at {
        return true;
    }
    match &track.segments[i] {
        Segment::Clip(c) => {
            let Some(asset) = project.asset(c.asset) else {
                return false;
            };
            let Some(fps) = asset.probe.fps() else {
                return false;
            };
            (c.source_at(at - start) * fps).is_integer()
        }
        Segment::Gap(_) => true,
        // `chunks` widens each chunk over every blend it touches, so a cut can
        // only land inside one if that widening did not converge.
        Segment::Transition(_) => false,
    }
}

/// All frame-aligned cut points, plus transition midpoints, in [0, total].
fn candidates(project: &Project) -> Vec<Seconds> {
    let total = project.timeline.end();
    let fps = project.fps.as_ref();
    let mut points = vec![Seconds::from_integer(0), total];
    for track in &project.timeline.tracks {
        let mut acc = Seconds::from_integer(0);
        for seg in &track.segments {
            let end = acc + seg.duration();
            points.push(acc);
            points.push(end);
            if let Segment::Transition(tr) = seg {
                points.push(acc + tr.dur / Seconds::from_integer(2));
            }
            acc = end;
        }
    }
    points.retain(|p| *p > Seconds::from_integer(0) && *p < total);
    points.push(Seconds::from_integer(0));
    points.push(total);
    for p in &mut points {
        *p = snap(fps, *p);
    }
    points.sort();
    points.dedup();
    points
}

fn choose_cuts(project: &Project, workers: usize) -> Vec<Seconds> {
    let total = project.timeline.end();
    let points = candidates(project);
    let mut chosen: Vec<Seconds> = Vec::new();
    for i in 1..workers {
        let target = total * Seconds::new(i as i64, workers as i64);
        let best = points
            .iter()
            .filter(|p| **p > Seconds::from_integer(0) && **p < total && !chosen.contains(p))
            .min_by(|a, b| {
                let da = (secs_f64(a) - secs_f64(&target)).abs();
                let db = (secs_f64(b) - secs_f64(&target)).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });
        if let Some(best) = best {
            chosen.push(*best);
        }
    }
    chosen.sort();
    chosen.dedup();
    let mut cuts = vec![Seconds::from_integer(0)];
    cuts.extend(chosen);
    cuts.push(total);
    cuts
}

fn chunks(project: &Project, cuts: &[Seconds]) -> Result<Vec<Chunk>> {
    let transitions = transitions(project);
    let mut out = Vec::new();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a >= b {
            continue;
        }
        // Include every transition that overlaps this output range, whole:
        // the graph needs both sides and the blend itself. Widening can reach
        // a blend on another track, so repeat until the range stops growing.
        let mut pre = a;
        let mut post = b;
        loop {
            let mut grew = false;
            for (start, end) in &transitions {
                if *start < post && *end > pre {
                    if *start < pre {
                        pre = *start;
                        grew = true;
                    }
                    if *end > post {
                        post = *end;
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
        }
        out.push(Chunk { a, b, pre, post });
    }
    Ok(out)
}

fn transitions(project: &Project) -> Vec<(Seconds, Seconds)> {
    let mut out = Vec::new();
    for track in &project.timeline.tracks {
        let mut acc = Seconds::from_integer(0);
        for seg in &track.segments {
            if let Segment::Transition(Transition { dur, .. }) = seg {
                out.push((acc, acc + *dur));
            }
            acc += seg.duration();
        }
    }
    out
}

fn snap(fps: Option<&Seconds>, t: Seconds) -> Seconds {
    let Some(fps) = fps else { return t };
    let frame = (secs_f64(&t) * secs_f64(fps)).round() as i64;
    Seconds::new(frame * fps.denom(), *fps.numer())
}

fn chunk_hash(project: &Project, spec: &ExportSpec, chunk: &Chunk) -> u64 {
    let mut h = DefaultHasher::new();
    EXPORT_CACHE_VERSION.hash(&mut h);
    // The playhead is session state — the TUI rewrites it on every seek and
    // no render reads it — so exclude it or every chunk is invalidated by a
    // move of the cursor.
    let mut snapshot = project.clone();
    snapshot.playhead = Seconds::from_integer(0);
    serde_json::to_vec(&snapshot)
        .unwrap_or_default()
        .hash(&mut h);
    // Project JSON has paths, not source content. Include size/mtime so a
    // replaced source invalidates its chunks too.
    for asset in &project.media {
        asset.path.hash(&mut h);
        if let Ok(meta) = fs::metadata(&asset.path) {
            meta.len().hash(&mut h);
            if let Ok(modified) = meta.modified()
                && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                since.as_secs().hash(&mut h);
                since.subsec_nanos().hash(&mut h);
            }
        }
    }
    format!("{:?}", spec.encoder).hash(&mut h);
    spec.crf.hash(&mut h);
    spec.preset.hash(&mut h);
    spec.bitrate.hash(&mut h);
    format!("{:?}", spec.audio).hash(&mut h);
    for value in [chunk.a, chunk.b, chunk.pre, chunk.post] {
        value.numer().hash(&mut h);
        value.denom().hash(&mut h);
    }
    h.finish()
}

fn fmt(t: &Seconds) -> String {
    format!("{:.3}", secs_f64(t))
}

/// Prune this project's completed chunk cache. The same narrow entry-name
/// rules as the proxy cache apply (only `<hash>.mkv`, `<hash>.mp4` and
/// crashed temps).
pub(crate) fn prune_export(
    project_path: &Path,
    max_age: Duration,
    dry_run: bool,
) -> Result<PruneReport> {
    let dir = state::cache_dir(project_path).join("export");
    proxy::prune_at(&dir, max_age, SystemTime::now(), dry_run).context("prune export chunks")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::export::QualityPreset;
    use nle_core::timeline::{Clip, Gap, TrackKind, TransitionKind, TransitionSource};

    fn project() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(nle_core::VideoSettings {
            width: 320,
            height: 180,
        });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe());
        p.timeline.tracks = vec![Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![
                Segment::Clip(Clip::new(
                    a,
                    a,
                    Seconds::from_integer(0),
                    Seconds::from_integer(2),
                )),
                Segment::Transition(Transition {
                    id: 1,
                    dur: Seconds::from_integer(1),
                    kind: TransitionKind::Dissolve,
                    a: TransitionSource::Clip {
                        asset: a,
                        src_in: Seconds::from_integer(2),
                        src_out: Seconds::from_integer(3),
                        effects: Vec::new(),
                    },
                    b: TransitionSource::Clip {
                        asset: a,
                        src_in: Seconds::from_integer(0),
                        src_out: Seconds::from_integer(1),
                        effects: Vec::new(),
                    },
                }),
                Segment::Clip(Clip::new(
                    a,
                    a,
                    Seconds::from_integer(1),
                    Seconds::from_integer(3),
                )),
            ],
        }];
        p
    }

    fn probe() -> nle_core::RawProbe {
        nle_core::RawProbe {
            streams: vec![nle_core::RawStream {
                index: 0,
                codec_type: Some("video".into()),
                codec_name: Some("h264".into()),
                width: Some(320),
                height: Some(180),
                r_frame_rate: Some("30/1".into()),
                ..Default::default()
            }],
            format: nle_core::RawFormat {
                format_name: "mp4".into(),
                duration: Some("10".into()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn sub_frame_segments_disable_the_scheduler() {
        let mut p = project();
        if let Segment::Clip(c) = &mut p.timeline.tracks[0].segments[0] {
            c.speed = Seconds::new(7, 3);
        }
        assert!(!frame_aligned(&p));
        let mut p = project();
        if let Segment::Clip(c) = &mut p.timeline.tracks[0].segments[0] {
            c.asset_in = Seconds::new(1, 7);
        }
        assert!(!frame_aligned(&p));
        let p = project();
        assert!(frame_aligned(&p));
    }

    #[test]
    fn boundaries_include_transition_midpoints() {
        let p = project();
        let total = p.timeline.end();
        let points = candidates(&p);
        assert!(points.contains(&(Seconds::from_integer(2) + Seconds::new(1, 2))));
        assert!(points.contains(&total));
    }

    /// The fixture is `[clip 0..2][transition 2..3][clip 3..5]`, so the only
    /// interior cut points are 2, 2.5 (the blend midpoint) and 3.
    #[test]
    fn cuts_land_on_the_nearest_cut_point_not_a_raw_fraction() {
        let p = project();
        // Thirds of 5 s are 1.667 and 3.333 — neither is a cut, so the
        // scheduler must fall back to the nearest one, never encode from the
        // middle of a clip's frame.
        assert_eq!(
            choose_cuts(&p, 3),
            vec![
                Seconds::from_integer(0),
                Seconds::from_integer(2),
                Seconds::from_integer(3),
                p.timeline.end(),
            ]
        );
    }

    #[test]
    fn one_or_zero_workers_is_the_single_export() {
        let p = project();
        let whole = vec![Seconds::from_integer(0), p.timeline.end()];
        assert_eq!(choose_cuts(&p, 1), whole);
        // `--parallel 0` must not divide by zero on the way to "no split".
        assert_eq!(choose_cuts(&p, 0), whole);
    }

    #[test]
    fn every_split_is_sorted_bounded_and_distinct() {
        let p = project();
        for workers in 2..=8 {
            let cuts = choose_cuts(&p, workers);
            assert_eq!(cuts.first(), Some(&Seconds::from_integer(0)), "{workers}");
            assert_eq!(cuts.last(), Some(&p.timeline.end()), "{workers}");
            for pair in cuts.windows(2) {
                assert!(pair[0] < pair[1], "unordered or empty range: {pair:?}");
            }
            assert!(
                cuts.len() <= workers + 1,
                "more cuts than requested: {cuts:?}"
            );
        }
    }

    #[test]
    fn degenerate_cut_lists_produce_no_chunks() {
        let p = project();
        assert!(chunks(&p, &[]).unwrap().is_empty());
        assert!(chunks(&p, &[Seconds::from_integer(1)]).unwrap().is_empty());
        // Repeated cuts would be zero-width chunks; they are skipped, not
        // passed to ffmpeg as `-ss N -to N`.
        let repeated = [Seconds::from_integer(0), Seconds::from_integer(0)];
        assert!(chunks(&p, &repeated).unwrap().is_empty());
    }

    #[test]
    fn snap_rounds_to_the_nearest_frame() {
        let fps = Seconds::from_integer(30);
        // 1.01 s is 30.3 frames -> frame 30; 1.02 s is 30.6 -> frame 31.
        assert_eq!(
            snap(Some(&fps), Seconds::new(101, 100)),
            Seconds::from_integer(1)
        );
        assert_eq!(
            snap(Some(&fps), Seconds::new(102, 100)),
            Seconds::new(31, 30)
        );
        // No fps (a probe that failed) leaves the time alone.
        assert_eq!(snap(None, Seconds::new(101, 100)), Seconds::new(101, 100));
    }

    #[test]
    fn transitions_are_reported_with_their_output_span() {
        assert_eq!(
            transitions(&project()),
            vec![(Seconds::from_integer(2), Seconds::from_integer(3))]
        );
    }

    #[test]
    fn a_zero_or_missing_fps_disables_the_scheduler() {
        let mut p = project();
        p.fps = None;
        assert!(!frame_aligned(&p));
        p.fps = Some(Seconds::from_integer(0));
        assert!(!frame_aligned(&p));
    }

    #[test]
    fn chunk_hash_is_stable_but_respects_the_chunk_and_the_preset() {
        let p = project();
        let out = Path::new("/tmp/out.mp4");
        let balanced = ExportOptions::default().spec(out);
        let max = ExportOptions {
            preset: QualityPreset::Max,
            ..Default::default()
        }
        .spec(out);
        let one = Chunk {
            a: Seconds::from_integer(0),
            b: Seconds::from_integer(2),
            pre: Seconds::from_integer(0),
            post: Seconds::from_integer(2),
        };
        let two = Chunk {
            a: Seconds::from_integer(2),
            b: Seconds::from_integer(5),
            pre: Seconds::from_integer(2),
            post: Seconds::from_integer(5),
        };
        assert_eq!(
            chunk_hash(&p, &balanced, &one),
            chunk_hash(&p, &balanced, &one)
        );
        assert_ne!(
            chunk_hash(&p, &balanced, &one),
            chunk_hash(&p, &balanced, &two)
        );
        assert_ne!(chunk_hash(&p, &balanced, &one), chunk_hash(&p, &max, &one));
    }

    #[test]
    fn moving_the_playhead_does_not_invalidate_chunks() {
        let p = project();
        let spec = ExportOptions::default().spec(Path::new("/tmp/out.mp4"));
        let one = Chunk {
            a: Seconds::from_integer(0),
            b: Seconds::from_integer(2),
            pre: Seconds::from_integer(0),
            post: Seconds::from_integer(2),
        };
        // The TUI persists the playhead on every seek; no render reads it, so
        // it must not be part of the cache key.
        let mut moved = p.clone();
        moved.set_playhead(1.5);
        assert_ne!(moved.playhead, p.playhead);
        assert_eq!(chunk_hash(&p, &spec, &one), chunk_hash(&moved, &spec, &one));
    }

    /// The fixture plus a second video track reading a 24 fps source: a cut at
    /// 1/30 s is a project frame but not a frame of that source.
    fn two_rate_project() -> Project {
        let mut p = project();
        let mut probe24 = probe();
        probe24.streams[0].r_frame_rate = Some("24/1".into());
        let (b, _) = p.add_asset("/media/blue.mp4".into(), probe24);
        p.timeline.tracks.push(Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![Segment::Clip(Clip::new(
                b,
                b,
                Seconds::from_integer(0),
                Seconds::from_integer(5),
            ))],
        });
        p
    }

    #[test]
    fn a_cut_inside_another_tracks_clip_must_land_on_its_source_frame() {
        let p = two_rate_project();
        let off_grid = Chunk {
            a: Seconds::from_integer(0),
            b: Seconds::new(1, 30),
            pre: Seconds::from_integer(0),
            post: Seconds::new(1, 30),
        };
        assert!(!boundaries_on_frames(&p, &[off_grid]));
        // 1/3 s is frame 10 at 30 fps and frame 8 at 24 fps.
        let on_grid = Chunk {
            a: Seconds::from_integer(0),
            b: Seconds::new(1, 3),
            pre: Seconds::from_integer(0),
            post: Seconds::new(1, 3),
        };
        assert!(boundaries_on_frames(&p, &[on_grid]));
    }

    fn blend(id: u32, a: u32, dur: Seconds) -> Segment {
        Segment::Transition(Transition {
            id,
            dur,
            kind: TransitionKind::Dissolve,
            a: TransitionSource::Clip {
                asset: a,
                src_in: Seconds::from_integer(0),
                src_out: Seconds::from_integer(1),
                effects: Vec::new(),
            },
            b: TransitionSource::Clip {
                asset: a,
                src_in: Seconds::from_integer(1),
                src_out: Seconds::from_integer(2),
                effects: Vec::new(),
            },
        })
    }

    fn gap(duration: Seconds) -> Segment {
        Segment::Gap(Gap { duration })
    }

    /// Two blends on different tracks: widening for the second (2.0..3.0)
    /// reaches the first (2.9..3.1). One pass would stop at 3.0 and slice the
    /// first blend in half.
    #[test]
    fn widening_follows_a_chain_of_transitions_on_other_tracks() {
        let mut p = project();
        let a = p.media[0].id;
        p.timeline.tracks = vec![
            Track {
                kind: TrackKind::Video,
                muted: false,
                segments: vec![gap(Seconds::new(29, 10)), blend(1, a, Seconds::new(2, 10))],
            },
            Track {
                kind: TrackKind::Video,
                muted: false,
                segments: vec![
                    gap(Seconds::from_integer(2)),
                    blend(2, a, Seconds::from_integer(1)),
                ],
            },
        ];
        let cuts = vec![
            Seconds::from_integer(2),
            Seconds::new(21, 10),
            Seconds::new(31, 10),
        ];
        let chunks = chunks(&p, &cuts).unwrap();
        assert_eq!(chunks[0].pre, Seconds::from_integer(2));
        assert_eq!(chunks[0].post, Seconds::new(31, 10));
    }

    #[test]
    fn chunks_include_the_whole_transition_around_a_midpoint_split() {
        let p = project();
        let cuts = vec![
            Seconds::from_integer(0),
            Seconds::from_integer(2) + Seconds::new(1, 2),
            p.timeline.end(),
        ];
        let chunks = chunks(&p, &cuts).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].pre, Seconds::from_integer(0));
        assert_eq!(chunks[0].post, Seconds::from_integer(3));
        assert_eq!(chunks[1].pre, Seconds::from_integer(2));
        assert_eq!(chunks[1].post, p.timeline.end());
        p.timeline
            .slice_range(chunks[0].pre, chunks[0].post)
            .unwrap();
        p.timeline
            .slice_range(chunks[1].pre, chunks[1].post)
            .unwrap();
    }
}
