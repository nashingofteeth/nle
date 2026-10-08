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

use nle_core::timeline::{Segment, TrackKind};
use nle_core::{Project, Seconds, format_seconds};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::trim::{Encoder, FfmpegError, device_args, run_ffmpeg, video_encode_args};

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
    #[error("asset #{0} is referenced by the timeline but missing from the media pool")]
    MissingAsset(u32),
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
        src_start: String,
        dur: String,
        has_audio: bool,
    },
    Gap {
        dur: String,
    },
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
    let video_count = project.timeline.video_tracks().count();
    let audio_count = project.timeline.audio_tracks().count();
    if video_count <= 1 && audio_count == 0 {
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
                    dur: format_seconds(&c.duration()),
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
                dur,
                has_audio,
            } => {
                graph.push_str(&format!(
                    "[{input}:v]trim=start={src_start}:duration={dur},setpts=PTS-STARTPTS,\
                     scale={w}:{h},setsar=1,fps={fps_s},format=yuv420p[v{i}];\n"
                ));
                if *has_audio {
                    graph.push_str(&format!(
                        "[{input}:a]atrim=start={src_start}:duration={dur},asetpts=PTS-STARTPTS,\
                         aformat=sample_rates={rate}:channel_layouts={layout}[a{i}];\n",
                        rate = AUDIO_RATE,
                        layout = AUDIO_LAYOUT
                    ));
                } else {
                    graph.push_str(&format!(
                        "anullsrc=r={rate}:cl={layout},atrim=duration={dur},\
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
                Segment::Clip(c) => {
                    let asset = project
                        .asset(c.asset)
                        .ok_or(CompileError::MissingAsset(c.asset))?;
                    let input = *input_of.entry(c.asset).or_insert_with(|| {
                        inputs.push(asset.path.clone());
                        inputs.len() - 1
                    });
                    let src_start = format_seconds(&c.asset_in);
                    let dur = format_seconds(&c.duration());
                    if is_video {
                        graph.push_str(&format!(
                            "[{input}:v]trim=start={src_start}:duration={dur},setpts=PTS-STARTPTS,\
                             scale={w}:{h},setsar=1,fps={fps_s},format=yuva420p[v{ti}_{idx}];\n"
                        ));
                        v_labels.push(format!("[v{ti}_{idx}]"));
                    }
                    if asset.probe.has_audio() {
                        graph.push_str(&format!(
                            "[{input}:a]atrim=start={src_start}:duration={dur},asetpts=PTS-STARTPTS,\
                             aformat=sample_rates={rate}:channel_layouts={layout}[a{ti}_{idx}];\n",
                            rate = AUDIO_RATE,
                            layout = AUDIO_LAYOUT
                        ));
                        a_labels.push(format!("[a{ti}_{idx}]"));
                    } else {
                        push_audio_silence(&mut graph, &mut a_labels, &dur, ti, idx);
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
    if video_labels.len() == 1 {
        graph.push_str(&format!("{vcomp}null[vcomp];\n"));
    } else {
        for (k, top) in video_labels[1..].iter().enumerate() {
            let out = format!("ov{k}");
            graph.push_str(&format!("{vcomp}{top}overlay=format=auto[{out}];\n"));
            vcomp = format!("[{out}]");
        }
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

    eprintln!("$ ffmpeg {}", args.join(" "));
    run_ffmpeg(&args)?;
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
    use nle_core::timeline::{Clip, Gap};

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
            }),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(2),
            }),
            Segment::Clip(Clip {
                id: 2,
                asset: a1,
                asset_in: Seconds::from_integer(2),
                asset_out: Seconds::from_integer(5),
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
        })];
        let plan = compile_export(&p).unwrap();
        let g = &plan.graph;
        assert!(g.contains("amix=inputs=2:normalize=0[aout]"), "{g}");
        assert_eq!(plan.duration, Seconds::from_integer(15));
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
        })];
        p.timeline.tracks[ti].muted = true;
        let g = compile_export(&p).unwrap().graph;
        // The only remaining audio source is the video track's own audio,
        // so there's nothing to mix.
        assert!(!g.contains("amix"), "{g}");
        assert!(g.contains("[aout]"), "{g}");
    }
}
