//! `nle export`: compile the timeline to a filter graph and run ffmpeg.

use std::path::Path;

use anyhow::Result;

use nle_core::Project;
use nle_ffmpeg::{Encoder, ExportSpec};

use crate::cmd::media::parse_f64;
use crate::{log, say};
pub(crate) fn cmd_export(
    project_path: &Path,
    output: &Path,
    encoder: Encoder,
    crf: u8,
    preset: &str,
    bitrate: &str,
) -> Result<()> {
    say!(
        "{}",
        run_export(project_path, output, encoder, crf, preset, bitrate)?
    );
    Ok(())
}

/// Render the timeline to `output`; returns the summary line. Split from
/// [`cmd_export`] so the TUI's `:export` can run the same code.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_export(
    project_path: &Path,
    output: &Path,
    encoder: Encoder,
    crf: u8,
    preset: &str,
    bitrate: &str,
) -> Result<String> {
    let project = Project::load(project_path)?;
    let spec = ExportSpec {
        output: output.to_path_buf(),
        encoder,
        crf,
        preset: preset.to_string(),
        bitrate: bitrate.to_string(),
    };
    log::line(&format!(
        "export -> {} ({:?})",
        output.display(),
        spec.encoder
    ));
    let plan = nle_ffmpeg::export(&project, &spec)?;
    let actual = nle_ffmpeg::probe(output)?;
    let shown = actual
        .format
        .duration
        .as_deref()
        .and_then(parse_f64)
        .map(|d| format!("{d:.2}"))
        .unwrap_or_else(|| "?".into());
    Ok(format!(
        "✓ {}  ({} s · {} frames @ {}/{})",
        output.display(),
        shown,
        plan.frames,
        plan.fps.numer(),
        plan.fps.denom()
    ))
}
