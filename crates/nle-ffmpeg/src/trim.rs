//! Cutting segments: translate a TrimSpec into ffmpeg arguments and run it.
//!
//! Strategy: `-ss` BEFORE `-i` = ffmpeg seeks at the demuxer (fast) then
//! decodes precisely up to the target frame — fast AND frame-accurate.
//! `-t` after input limits how long the output runs.
//!
//! The helpers here (device selection, codec args, the process runner)
//! are shared with the export graph compiler in `graph.rs`.

use std::path::PathBuf;
use std::process::Command;

use nle_core::{Seconds, format_seconds};
use thiserror::Error;

/// This machine's encode engines. Intel UHD 630 supports both hardware
/// APIs (VAAPI + QSV); the CPU path is libx264.
const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// The VAAPI render node. Picks the first `/dev/dri/renderD*` at runtime —
/// the node number can change after a driver/kernel update — overridable
/// with `NLE_VAAPI_DEVICE`. Falls back to `renderD128` when the directory
/// can't be read.
pub fn vaapi_device() -> String {
    if let Ok(p) = std::env::var("NLE_VAAPI_DEVICE") {
        return p;
    }
    pick_render_node(read_render_nodes()).unwrap_or_else(|| DEFAULT_VAAPI_DEVICE.to_string())
}

/// Lowest-numbered render node, as an absolute path.
fn pick_render_node(mut nodes: Vec<String>) -> Option<String> {
    nodes.sort();
    nodes.into_iter().next().map(|n| format!("/dev/dri/{n}"))
}

fn read_render_nodes() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("renderD"))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    /// libx264 — software, best quality per bit, many CPU cores.
    Cpu,
    /// Intel UHD 630 via VAAPI — near-zero CPU cost (previews/proxies).
    Vaapi,
    /// Intel Quick Sync (same GPU, different API).
    Qsv,
}

#[derive(Debug, Error)]
pub enum FfmpegError {
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("ffmpeg failed with {0}")]
    Failed(std::process::ExitStatus),
}

