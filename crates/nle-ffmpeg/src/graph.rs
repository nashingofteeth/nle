//! Graph compiler: video track segments → one `filter_complex` graph.
//!
//! The model IS the segment list now — there's no gap synthesis or
//! position math: each segment becomes two filter chains (video+audio),
//! normalized to the project's output settings, then concatenated.
//!
//! ```text
//! [0:v]trim=start=2:duration=3,setpts=PTS-STARTPTS,scale=…,fps=…[v0]
//! [0:a]atrim=start=2:duration=3,asetpts=PTS-STARTPTS,aformat=…[a0]
//! color=…:d=2[v1]           ← gap → black
//! anullsrc…[a1]             ← gap → silence
//! [v0][v1]concat=n=2:v=1:a=0[vout]
//! [a0][a1]concat=n=2:v=0:a=1[aout]
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use nle_core::project::Asset;
use nle_core::timeline::{Clip, Effect, Segment, TrackKind, TransitionKind, TransitionSource};
use nle_core::{Project, Seconds, format_seconds};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::trim::{
    Encoder, FfmpegError, device_args, run_ffmpeg_opt, run_ffmpeg_progress, video_encode_args,
};

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_LAYOUT: &str = "stereo";

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("timeline is empty — add clips first")]
    Empty,
    #[error("no frame rate set — add a clip first")]
    NoFps,
    #[error("no output size set — add a clip first")]
    NoSize,
    #[error("no video track in timeline")]
    NoVideoTrack,
    #[error("every video track is muted — nothing to export")]
    AllMuted,
    #[error("asset #{0} is referenced by the timeline but missing from the media pool")]
    MissingAsset(u32),
    #[error("transitions aren't exported yet (M5)")]
    Transition,
    #[error("asset #{0} has no picture size — cannot resolve a percentage crop")]
    MissingGeometry(u32),
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error("could not write filter script: {0}")]
    Script(#[from] std::io::Error),
    #[error(transparent)]
    Ffmpeg(#[from] FfmpegError),
}

/// One flattened segment for the filter graph.
enum Piece {
    Clip {
        input: usize,
        /// Source in-point (seconds into the file).
        src_start: String,
        /// Source span to trim (seconds of the file).
        src_dur: String,
        /// Timeline span the clip occupies (source / speed).
        out_dur: String,
        /// Playback rate; `1` is a no-op retime.
        speed: Seconds,
        /// Pre-rendered per-clip effect filters (empty when none).
        effects: String,
        has_audio: bool,
    },
    Gap {
        dur: String,
    },
}

/// The per-clip effect filters, joined for the chain — empty when there
/// are none, otherwise `f1,f2,` (the trailing comma is the separator before
/// the normalize `scale`). The chain runs before the normalize `scale`, so
/// any effect that measures the frame — crop, scale, and the distance an
/// offset moves — resolves against the asset's own frame, not the output.
fn clip_effects(c: &Clip, asset: &Asset, fallback: (u32, u32)) -> Result<String, CompileError> {
    effect_chain(&c.effects, asset, fallback)
}

/// The effect filters for a chain, joined and comma-terminated (empty when
/// nothing renders). `Effect::chain` threads the picture box through the
/// effects, so a crop resolves against a prior scale (and vice versa).
fn effect_chain(
    effects: &[Effect],
    asset: &Asset,
    fallback: (u32, u32),
) -> Result<String, CompileError> {
    if effects.is_empty() {
        return Ok(String::new());
    }
    let needs_geometry = effects.iter().any(|e| {
        matches!(
            e,
            Effect::Crop { .. } | Effect::Scale { .. } | Effect::Offset { .. }
        )
    });
    let canvas = if needs_geometry {
        asset
            .video_size()
            .ok_or(CompileError::MissingGeometry(asset.id))?
    } else {
        fallback
    };
    let chain = Effect::chain(effects, canvas);
    if chain.is_empty() {
        Ok(chain)
    } else {
        Ok(format!("{chain},"))
    }
}

/// The video retime stage. At 1× this is exactly the old
/// `setpts=PTS-STARTPTS`, so the simple-case graph stays byte-identical.
fn video_setpts(speed: &Seconds) -> String {
    if *speed == Seconds::from_integer(1) {
        "setpts=PTS-STARTPTS".to_string()
    } else {
        format!("setpts=(PTS-STARTPTS)/{}", format_seconds(speed))
    }
}

/// The audio retime stage (empty at 1×). `atempo` only accepts a limited
/// factor range, so an extreme speed (e.g. 0.4×) is split into a chain.
fn audio_atempo(speed: &Seconds) -> String {
    if *speed == Seconds::from_integer(1) {
        return String::new();
    }
    let mut f = secs_f64(speed);
    let mut parts: Vec<f64> = Vec::new();
    while f < 0.5 {
        parts.push(0.5);
        f /= 0.5;
    }
    while f > 2.0 {
        parts.push(2.0);
        f /= 2.0;
    }
    parts.push(f);
    parts
        .iter()
        .map(|p| format!(",atempo={}", trim_f64(*p)))
        .collect()
}

/// Compact decimal for a filter factor (avoids trailing zeros).
fn trim_f64(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// A compiled, ready-to-run export.
pub struct ExportPlan {
    /// Distinct input files, in the order their `-i` args must appear.
    pub inputs: Vec<PathBuf>,
    /// The full filter_complex text (for logging/tests).
    pub graph: String,
    pub width: u32,
    pub height: u32,
    pub fps: Seconds,
    /// Total output frames.
    pub frames: i64,
    /// Exact total duration (sum of segment durations).
    pub duration: Seconds,
}

/// Translate the project's tracks into an ffmpeg graph. A single video
/// track with no separate audio uses the classic concat path (byte-for-
/// byte unchanged); anything else composites video tracks (`overlay`) and
/// mixes audio (`amix`).
pub fn compile_export(project: &Project) -> Result<ExportPlan, CompileError> {
    // The fast path (one video track, no audio) ignores the mute flag, so a
    // muted sole video track exported as if unmuted. There is no video to
    // render either way: say so.
    if project.timeline.video_tracks().all(|t| t.muted) {
        return Err(CompileError::AllMuted);
    }
    let video_count = project.timeline.video_tracks().count();
    let audio_count = project.timeline.audio_tracks().count();
    // The classic path is byte-identical only for the simple case — and it
    // can't express a transition at all, so those fall through.
    let has_transition = project.timeline.tracks.iter().any(|t| {
        t.segments
            .iter()
            .any(|s| matches!(s, Segment::Transition(_)))
    });
    if video_count <= 1 && audio_count == 0 && !has_transition {
        return compile_single_video(project);
    }
    compile_multi_track(project)
}

/// The classic single-video-track graph: per-segment chains, one concat.
fn compile_single_video(project: &Project) -> Result<ExportPlan, CompileError> {
    let track = project
        .timeline
        .video_track()
        .ok_or(CompileError::NoVideoTrack)?;
    if track.segments.is_empty() {
        return Err(CompileError::Empty);
    }
    let fps = project.fps.ok_or(CompileError::NoFps)?;
    let settings = project.video.as_ref().ok_or(CompileError::NoSize)?;
    let (w, h) = (settings.width, settings.height);
    let fps_s = format!("{}/{}", fps.numer(), fps.denom());

    let mut pieces: Vec<Piece> = Vec::new();
    let mut input_of: HashMap<u32, usize> = HashMap::new();
    let mut inputs: Vec<PathBuf> = Vec::new();

    for seg in &track.segments {
        match seg {
            Segment::Transition(_) => return Err(CompileError::Transition),
            Segment::Clip(c) => {
                let asset = project
                    .asset(c.asset)
                    .ok_or(CompileError::MissingAsset(c.asset))?;
                // One -i per distinct asset; the graph is sequential, so
                // inputs are never busy twice.
                let input = *input_of.entry(c.asset).or_insert_with(|| {
                    inputs.push(asset.path.clone());
                    inputs.len() - 1
                });
                pieces.push(Piece::Clip {
                    input,
                    src_start: format_seconds(&c.asset_in),
                    src_dur: format_seconds(&c.source_duration()),
                    out_dur: format_seconds(&c.duration()),
                    speed: c.speed,
                    effects: clip_effects(c, asset, (w, h))?,
                    has_audio: asset.probe.has_audio(),
                });
            }
            Segment::Gap(g) => pieces.push(Piece::Gap {
                dur: format_seconds(&g.duration),
            }),
        }
    }

    // Emit the graph.
    let mut graph = String::new();
    for (i, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Clip {
                input,
                src_start,
                src_dur,
                out_dur,
                speed,
                effects,
                has_audio,
            } => {
                graph.push_str(&format!(
                    "[{input}:v]trim=start={src_start}:duration={src_dur},{setpts},{effects}\
                     scale={w}:{h},setsar=1,fps={fps_s},format=yuv420p[v{i}];\n",
                    setpts = video_setpts(speed),
                ));
                if *has_audio {
                    graph.push_str(&format!(
                        "[{input}:a]atrim=start={src_start}:duration={src_dur},asetpts=PTS-STARTPTS{atempo},\
                         aformat=sample_rates={rate}:channel_layouts={layout}[a{i}];\n",
                        atempo = audio_atempo(speed),
                        rate = AUDIO_RATE,
                        layout = AUDIO_LAYOUT
                    ));
                } else {
                    graph.push_str(&format!(
                        "anullsrc=r={rate}:cl={layout},atrim=duration={out_dur},\
                         asetpts=PTS-STARTPTS[a{i}];\n",
                        rate = AUDIO_RATE,
                        layout = AUDIO_LAYOUT
                    ));
                }
            }
            Piece::Gap { dur } => {
                graph.push_str(&format!(
                    "color=c=black:s={w}x{h}:r={fps_s}:d={dur},format=yuv420p[v{i}];\n"
                ));
                graph.push_str(&format!(
                    "anullsrc=r={rate}:cl={layout},atrim=duration={dur},\
                     asetpts=PTS-STARTPTS[a{i}];\n",
                    rate = AUDIO_RATE,
                    layout = AUDIO_LAYOUT
                ));
            }
        }
    }
    let n = pieces.len();
    let v_in: String = (0..n).map(|i| format!("[v{i}]")).collect();
    let a_in: String = (0..n).map(|i| format!("[a{i}]")).collect();
    graph.push_str(&format!("{v_in}concat=n={n}:v=1:a=0[vout];\n"));
    graph.push_str(&format!("{a_in}concat=n={n}:v=0:a=1[aout]\n"));

    let duration = track.end();
    let frames = (secs_f64(&duration) * secs_f64(&fps)).round() as i64;
    Ok(ExportPlan {
        inputs,
        graph,
        width: w,
        height: h,
        fps,
        frames,
        duration,
    })
}

/// Multi-track graph: each video track is a layer composited bottom→top
/// with `overlay` — gaps in an upper layer are transparent (`black@0.0`,
/// `yuva420p`) so lower layers show through; the bottom layer's gaps are
/// opaque black. Every track's audio is summed with `amix`. Each layer is
/// padded to the timeline length so overlay/amix span the whole timeline
/// regardless of individual track lengths.
fn compile_multi_track(project: &Project) -> Result<ExportPlan, CompileError> {
    let fps = project.fps.ok_or(CompileError::NoFps)?;
    let settings = project.video.as_ref().ok_or(CompileError::NoSize)?;
    let (w, h) = (settings.width, settings.height);
    let fps_s = format!("{}/{}", fps.numer(), fps.denom());
    let geo = Geometry {
        w,
        h,
        fps_s: &fps_s,
    };
    let total = project.timeline.end();
    if total <= Seconds::from_integer(0) {
        return Err(CompileError::Empty);
    }
    if project.timeline.video_tracks().next().is_none() {
        return Err(CompileError::NoVideoTrack);
    }

    let mut graph = String::new();
    let mut input_of: HashMap<u32, usize> = HashMap::new();
    let mut inputs: Vec<PathBuf> = Vec::new();

    // Video tracks are always layers (an empty one is a black base or a
    // no-op transparent overlay); an empty audio track would add only
    // silence, so it is skipped. A muted track contributes nothing.
    let included: Vec<usize> = project
        .timeline
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.muted && (t.kind == TrackKind::Video || !t.segments.is_empty()))
        .map(|(i, _)| i)
        .collect();
    let base_video = included
        .iter()
        .copied()
        .find(|&i| project.timeline.tracks[i].kind == TrackKind::Video);

    let mut video_labels: Vec<String> = Vec::new();
    let mut audio_labels: Vec<String> = Vec::new();

    for &ti in &included {
        let track = &project.timeline.tracks[ti];
        let is_video = track.kind == TrackKind::Video;
        // Upper video layers are transparent where they have no clip.
        let transparent = is_video && Some(ti) != base_video;
        let mut v_labels: Vec<String> = Vec::new();
        let mut a_labels: Vec<String> = Vec::new();
        let mut idx = 0usize;

        for seg in &track.segments {
            match seg {
                // A transition: two source sides, each retimed to exactly
                // `dur`, crossfaded into one video output and one audio.
                Segment::Transition(tr) => {
                    let dur = tr.dur;
                    let dur_s = format_seconds(&dur);
                    let (mut vl, mut al) = (Vec::new(), Vec::new());
                    for (side, tag) in [(&tr.a, "a"), (&tr.b, "b")] {
                        let v = format!("t{ti}_{idx}_v{tag}");
                        let a = format!("t{ti}_{idx}_a{tag}");
                        match side {
                            TransitionSource::Black => {
                                graph.push_str(&format!(
                                    "color=c=black:s={w}x{h}:r={fps_s}:d={dur_s},\
                                     format=yuva420p[{v}];\n"
                                ));
                                graph.push_str(&format!(
                                    "anullsrc=r={rate}:cl={layout},atrim=duration={dur_s},\
                                     asetpts=PTS-STARTPTS[{a}];\n",
                                    rate = AUDIO_RATE,
                                    layout = AUDIO_LAYOUT
                                ));
                            }
                            TransitionSource::Clip {
                                asset: asset_id,
                                src_in,
                                src_out,
                                effects,
                            } => {
                                let asset = project
                                    .asset(*asset_id)
                                    .ok_or(CompileError::MissingAsset(*asset_id))?;
                                let input = *input_of.entry(*asset_id).or_insert_with(|| {
                                    inputs.push(asset.path.clone());
                                    inputs.len() - 1
                                });
                                let src_start = format_seconds(src_in);
                                let src_dur = format_seconds(&(*src_out - *src_in));
                                // The span renders into `dur`, so its rate is
                                // source-per-timeline = src_dur / dur.
                                let speed = (*src_out - *src_in) / dur;
                                // The side is the neighbour's material, so its
                                // effects apply here too (the monitor proxy and
                                // the export use this same chain).
                                let fx = effect_chain(effects, asset, (w, h))?;
                                graph.push_str(&format!(
                                    "[{input}:v]trim=start={src_start}:duration={src_dur},\
                                     {setpts},{fx}scale={w}:{h},setsar=1,fps={fps_s},\
                                     format=yuva420p[{v}];\n",
                                    setpts = video_setpts(&speed)
                                ));
                                if asset.probe.has_audio() {
                                    graph.push_str(&format!(
                                        "[{input}:a]atrim=start={src_start}:duration={src_dur},\
                                         asetpts=PTS-STARTPTS{atempo},aformat=sample_rates={rate}:\
                                         channel_layouts={layout}[{a}];\n",
                                        atempo = audio_atempo(&speed),
                                        rate = AUDIO_RATE,
                                        layout = AUDIO_LAYOUT
                                    ));
                                } else {
                                    graph.push_str(&format!(
                                        "anullsrc=r={rate}:cl={layout},atrim=duration={dur_s},\
                                         asetpts=PTS-STARTPTS[{a}];\n",
                                        rate = AUDIO_RATE,
                                        layout = AUDIO_LAYOUT
                                    ));
                                }
                            }
                        }
                        vl.push(v);
                        al.push(a);
                    }
                    let xfade = match tr.kind {
                        TransitionKind::Dissolve => "fade",
                        TransitionKind::FadeBlack => "fadeblack",
                    };
                    graph.push_str(&format!(
                        "[{}][{}]xfade=transition={xfade}:duration={dur_s}:offset=0,\
                         format=yuva420p[v{ti}_{idx}];\n",
                        vl[0], vl[1]
                    ));
                    graph.push_str(&format!(
                        "[{}][{}]acrossfade=d={dur_s}[a{ti}_{idx}];\n",
                        al[0], al[1]
                    ));
                    v_labels.push(format!("[v{ti}_{idx}]"));
                    a_labels.push(format!("[a{ti}_{idx}]"));
                }
                Segment::Clip(c) => {
                    let asset = project
                        .asset(c.asset)
                        .ok_or(CompileError::MissingAsset(c.asset))?;
                    let input = *input_of.entry(c.asset).or_insert_with(|| {
                        inputs.push(asset.path.clone());
                        inputs.len() - 1
                    });
                    let src_start = format_seconds(&c.asset_in);
                    let src_dur = format_seconds(&c.source_duration());
                    let out_dur = format_seconds(&c.duration());
                    if is_video {
                        graph.push_str(&format!(
                            "[{input}:v]trim=start={src_start}:duration={src_dur},{setpts},{effects}\
                             scale={w}:{h},setsar=1,fps={fps_s},format=yuva420p[v{ti}_{idx}];\n",
                            setpts = video_setpts(&c.speed),
                            effects = clip_effects(c, asset, (w, h))?,
                        ));
                        v_labels.push(format!("[v{ti}_{idx}]"));
                    }
                    if asset.probe.has_audio() {
                        graph.push_str(&format!(
                            "[{input}:a]atrim=start={src_start}:duration={src_dur},asetpts=PTS-STARTPTS{atempo},\
                             aformat=sample_rates={rate}:channel_layouts={layout}[a{ti}_{idx}];\n",
                            atempo = audio_atempo(&c.speed),
                            rate = AUDIO_RATE,
                            layout = AUDIO_LAYOUT
                        ));
                        a_labels.push(format!("[a{ti}_{idx}]"));
                    } else {
                        push_audio_silence(&mut graph, &mut a_labels, &out_dur, ti, idx);
                    }
                }
                Segment::Gap(g) => {
                    let dur = format_seconds(&g.duration);
                    if is_video {
                        push_video_gap(&mut graph, &mut v_labels, &geo, transparent, &dur, ti, idx);
                    }
                    push_audio_silence(&mut graph, &mut a_labels, &dur, ti, idx);
                }
            }
            idx += 1;
        }

        // Pad the layer out to the timeline length so every layer spans it.
        if track.end() < total {
            let dur = format_seconds(&(total - track.end()));
            if is_video {
                push_video_gap(&mut graph, &mut v_labels, &geo, transparent, &dur, ti, idx);
            }
            push_audio_silence(&mut graph, &mut a_labels, &dur, ti, idx);
            idx += 1;
        }

        let n = idx;
        let v_in: String = v_labels.concat();
        let a_in: String = a_labels.concat();
        if is_video {
            graph.push_str(&format!("{v_in}concat=n={n}:v=1:a=0[vt{ti}];\n"));
            video_labels.push(format!("[vt{ti}]"));
        }
        graph.push_str(&format!("{a_in}concat=n={n}:v=0:a=1[at{ti}];\n"));
        audio_labels.push(format!("[at{ti}]"));
    }

    // Video: overlay bottom→top (all layers are yuva420p), then drop alpha.
    if video_labels.is_empty() {
        return Err(CompileError::NoVideoTrack);
    }
    let mut vcomp = video_labels[0].clone();
    for (k, top) in video_labels[1..].iter().enumerate() {
        let out = format!("ov{k}");
        graph.push_str(&format!("{vcomp}{top}overlay=format=auto[{out}];\n"));
        vcomp = format!("[{out}]");
    }
    graph.push_str(&format!("{vcomp}format=yuv420p[vout];\n"));

    // Audio: sum every track.
    if audio_labels.len() == 1 {
        graph.push_str(&format!("{}anull[aout]\n", audio_labels[0]));
    } else {
        let ins: String = audio_labels.concat();
        let n = audio_labels.len();
        graph.push_str(&format!("{ins}amix=inputs={n}:normalize=0[aout]\n"));
    }

    let frames = (secs_f64(&total) * secs_f64(&fps)).round() as i64;
    Ok(ExportPlan {
        inputs,
        graph,
        width: w,
        height: h,
        fps,
        frames,
        duration: total,
    })
}

