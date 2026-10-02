//! ffprobe wrapper: ask ffmpeg's probe tool what's inside a file,
//! parse the JSON answer into typed structs.
//!
//! The types (RawProbe etc.) live in nle-core::media — pure data shared
//! with the project file. This module only owns the *process* side.

use std::path::Path;
use std::process::Command;

use nle_core::media::RawProbe;
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