#[derive(Debug, Error)]
pub enum TrimError {
    #[error("invalid range: end ({end}) must be after start ({start})")]
    Range { start: String, end: String },
    #[error(transparent)]
    Ffmpeg(#[from] FfmpegError),
}

/// Everything needed to perform one cut.
#[derive(Debug, Clone)]
pub struct TrimSpec {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Inclusive start (seconds, exact).
    pub start: Seconds,
    /// Exclusive end (seconds, exact).
    pub end: Seconds,
    pub encoder: Encoder,
    /// Stream copy instead of re-encode: instant, starts exactly at
    /// `start` (mp4 edit list), end lands within a frame or two.
    pub copy: bool,
    pub crf: u8,
    pub preset: String,
    pub bitrate: String,
}

/// Global args that select and initialize a hardware device.
///
/// `hw_decode` also decodes the input on the GPU (QSV trim does; the
/// export graph decodes in software so filters can process frames).
pub fn device_args(encoder: Encoder, hw_decode: bool) -> Vec<String> {
    let mut a = Vec::new();
    match encoder {
        Encoder::Cpu => {}
        Encoder::Vaapi => {
            a.push("-init_hw_device".into());
            a.push(format!("vaapi=va:{}", vaapi_device()));
            a.push("-filter_hw_device".into());
            a.push("va".into());
        }
        Encoder::Qsv => {
            a.push("-init_hw_device".into());
            a.push("qsv=qsv".into());
            if hw_decode {
                a.push("-hwaccel".into());
                a.push("qsv".into());
            }
        }
    }
    a
}

/// Output-side codec args (`-c:v ...`). Video only; audio is always AAC.
pub fn video_encode_args(encoder: Encoder, crf: u8, preset: &str, bitrate: &str) -> Vec<String> {
    let mut a = Vec::new();
    match encoder {
        Encoder::Cpu => {
            a.push("-c:v".into());
            a.push("libx264".into());
            a.push("-preset".into());
            a.push(preset.into());
            a.push("-crf".into());
            a.push(crf.to_string());
        }
        Encoder::Vaapi => {
            a.push("-c:v".into());
            a.push("h264_vaapi".into());
            a.push("-b:v".into());
            a.push(bitrate.into());
        }
        Encoder::Qsv => {
            a.push("-c:v".into());
            a.push("h264_qsv".into());
            a.push("-b:v".into());
            a.push(bitrate.into());
        }
    }
    a
}

/// Run ffmpeg, inheriting stdio (its live progress goes to our stderr).
pub fn run_ffmpeg(args: &[String]) -> Result<(), FfmpegError> {
    let status = Command::new("ffmpeg").args(args).status()?;
    if !status.success() {
        return Err(FfmpegError::Failed(status));
    }
    Ok(())
}

/// Build the argument list for a trim. Exposed so tests (and later the
/// graph compiler) can inspect the exact command without running it.
pub fn build_args(spec: &TrimSpec) -> Result<Vec<String>, TrimError> {
    if spec.end <= spec.start {
        return Err(TrimError::Range {
            start: format_seconds(&spec.start),
            end: format_seconds(&spec.end),
        });
    }
    let duration = spec.end - spec.start;

    let mut a: Vec<String> = Vec::new();
    a.push("-y".to_string()); // overwrite output without asking
    a.extend(device_args(spec.encoder, true));

    a.push("-ss".into());
    a.push(format_seconds(&spec.start));
    a.push("-i".into());
    a.push(spec.input.display().to_string());
    a.push("-t".into());
    a.push(format_seconds(&duration));

    if spec.copy {
        a.push("-c".into());
        a.push("copy".into());
        // NOTE: deliberately NO `-avoid_negative_ts make_zero` — it shifts
        // the pre-start packets back into the output (measured: 6.09s
        // instead of 4s). The mp4 muxer's edit list hides the pre-start
        // material, giving the exact requested window with clean decode.
    } else {
        // Video only: route frames onto the GPU where the encoder wants them.
        if spec.encoder == Encoder::Vaapi {
            a.push("-vf".into());
            a.push("format=nv12,hwupload".into());
        }
        a.extend(video_encode_args(
            spec.encoder,
            spec.crf,
            &spec.preset,
            &spec.bitrate,
        ));
        a.push("-c:a".into());
        a.push("aac".into());
        a.push("-b:a".into());
        a.push("192k".into());
        a.push("-movflags".into());
        a.push("+faststart".into());
    }

    // `--` ends option parsing: a file literally named "-version.mp4" can't
    // be misread as an option. (Inputs via `-i` are safe — option values
    // are consumed literally — but the trailing OUTPUT is positional.)
    a.push("--".into());
    a.push(spec.output.display().to_string());
    Ok(a)
}

/// Execute a trim.
pub fn trim(spec: &TrimSpec) -> Result<(), TrimError> {
    let args = build_args(spec)?;
    eprintln!("$ ffmpeg {}", args.join(" "));
    run_ffmpeg(&args)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TrimSpec {
        TrimSpec {
            input: "in.mp4".into(),
            output: "out.mp4".into(),
            start: Seconds::new(25, 2), // 12.5
            end: Seconds::from_integer(20),
            encoder: Encoder::Cpu,
            copy: false,
            crf: 18,
            preset: "fast".into(),
            bitrate: "8M".into(),
        }
    }

    #[test]
    fn cpu_args_accurate_cut() {
        let args = build_args(&spec()).unwrap();
        let line = args.join(" ");
        // seek before input, duration after → fast + accurate
        assert!(line.contains("-ss 12.5 -i in.mp4 -t 7.5"), "{line}");
        assert!(line.contains("-c:v libx264"), "{line}");
        // `--` guards the positional output against option-injection
        assert_eq!(args.last().map(String::as_str), Some("out.mp4"));
        assert_eq!(args[args.len() - 2], "--");
    }

    #[test]
    fn vaapi_device_picks_the_lowest_render_node() {
        assert_eq!(
            pick_render_node(vec!["renderD129".into(), "renderD128".into()]),
            Some("/dev/dri/renderD128".into())
        );
        assert_eq!(pick_render_node(vec![]), None);
        assert!(vaapi_device().starts_with("/dev/dri/"));
    }

    #[test]
    fn vaapi_args_name_a_device() {
        let line = device_args(Encoder::Vaapi, false).join(" ");
        assert!(line.contains("vaapi=va:/dev/dri/"), "{line}");
    }

    #[test]
    fn copy_args_skip_encoding() {
        let mut s = spec();
        s.copy = true;
        let line = build_args(&s).unwrap().join(" ");
        assert!(line.contains("-c copy"), "{line}");
        assert!(!line.contains("libx264"), "{line}");
        // make_zero would re-include pre-start packets (bloats the output);
        // see the note in build_args.
        assert!(!line.contains("avoid_negative_ts"), "{line}");
    }

    #[test]
    fn vaapi_args_init_device_and_upload() {
        let mut s = spec();
        s.encoder = Encoder::Vaapi;
        let line = build_args(&s).unwrap().join(" ");
        assert!(line.contains("-init_hw_device vaapi=va:"), "{line}");
        assert!(line.contains("format=nv12,hwupload"), "{line}");
        assert!(line.contains("-c:v h264_vaapi"), "{line}");
        // device init must appear before -i
        assert!(
            line.find("-init_hw_device").unwrap() < line.find(" -i ").unwrap(),
            "{line}"
        );
    }

    #[test]
    fn qsv_trim_decodes_on_gpu() {
        let mut s = spec();
        s.encoder = Encoder::Qsv;
        let line = build_args(&s).unwrap().join(" ");
        assert!(line.contains("-hwaccel qsv"), "{line}");
        assert!(line.contains("-c:v h264_qsv"), "{line}");
        // trim decodes on the GPU; the export graph does NOT (see graph.rs)
    }

    #[test]
    fn rejects_inverted_range() {
        let mut s = spec();
        s.start = Seconds::from_integer(10);
        s.end = Seconds::from_integer(10);
        assert!(matches!(build_args(&s), Err(TrimError::Range { .. })));
    }
}