/// Output geometry shared by the multi-track emit helpers.
struct Geometry<'a> {
    w: u32,
    h: u32,
    fps_s: &'a str,
}

/// A gap on a video layer: opaque black (base) or fully transparent (upper).
fn push_video_gap(
    graph: &mut String,
    labels: &mut Vec<String>,
    geo: &Geometry,
    transparent: bool,
    dur: &str,
    ti: usize,
    idx: usize,
) {
    let color = if transparent { "black@0.0" } else { "black" };
    graph.push_str(&format!(
        "color=c={color}:s={w}x{h}:r={fps}:d={dur},format=yuva420p[v{ti}_{idx}];\n",
        w = geo.w,
        h = geo.h,
        fps = geo.fps_s
    ));
    labels.push(format!("[v{ti}_{idx}]"));
}

/// A silence segment for a track's audio. Pushes its output label too.
fn push_audio_silence(
    graph: &mut String,
    labels: &mut Vec<String>,
    dur: &str,
    ti: usize,
    idx: usize,
) {
    graph.push_str(&format!(
        "anullsrc=r={rate}:cl={layout},atrim=duration={dur},\
         asetpts=PTS-STARTPTS[a{ti}_{idx}];\n",
        rate = AUDIO_RATE,
        layout = AUDIO_LAYOUT
    ));
    labels.push(format!("[a{ti}_{idx}]"));
}

