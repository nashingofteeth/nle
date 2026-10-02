//! ffprobe wrapper: ask ffmpeg's probe tool what's inside a file,
//! parse the JSON answer into typed structs.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("no such file: {0}")]
    MissingFile(String),
    #[error("could not run ffprobe: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("ffprobe failed ({status}): {message}")]
    Failed {
        status: std::process::ExitStatus,
        message: String,
    },
    #[error("could not parse ffprobe output: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Full ffprobe report: container info + one entry per stream.
///
/// `Option` everywhere = "this field may be absent" — ffprobe omits fields
/// it doesn't know (no duration for some streams, no width for audio, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawProbe {
    #[serde(default)]
    pub streams: Vec<RawStream>,
    #[serde(default)]
    pub format: RawFormat,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawFormat {
    #[serde(default)]
    pub format_name: String,
    /// Seconds as a string ("12.500000") — ffprobe's quirk, we reparse.
    pub duration: Option<String>,
    /// Total size in bytes, as a string.
    pub size: Option<String>,
    /// Overall bit rate (bits/second), as a string.
    pub bit_rate: Option<String>,
    /// Container tags: major_brand, encoder, ...
    pub tags: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawStream {
    pub index: u32,
    /// "video" | "audio" | "subtitle" | "data"
    pub codec_type: Option<String>,
    /// "h264", "aac", ...
    pub codec_name: Option<String>,
    pub profile: Option<String>,
    // video
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Frame rate as a fraction, e.g. "30000/1001".
    pub r_frame_rate: Option<String>,
    pub pix_fmt: Option<String>,
    // audio
    /// Samples per second as a string ("48000").
    pub sample_rate: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    // either
    pub bit_rate: Option<String>,
}

/// Run ffprobe on `path` and return the parsed report.
pub fn probe(path: &Path) -> Result<RawProbe, ProbeError> {
    if !path.is_file() {
        return Err(ProbeError::MissingFile(path.display().to_string()));
    }
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()?;
    if !out.status.success() {
        return Err(ProbeError::Failed {
            status: out.status,
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}
