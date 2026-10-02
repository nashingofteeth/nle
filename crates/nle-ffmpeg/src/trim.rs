//! Cutting segments: translate a TrimSpec into ffmpeg arguments and run it.
//!
//! Strategy: `-ss` BEFORE `-i` = ffmpeg seeks at the demuxer (fast) then
//! decodes precisely up to the target frame — fast AND frame-accurate.
//! `-t` after input limits how long the output runs.

use std::path::PathBuf;
use std::process::Command;

use nle_core::{format_seconds, Seconds};
use thiserror::Error;

/// This machine's encode engines. Intel UHD 630 supports both hardware
/// APIs (VAAPI + QSV); the CPU path is libx264.
// TODO(M1): detect devices at runtime instead of hardcoding the path.
const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    /// libx264 — software, best quality per bit, many CPU cores.
    Cpu,
    /// Intel UHD 630 via VAAPI — near-zero CPU cost (previews/proxies).
    Vaapi,
    /// Intel Quick Sync (same GPU, different API).
    Qsv,
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
    /// Stream copy instead of re-encode: instant, but snaps to keyframes.
    pub copy: bool,
    pub crf: u8,
    pub preset: String,
    pub bitrate: String,
}

#[derive(Debug, Error)]
pub enum TrimError {
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("invalid range: end ({end}) must be after start ({start})")]
    Range { start: String, end: String },
    #[error("ffmpeg failed with {status}")]
    Failed { status: std::process::ExitStatus },
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
    let duration = spec.end.clone() - spec.start.clone();

    let mut a: Vec<String> = Vec::new();
    a.push("-y".to_string()); // overwrite output without asking

    // Global hardware-device setup must come before any input.
    match spec.encoder {
        Encoder::Cpu => {}
        Encoder::Vaapi => {
            a.push("-init_hw_device".into());
            a.push(format!("vaapi=va:{VAAPI_DEVICE}"));
            a.push("-filter_hw_device".into());
            a.push("va".into());
        }
        Encoder::Qsv => {
            a.push("-init_hw_device".into());
            a.push("qsv=qsv".into());
            // Decode straight into QSV surfaces → encoder consumes them
            // without ever touching system memory.
            a.push("-hwaccel".into());
            a.push("qsv".into());
        }
    }

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
        // the pre-start packets back into the output (measured: 6.09s instead
        // of 4s). The mp4 muxer drops negative-timestamp packets itself,
        // which gives us the exact requested window.
    } else {
        match spec.encoder {
            Encoder::Cpu => {
                a.push("-c:v".into());
                a.push("libx264".into());
                a.push("-preset".into());
                a.push(spec.preset.clone());
                a.push("-crf".into());
                a.push(spec.crf.to_string());
            }
            Encoder::Vaapi => {
                // Software frames → GPU surfaces → hardware encode.
                a.push("-vf".into());
                a.push("format=nv12,hwupload".into());
                a.push("-c:v".into());
                a.push("h264_vaapi".into());
                a.push("-b:v".into());
                a.push(spec.bitrate.clone());
            }
            Encoder::Qsv => {
                a.push("-c:v".into());
                a.push("h264_qsv".into());
                a.push("-b:v".into());
                a.push(spec.bitrate.clone());
            }
        }
        a.push("-c:a".into());
        a.push("aac".into());
        a.push("-b:a".into());
        a.push("192k".into());
        a.push("-movflags".into());
        a.push("+faststart".into());
    }

    a.push(spec.output.display().to_string());
    Ok(a)
}

/// Execute a trim. ffmpeg inherits our stderr, so you see its live
/// progress bar; we just check the exit code.
pub fn trim(spec: &TrimSpec) -> Result<(), TrimError> {
    let args = build_args(spec)?;
    eprintln!("$ ffmpeg {}", args.join(" "));
    let status = Command::new("ffmpeg").args(&args).status()?;
    if !status.success() {
        return Err(TrimError::Failed { status });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TrimSpec {
        TrimSpec {
            input: "in.mp4".into(),
            output: "out.mp4".into(),
            start: Seconds::new(25, 2),   // 12.5
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
        assert!(line.find("-init_hw_device").unwrap() < line.find(" -i ").unwrap(), "{line}");
    }

    #[test]
    fn rejects_inverted_range() {
        let mut s = spec();
        s.start = Seconds::from_integer(10);
        s.end = Seconds::from_integer(10);
        assert!(matches!(build_args(&s), Err(TrimError::Range { .. })));
    }
}