/// Hardware encoders can't sit directly after a software filter graph —
/// route [vout] through hw surfaces first. Returns (graph, video label).
/// The leading `;` closes the previous (audio-concat) chain.
fn apply_encoder_tail(graph: String, encoder: Encoder) -> (String, &'static str) {
    let mut graph = graph;
    let vmap = match encoder {
        Encoder::Cpu => "vout",
        Encoder::Vaapi => {
            graph.push_str(";\n[vout]format=nv12,hwupload[venc]\n");
            "venc"
        }
        Encoder::Qsv => {
            graph.push_str(";\n[vout]format=nv12,hwupload=extra_hw_frames=64[venc]\n");
            "venc"
        }
    };
    (graph, vmap)
}

/// Export settings beyond what the graph compiler decides.
#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub output: PathBuf,
    pub encoder: Encoder,
    pub crf: u8,
    pub preset: String,
    pub bitrate: String,
}

/// Compile the graph and run the export. Returns the plan on success.
pub fn export(project: &Project, spec: &ExportSpec) -> Result<ExportPlan, ExportError> {
    run_export(project, spec, true, None)
}

/// [`export`] for a caller whose stderr is a terminal UI: ffmpeg's chatter
/// (and our own `$ ffmpeg …` line) is swallowed rather than painted over the
/// screen. A failure still carries ffmpeg's reason.
pub fn export_quiet(project: &Project, spec: &ExportSpec) -> Result<ExportPlan, ExportError> {
    run_export(project, spec, false, None)
}

