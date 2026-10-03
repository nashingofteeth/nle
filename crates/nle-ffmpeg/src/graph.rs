//! Graph compiler v0: single video track → one `filter_complex` graph.
//!
//! The timeline is a sequence of segments (clips and gaps). Each becomes
//! two filter chains — video and audio — normalized to the project's
//! output settings, then concatenated:
//!
//! ```text
//! [0:v]trim=start=2,duration=3,setpts=PTS-STARTPTS,scale=…,fps=…[v0]
//! [0:a]atrim=start=2,duration=3,asetpts=PTS-STARTPTS,aformat=…[a0]
//! color=…:d=2[v1]           ← gap → black
//! anullsrc…[a1]             ← gap → silence
//! [v0][v1]concat=n=2:v=1:a=0[vout]
//! [a0][a1]concat=n=2:v=0:a=1[aout]
//! ```
//!
//! All positions snap to the frame grid of the project frame rate, so
//! every cut point is frame-exact even though the model stores seconds.

use std::collections::HashMap;
use std::path::PathBuf;

use nle_core::{format_seconds, Clip, Project, Seconds};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::trim::{device_args, run_ffmpeg, video_encode_args, Encoder, FfmpegError};

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
    #[error("clip #{0} is shorter than one frame at the project frame rate")]
    TooShort(u32),
    #[error("clips overlap on the timeline near {0}")]
    Overlap(String),
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