/// [`export_quiet`] reporting the output position (seconds) as it renders —
/// the monitor turns this into the proxy progress in its status row.
pub fn export_quiet_progress(
    project: &Project,
    spec: &ExportSpec,
    progress: &mut dyn FnMut(f64),
) -> Result<ExportPlan, ExportError> {
    run_export(project, spec, false, Some(progress))
}

fn run_export(
    project: &Project,
    spec: &ExportSpec,
    echo: bool,
    mut progress: Option<&mut dyn FnMut(f64)>,
) -> Result<ExportPlan, ExportError> {
    let plan = compile_export(project)?;
    let (graph, vmap) = apply_encoder_tail(plan.graph.clone(), spec.encoder);

    // Long graphs exceed argv limits — pass via `-/filter_complex`, the
    // ffmpeg 9 file-read syntax (ffmpeg ≤8 used -filter_complex_script,
    // which was removed).
    let script = NamedTempFile::new()?;
    std::fs::write(script.path(), &graph)?;

    let mut args: Vec<String> = vec!["-y".into()];
    // export decodes in software (filters want sw frames)
    args.extend(device_args(spec.encoder, false));
    for path in &plan.inputs {
        args.push("-i".into());
        args.push(path.display().to_string());
    }
    args.push("-/filter_complex".into());
    args.push(script.path().display().to_string());
    args.push("-map".into());
    args.push(format!("[{vmap}]"));
    args.push("-map".into());
    args.push("[aout]".into());
    args.extend(video_encode_args(
        spec.encoder,
        spec.crf,
        &spec.preset,
        &spec.bitrate,
    ));
    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-b:a".into());
    args.push("192k".into());
    args.push("-movflags".into());
    args.push("+faststart".into());
    // `--` ends option parsing (protects outputs named like options).
    args.push("--".into());
    args.push(spec.output.display().to_string());

    if echo {
        eprintln!("$ ffmpeg {}", args.join(" "));
    }
    if let Some(cb) = progress.as_mut() {
        run_ffmpeg_progress(&args, echo, &mut **cb)?;
    } else {
        run_ffmpeg_opt(&args, echo)?;
    }
    Ok(plan) // script deleted on drop, after ffmpeg is done
}

fn secs_f64(t: &Seconds) -> f64 {
    *t.numer() as f64 / *t.denom() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::VideoSettings;
    use nle_core::media::{RawFormat, RawProbe, RawStream};
    use nle_core::timeline::{Clip, Effect, Gap, Transition, TransitionKind, TransitionSource};

    fn probe_report(duration: &str, w: u32, h: u32, rate: &str, audio: bool) -> RawProbe {
        let mut streams = vec![RawStream {
            index: 0,
            codec_type: Some("video".into()),
            codec_name: Some("h264".into()),
            width: Some(w),
            height: Some(h),
            r_frame_rate: Some(rate.into()),
            pix_fmt: Some("yuv420p".into()),
            ..Default::default()
        }];
        if audio {
            streams.push(RawStream {
                index: 1,
                codec_type: Some("audio".into()),
                codec_name: Some("aac".into()),
                sample_rate: Some("48000".into()),
                channels: Some(2),
                channel_layout: Some("stereo".into()),
                ..Default::default()
            });
        }
        RawProbe {
            streams,
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    fn project_with_two_clips() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings {
            width: 1920,
            height: 1080,
        });
        let (a0, _) = p.add_asset("a.mp4".into(), probe_report("10", 1920, 1080, "30/1", true));
        let (a1, _) = p.add_asset(
            "b.mp4".into(),
            probe_report("8", 1280, 720, "30000/1001", false),
        );
        let track = p.timeline.video_track_mut().unwrap();
        track.segments = vec![
            Segment::Clip(Clip {
                id: 1,
                asset: a0,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(10),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(2),
            }),
            Segment::Clip(Clip {
                id: 2,
                asset: a1,
                asset_in: Seconds::from_integer(2),
                asset_out: Seconds::from_integer(5),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
        ];
        p
    }

    #[test]
    fn compiles_clips_gap_and_concat() {
        let p = project_with_two_clips();
        let plan = compile_export(&p).unwrap();

        // inputs: deduped, first-use order
        assert_eq!(
            plan.inputs,
            vec![PathBuf::from("a.mp4"), PathBuf::from("b.mp4")]
        );
        assert_eq!(plan.width, 1920);
        assert_eq!(plan.height, 1080);

        let g = &plan.graph;
        // clip 1: full 10s from input 0
        assert!(g.contains("[0:v]trim=start=0:duration=10,"), "{g}");
        assert!(g.contains("[0:a]atrim=start=0:duration=10,"), "{g}");
        // gap → black + silence
        assert!(g.contains("color=c=black:s=1920x1080:r=30/1:d=2"), "{g}");
        // clip 2: src 2–5 from input 1; b.mp4 has no audio → anullsrc
        assert!(g.contains("[1:v]trim=start=2:duration=3,"), "{g}");
        assert!(
            g.contains("anullsrc=r=48000:cl=stereo,atrim=duration=3"),
            "{g}"
        );
        // concat of 3 segments, video then audio
        assert!(g.contains("[v0][v1][v2]concat=n=3:v=1:a=0[vout]"), "{g}");
        assert!(g.contains("[a0][a1][a2]concat=n=3:v=0:a=1[aout]"), "{g}");

        // frame math: 300 + 60 + 90 = 450 frames @ 30fps
        assert_eq!(plan.frames, 450);
        assert_eq!(plan.duration, Seconds::from_integer(15));
    }

    #[test]
    fn speed_clip_retimes_the_graph() {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(24));
        p.video = Some(VideoSettings {
            width: 1920,
            height: 1080,
        });
        let (a, _) = p.add_asset(
            "a.mp4".into(),
            probe_report("10", 1920, 1080, "30000/1001", true),
        );
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::new(1001, 1250), // 24 / (30000/1001) = 0.8008x
            effects: Vec::new(),
        })];
        let plan = compile_export(&p).unwrap();
        let g = &plan.graph;
        assert!(g.contains("setpts=(PTS-STARTPTS)/0.8008"), "{g}");
        assert!(g.contains("atempo=0.8008"), "{g}");
        // the trim is the SOURCE span (5s), not the 6.24s timeline span
        assert!(g.contains("duration=5,"), "{g}");
        assert_eq!(
            plan.duration,
            Seconds::from_integer(5) / Seconds::new(1001, 1250)
        );
    }

    #[test]
    fn extreme_slowdown_chains_atempo() {
        let mut p = Project::new();
        p.fps = Some(Seconds::from_integer(24));
        p.video = Some(VideoSettings {
            width: 1920,
            height: 1080,
        });
        let (a, _) = p.add_asset(
            "a.mp4".into(),
            probe_report("10", 1920, 1080, "60000/1001", true),
        );
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::new(1001, 2500), // 24 / (60000/1001) = 0.4004x
            effects: Vec::new(),
        })];
        let plan = compile_export(&p).unwrap();
        // atempo can't do 0.4x in one step; it splits into 0.5 x 0.8008
        assert!(
            plan.graph.contains("atempo=0.5,atempo=0.8008"),
            "{}",
            plan.graph
        );
    }

    #[test]
    fn one_x_graph_has_no_retime() {
        let p = project_with_two_clips();
        let plan = compile_export(&p).unwrap();
        assert!(
            !plan.graph.contains("setpts=(PTS-STARTPTS)/"),
            "{}",
            plan.graph
        );
        assert!(!plan.graph.contains("atempo"), "{}", plan.graph);
    }

    #[test]
    fn trailing_gap_segment_fills_the_export() {
        let mut p = project_with_two_clips();
        p.timeline
            .video_track_mut()
            .unwrap()
            .segments
            .push(Segment::Gap(Gap {
                duration: Seconds::from_integer(5),
            }));
        let plan = compile_export(&p).unwrap();
        assert!(
            plan.graph.contains("color=c=black:s=1920x1080:r=30/1:d=5"),
            "{}",
            plan.graph
        );
        assert_eq!(plan.frames, 600);
        assert_eq!(plan.duration, Seconds::from_integer(20));
    }

    #[test]
    fn rejects_empty() {
        let empty = Project::new();
        assert!(matches!(compile_export(&empty), Err(CompileError::Empty)));
    }

    #[test]
    fn hw_encoders_route_vout_through_hwupload() {
        let (g, vmap) = apply_encoder_tail("[vout]\n".into(), Encoder::Vaapi);
        assert_eq!(vmap, "venc");
        assert!(g.contains("[vout]format=nv12,hwupload[venc]"), "{g}");

        let (g, _) = apply_encoder_tail("[vout]\n".into(), Encoder::Qsv);
        assert!(
            g.contains("[vout]format=nv12,hwupload=extra_hw_frames=64[venc]"),
            "{g}"
        );

        let (g, vmap) = apply_encoder_tail("[vout]\n".into(), Encoder::Cpu);
        assert_eq!(vmap, "vout");
        assert_eq!(g, "[vout]\n");
    }

    #[test]
    fn single_track_graph_has_no_overlay_or_amix() {
        let p = project_with_two_clips();
        let g = compile_export(&p).unwrap().graph;
        assert!(!g.contains("overlay"), "{g}");
        assert!(!g.contains("amix"), "{g}");
    }

    /// Two video tracks: the base is `project_with_two_clips`'s track
    /// (15 s); the upper track ends at 2 s and must be padded to 15 s.
    fn project_with_two_video_tracks() -> Project {
        let mut p = project_with_two_clips();
        let a = p.media[0].id; // a.mp4 (has audio)
        let ti = p.timeline.add_track(TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![
            Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }),
            Segment::Clip(Clip {
                id: 100,
                asset: a,
                asset_in: Seconds::from_integer(1),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
        ];
        p
    }

    #[test]
    fn two_video_tracks_composite_with_overlay() {
        let p = project_with_two_video_tracks();
        let plan = compile_export(&p).unwrap();
        let g = &plan.graph;
        assert!(g.contains("overlay=format=auto"), "{g}");
        assert!(g.contains("[vout]"), "{g}");
        // The upper layer's gap is transparent; the base layer's is opaque.
        assert!(g.contains("color=c=black@0.0:s=1920x1080"), "{g}");
        assert!(g.contains("color=c=black:s=1920x1080"), "{g}");
        // The upper layer is padded from 2 s to the 15 s timeline.
        assert!(g.contains("d=13"), "{g}");
        assert_eq!(plan.duration, Seconds::from_integer(15));
        assert_eq!(plan.frames, 450);
    }

    #[test]
    fn separate_audio_track_mixes_with_amix() {
        let mut p = project_with_two_clips();
        let a = p.media[0].id;
        let ti = p.timeline.add_track(TrackKind::Audio);
        p.timeline.tracks[ti].segments = vec![Segment::Clip(Clip {
            id: 200,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        let plan = compile_export(&p).unwrap();
        let g = &plan.graph;
        assert!(g.contains("amix=inputs=2:normalize=0[aout]"), "{g}");
        // One video layer must flow straight to [vout] — no dangling
        // `null[vcomp]` (which ffmpeg rejected with an unconnected pad).
        assert!(g.contains("[vt0]format=yuv420p[vout]"), "{g}");
        assert!(!g.contains("null[vcomp]"), "{g}");
        assert_eq!(plan.duration, Seconds::from_integer(15));
    }

    #[test]
    fn a_muted_sole_video_track_is_not_silently_unmuted() {
        // The fast path (one video track, no audio) ignored `muted`, so a
        // muted sole video track exported its video anyway.
        let mut p = project_with_two_clips();
        p.timeline.tracks[0].muted = true;
        assert!(matches!(compile_export(&p), Err(CompileError::AllMuted)));
    }

    #[test]
    fn muted_tracks_are_skipped() {
        let mut p = project_with_two_clips();
        let a = p.media[0].id;
        let ti = p.timeline.add_track(TrackKind::Audio);
        p.timeline.tracks[ti].segments = vec![Segment::Clip(Clip {
            id: 200,
            asset: a,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(5),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        p.timeline.tracks[ti].muted = true;
        let g = compile_export(&p).unwrap().graph;
        // The only remaining audio source is the video track's own audio,
        // so there's nothing to mix.
        assert!(!g.contains("amix"), "{g}");
        assert!(g.contains("[aout]"), "{g}");
    }

    #[test]
    fn transition_composes_with_xfade_and_acrossfade() {
        let mut p = project_with_two_clips();
        let a = p.media[0].id;
        // [clip1 src 0..2] [1 s dissolve] [clip2 src 1..3] — the two sides
        // hold the trimmed-away source spans (each clip gave up 1 s).
        let clip = |id, in_, out| {
            Segment::Clip(Clip {
                id,
                asset: a,
                asset_in: Seconds::from_integer(in_),
                asset_out: Seconds::from_integer(out),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            })
        };
        p.timeline.video_track_mut().unwrap().segments = vec![
            clip(1, 0, 2),
            Segment::Transition(Transition {
                id: 3,
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
            clip(2, 1, 3),
        ];
        let plan = compile_export(&p).unwrap();
        let g = &plan.graph;
        // Two sides, each retimed to 1 s, crossfaded into one output.
        assert!(g.contains("xfade=transition=fade:"), "{g}");
        assert!(g.contains("acrossfade=d="), "{g}");
        assert_eq!(plan.duration, Seconds::from_integer(5), "clip|trans|clip");
    }

    #[test]
    fn effects_run_before_the_geometry_normalize() {
        let mut p = project_with_two_clips();
        if let Segment::Clip(c) = &mut p.timeline.video_track_mut().unwrap().segments[0] {
            c.effects = vec![
                // 25% off each edge of 1920×1080: mask, not zoom.
                Effect::Crop {
                    left: 25.0,
                    right: 25.0,
                    top: 25.0,
                    bottom: 25.0,
                },
                // Half width, full height.
                Effect::Scale { w: 50.0, h: 100.0 },
            ];
        }
        let g = compile_export(&p).unwrap().graph;
        // Effects chain after the retime, before the normalize scale. The
        // crop masks the outer quarter; the 50%/100% scale then acts on that
        // box (the 960×540 picture), not on the transparent canvas.
        assert!(
            g.contains(
                "format=yuva420p,crop=960:540:480:270,\
                 pad=1920:1080:480:270:color=black@0,\
                 format=yuva420p,crop=960:540:480:270,\
                 scale=480:540,pad=1920:1080:720:270:color=black@0,\
                 scale=1920:1080"
            ),
            "{g}"
        );
    }

    // An offset measures the distances to the edges of the frame the chain
    // runs on — the asset's own picture, since effects go before the
    // normalize. Resolving it against the output size instead produced a
    // crop of the output's dimensions against a smaller stream (export
    // refused at best, misframed at worst when the asset happens to be
    // larger than the sequence).
    #[test]
    fn an_offset_resolves_against_the_assets_frame() {
        let mut p = project_with_two_clips(); // output 1920×1080; clip 2 is 1280×720
        if let Segment::Clip(c) = &mut p.timeline.video_track_mut().unwrap().segments[2] {
            c.effects = vec![Effect::Offset { x: 50.0, y: 0.0 }];
        }
        let g = compile_export(&p).unwrap().graph;
        // Half of the 1280-wide frame: the picture's on-canvas part moves
        // right, leaving transparent canvas on the left.
        assert!(
            g.contains("crop=640:720:0:0,pad=1280:720:640:0:color=black@0"),
            "{g}"
        );
        // The output's 960×1080 half-geometry must not leak into the chain.
        assert!(!g.contains("crop=960:1080"), "{g}");
    }

    // A transition renders its neighbours' material, so their effects apply to
    // its sides too — the monitor proxy and the export use the same chain.
    #[test]
    fn a_transition_side_applies_the_neighbours_effects() {
        let mut p = project_with_two_clips(); // C1 0..10 · gap · C2
        let eq = Effect::Eq {
            brightness: 0.2,
            contrast: 1.0,
            saturation: 1.0,
        };
        if let Segment::Clip(c) = &mut p.timeline.video_track_mut().unwrap().segments[0] {
            c.effects = vec![eq];
        }
        let mut h = nle_core::edit::History::default();
        let batch = nle_core::edit::add_transition(
            &p,
            0,
            10.0,
            TransitionKind::Dissolve,
            Seconds::from_integer(1),
        )
        .unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let g = compile_export(&p).unwrap().graph;
        assert_eq!(
            g.matches("eq=brightness=0.2:contrast=1:saturation=1")
                .count(),
            2,
            "once on the clip, once on the transition side:\n{g}"
        );
    }
}