/// One segment of the flattened timeline.
enum Segment {
    Clip {
        /// Index into ExportPlan::inputs.
        input: usize,
        /// Source in-point (seconds, frame-aligned).
        src_start: String,
        /// Segment duration (seconds, frame-aligned).
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
    /// Exact total duration (frames / fps).
    pub duration: Seconds,
}

/// Translate the project's video track into an ffmpeg graph.
pub fn compile_export(project: &Project) -> Result<ExportPlan, CompileError> {
    let track = project
        .timeline
        .video_track()
        .ok_or(CompileError::NoVideoTrack)?;
    let mut clips: Vec<&Clip> = track.clips.iter().collect();
    clips.sort_by_key(|a| a.start);
    if clips.is_empty() {
        return Err(CompileError::Empty);
    }
    let fps = project.fps.ok_or(CompileError::NoFps)?;
    let settings = project.video.as_ref().ok_or(CompileError::NoSize)?;
    let (w, h) = (settings.width, settings.height);

    // Timeline position → frame index on the output grid.
    // round() returns a Ratio; to_integer() is the i64 extraction.
    let frame = |t: &Seconds| -> i64 { (*t * fps).round().to_integer() };

    let mut segments: Vec<Segment> = Vec::new();
    let mut input_of: HashMap<u32, usize> = HashMap::new();
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut cursor = 0i64; // current end of output, in frames

    for clip in clips {
        let asset = project
            .asset(clip.asset)
            .ok_or(CompileError::MissingAsset(clip.asset))?;

        let start_f = frame(&clip.start);
        if start_f < cursor {
            return Err(CompileError::Overlap(format_seconds(&clip.start)));
        }
        if start_f > cursor {
            // gap → black + silence (cursor jumps ahead below, past the gap)
            let gap = Seconds::new(start_f - cursor, 1) / fps;
            segments.push(Segment::Gap { dur: format_seconds(&gap) });
        }

        let in_f = frame(&clip.asset_in);
        let out_f = frame(&clip.asset_out);
        if out_f <= in_f {
            return Err(CompileError::TooShort(clip.id));
        }

        // One -i per distinct asset; clips sharing an asset share the input
        // (the graph is purely sequential, so inputs are never busy twice).
        let input = *input_of.entry(clip.asset).or_insert_with(|| {
            inputs.push(asset.path.clone());
            inputs.len() - 1
        });

        let src_start = Seconds::new(in_f, 1) / fps;
        let dur = Seconds::new(out_f - in_f, 1) / fps;
        segments.push(Segment::Clip {
            input,
            src_start: format_seconds(&src_start),
            dur: format_seconds(&dur),
            has_audio: asset.probe.has_audio(),
        });
        cursor = start_f + (out_f - in_f);
    }

    // Trailing sequence tail: fill with black + silence so the export
    // honors the timeline's displayed end.
    if let Some(len) = &project.timeline.length {
        let len_f = frame(len);
        if len_f > cursor {
            let gap = Seconds::new(len_f - cursor, 1) / fps;
            segments.push(Segment::Gap { dur: format_seconds(&gap) });
            cursor = len_f;
        }
    }

    // Emit the graph.
    let fps_s = format!("{}/{}", fps.numer(), fps.denom());
    let mut graph = String::new();
    for (i, seg) in segments.iter().enumerate() {
        match seg {
            Segment::Clip { input, src_start, dur, has_audio } => {
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
            Segment::Gap { dur } => {
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
    let n = segments.len();
    let v_in: String = (0..n).map(|i| format!("[v{i}]")).collect();
    let a_in: String = (0..n).map(|i| format!("[a{i}]")).collect();
    graph.push_str(&format!("{v_in}concat=n={n}:v=1:a=0[vout];\n"));
    graph.push_str(&format!("{a_in}concat=n={n}:v=0:a=1[aout]\n"));

    Ok(ExportPlan {
        inputs,
        graph,
        width: w,
        height: h,
        fps,
        frames: cursor,
        duration: Seconds::new(cursor, 1) / fps,
    })
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
    args.extend(video_encode_args(spec.encoder, spec.crf, &spec.preset, &spec.bitrate));
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

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::media::{RawFormat, RawStream, RawProbe};
    use nle_core::Project;
    use std::path::PathBuf;

    fn probe_report(duration: &str, w: u32, h: u32, rate: &str, audio: bool) -> nle_core::RawProbe {
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
        let (a0, _) = p.add_asset("a.mp4".into(), probe_report("10", 1920, 1080, "30/1", true));
        let (a1, _) = p.add_asset("b.mp4".into(), probe_report("8", 1280, 720, "30000/1001", false));
        p.add_clip(a0, None, Seconds::from_integer(0), Seconds::from_integer(10))
            .unwrap();
        // gap 10s→12s, then 3s from b.mp4's second 3 seconds
        p.add_clip(a1, Some(Seconds::from_integer(12)), Seconds::from_integer(2), Seconds::from_integer(5))
            .unwrap();
        p
    }

    #[test]
    fn compiles_clips_gap_and_concat() {
        let p = project_with_two_clips();
        let plan = compile_export(&p).unwrap();

        // inputs: deduped, first-use order
        assert_eq!(plan.inputs, vec![PathBuf::from("a.mp4"), PathBuf::from("b.mp4")]);
        assert_eq!(plan.width, 1920);
        assert_eq!(plan.height, 1080);

        let g = &plan.graph;
        // clip 1: full 10s from input 0
        assert!(g.contains("[0:v]trim=start=0:duration=10,"), "{g}");
        assert!(g.contains("[0:a]atrim=start=0:duration=10,"), "{g}");
        // gap 10→12 → black + silence
        assert!(g.contains("color=c=black:s=1920x1080:r=30/1:d=2"), "{g}");
        // clip 2: src 2–5 from input 1; b.mp4 has no audio → anullsrc
        assert!(g.contains("[1:v]trim=start=2:duration=3,"), "{g}");
        assert!(g.contains("anullsrc=r=48000:cl=stereo,atrim=duration=3"), "{g}");
        // concat of 3 segments, video then audio
        assert!(g.contains("[v0][v1][v2]concat=n=3:v=1:a=0[vout]"), "{g}");
        assert!(g.contains("[a0][a1][a2]concat=n=3:v=0:a=1[aout]"), "{g}");

        // frame math: 300 + 60 + 90 = 450 frames @ 30fps
        assert_eq!(plan.frames, 450);
        assert_eq!(plan.duration, Seconds::from_integer(15));
    }

    #[test]
    fn frame_snapping_keeps_cuts_exact() {
        let mut p = Project::new();
        let (a, _) = p.add_asset("a.mp4".into(), probe_report("15", 640, 480, "30000/1001", true));
        // in-point 0.5s → 14.985 frames → snaps to 15; out 12.5s → 374.6 → 375
        p.add_clip(a, None, Seconds::new(1, 2), Seconds::new(25, 2)).unwrap();
        let plan = compile_export(&p).unwrap();
        // seconds on the frame grid: 15/29.97 = 0.5005, 360 frames = 12.012s
        assert!(plan.graph.contains("trim=start=0.5005:duration=12.012,"), "{}", plan.graph);
        assert_eq!(plan.frames, 360);
    }

    #[test]
    fn rejects_empty_and_too_short() {
        let empty = Project::new();
        assert!(matches!(compile_export(&empty), Err(CompileError::Empty)));

        let mut p = Project::new();
        let (a, _) = p.add_asset("a.mp4".into(), probe_report("10", 640, 480, "30/1", true));
        // 0.01s = 0.3 frames @30 → less than one frame
        p.add_clip(a, None, Seconds::from_integer(0), Seconds::new(1, 100)).unwrap();
        assert!(matches!(
            compile_export(&p),
            Err(CompileError::TooShort(1))
        ));
    }

    #[test]
    fn trailing_tail_fills_the_export() {
        let mut p = project_with_two_clips();
        p.timeline.length = Some(Seconds::new(20, 1)); // clips end at 15
        let plan = compile_export(&p).unwrap();
        // gap [15..20) → black + silence, 150 extra frames
        assert!(plan.graph.contains("color=c=black:s=1920x1080:r=30/1:d=5"), "{}", plan.graph);
        assert!(plan.graph.contains("anullsrc=r=48000:cl=stereo,atrim=duration=5"), "{}", plan.graph);
        assert_eq!(plan.frames, 600);
        assert_eq!(plan.duration, Seconds::from_integer(20));
    }

    #[test]
    fn hw_encoders_route_vout_through_hwupload() {
        let (g, vmap) = apply_encoder_tail("[vout]\n".into(), Encoder::Vaapi);
        assert_eq!(vmap, "venc");
        assert!(g.contains("[vout]format=nv12,hwupload[venc]"), "{g}");

        let (g, _) = apply_encoder_tail("[vout]\n".into(), Encoder::Qsv);
        assert!(g.contains("[vout]format=nv12,hwupload=extra_hw_frames=64[venc]"), "{g}");

        let (g, vmap) = apply_encoder_tail("[vout]\n".into(), Encoder::Cpu);
        assert_eq!(vmap, "vout");
        assert_eq!(g, "[vout]\n");
    }
}
