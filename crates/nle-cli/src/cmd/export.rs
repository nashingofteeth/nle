//! `nile export`: choose a quality tier and render the timeline.

use std::path::Path;

use anyhow::{Context, Result};
use clap::ValueEnum;

use nle_core::Project;
use nle_ffmpeg::{AudioCodec, Encoder, ExportSpec};

use crate::cmd::media::parse_f64;
use crate::{log, say};

/// Quality tiers. These are deliberately tuned for this machine's UHD 630 +
/// 8-core CPU, not a portable quality matrix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum QualityPreset {
    /// VAAPI H.264; falls back to software if the GPU path fails.
    Preview,
    /// x264 software, good quality/speed balance.
    #[default]
    Balanced,
    /// x265 software, slow but archival.
    Max,
}

/// The export knobs after merging a tier with explicit overrides.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub preset: QualityPreset,
    pub encoder: Option<Encoder>,
    pub crf: Option<u8>,
    pub encoder_preset: Option<String>,
    pub bitrate: Option<String>,
    /// Number of segment workers; `None` stays single-worker.
    pub parallel: Option<usize>,
    /// Reuse completed chunk files. Benchmarks turn this off.
    pub use_cache: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            preset: QualityPreset::Balanced,
            encoder: None,
            crf: None,
            encoder_preset: None,
            bitrate: None,
            parallel: None,
            use_cache: true,
        }
    }
}

impl QualityPreset {
    pub(crate) fn label(self) -> &'static str {
        match self {
            QualityPreset::Preview => "preview",
            QualityPreset::Balanced => "balanced",
            QualityPreset::Max => "max",
        }
    }

    fn base(self) -> (Encoder, u8, &'static str, &'static str) {
        match self {
            QualityPreset::Preview => (Encoder::Vaapi, 26, "ultrafast", "6M"),
            QualityPreset::Balanced => (Encoder::Cpu, 22, "fast", "8M"),
            QualityPreset::Max => (Encoder::X265, 20, "medium", "8M"),
        }
    }
}

impl ExportOptions {
    /// Apply explicit flags over the tier defaults.
    pub fn spec(&self, output: &Path) -> ExportSpec {
        let (tier_encoder, mut crf, tier_preset, tier_bitrate) = self.preset.base();
        let encoder = self.encoder.unwrap_or(tier_encoder);
        let mut bitrate = tier_bitrate.to_string();
        let mut preset = match self.encoder {
            // Keep the tier's speed when the user only changes CPU/GPU engine.
            None | Some(Encoder::Cpu) => tier_preset.to_string(),
            Some(Encoder::X265) => "medium".to_string(),
            Some(Encoder::SvtAv1) => "6".to_string(),
            Some(Encoder::Vaapi | Encoder::Qsv) => "fast".to_string(),
        };
        if let Some(c) = self.crf {
            crf = c;
        }
        if let Some(p) = &self.encoder_preset {
            preset = p.clone();
        }
        if let Some(b) = &self.bitrate {
            bitrate = b.clone();
        }
        ExportSpec {
            output: output.to_path_buf(),
            encoder,
            crf,
            preset,
            bitrate,
            audio: AudioCodec::Aac,
        }
    }

    /// The software retry for a failed hardware encode.
    pub fn cpu_fallback(&self, spec: &ExportSpec) -> ExportSpec {
        let preset = if self.preset == QualityPreset::Preview {
            "ultrafast"
        } else {
            "fast"
        };
        ExportSpec {
            output: spec.output.clone(),
            encoder: Encoder::Cpu,
            crf: spec.crf,
            preset: preset.to_string(),
            bitrate: spec.bitrate.clone(),
            audio: spec.audio,
        }
    }
}

pub(crate) fn cmd_export(
    project_path: &Path,
    output: &Path,
    options: &ExportOptions,
) -> Result<()> {
    say!("{}", run_export(project_path, output, options, false)?);
    Ok(())
}

/// Render the timeline to `output`; returns the summary line. Split from
/// [`cmd_export`] so the TUI's `:export` can run the same code. `quiet`
/// suppresses ffmpeg's chatter for a worker or benchmark.
pub(crate) fn run_export(
    project_path: &Path,
    output: &Path,
    options: &ExportOptions,
    quiet: bool,
) -> Result<String> {
    let project = Project::load(project_path)?;
    // The scheduler is opt-in: a local benchmark showed no gain from
    // splitting an x264 (already multithreaded) or a short VAAPI export.
    // `--parallel N` is for benchmarking/experiments and for long-proxy
    // workloads where the caller measured a win.
    let workers = options.parallel.unwrap_or(1).max(1);
    let mut skipped = None;
    if workers > 1 {
        match crate::cmd::schedule::run(
            &project,
            project_path,
            output,
            options,
            workers,
            quiet,
            &mut skipped,
        ) {
            Ok(Some(summary)) => return Ok(summary),
            Ok(None) => {}
            Err(e) => {
                log::line(&format!("parallel export failed ({e}); retrying single"));
                skipped = Some("chunking failed");
            }
        }
    }
    let spec = options.spec(output);
    log::line(&format!(
        "export -> {} ({:?}, {:?})",
        output.display(),
        options.preset,
        spec.encoder
    ));
    let run = |spec: &ExportSpec| {
        if quiet {
            nle_ffmpeg::export_quiet(&project, spec)
        } else {
            nle_ffmpeg::export(&project, spec)
        }
    };
    let (plan, fallback) = match run(&spec) {
        Ok(plan) => (plan, false),
        Err(e) if matches!(spec.encoder, Encoder::Vaapi | Encoder::Qsv) => {
            log::line(&format!(
                "{} export failed ({e}); retrying on CPU",
                match spec.encoder {
                    Encoder::Vaapi => "VAAPI",
                    Encoder::Qsv => "QSV",
                    _ => unreachable!(),
                }
            ));
            let cpu = options.cpu_fallback(&spec);
            (run(&cpu)?, true)
        }
        Err(e) => return Err(e.into()),
    };
    let actual = nle_ffmpeg::probe(output)?;
    let shown = actual
        .format
        .duration
        .as_deref()
        .and_then(parse_f64)
        .map(|d| format!("{d:.2}"))
        .unwrap_or_else(|| "?".into());
    let tier = options.preset.label();
    let frames = nle_ffmpeg::frame_count(output).unwrap_or(plan.frames);
    let fallback = if fallback { " · CPU fallback" } else { "" };
    let skipped = skipped
        .map(|why| format!(" · single ({why})"))
        .unwrap_or_default();
    Ok(format!(
        "✓ {}  ({} s · {} frames @ {}/{}) · {tier}{fallback}{skipped}",
        output.display(),
        shown,
        frames,
        plan.fps.numer(),
        plan.fps.denom()
    ))
}

/// `nile bench [--runs N] [--preset TIER]`: render to a temp file and report
/// wall time and effective fps. Defaults to all three tiers, one run each.
pub(crate) fn cmd_bench(
    project_path: &Path,
    runs: usize,
    only: Option<QualityPreset>,
    parallel: Option<usize>,
) -> Result<()> {
    if runs == 0 {
        anyhow::bail!("--runs must be at least 1");
    }
    let project = Project::load(project_path)?;
    let duration = *project.timeline.end().numer() as f64 / *project.timeline.end().denom() as f64;
    let fps = project
        .fps
        .as_ref()
        .map(|f| *f.numer() as f64 / *f.denom() as f64)
        .unwrap_or(0.0);
    let frames = (duration * fps).round() as i64;
    say!(
        "project: {} — {duration:.2} s @ {:.3} fps · {frames} frames",
        project_path.display(),
        fps
    );
    say!("preset     run  time      fps     notes");
    let presets = match only {
        Some(p) => vec![p],
        None => vec![
            QualityPreset::Preview,
            QualityPreset::Balanced,
            QualityPreset::Max,
        ],
    };
    let dir = tempfile::tempdir().context("bench temp dir")?;
    for preset in presets {
        for run in 1..=runs {
            let out = dir.path().join(format!("{}-{run}.mp4", preset.label()));
            let options = ExportOptions {
                preset,
                parallel,
                use_cache: false,
                ..Default::default()
            };
            let started = std::time::Instant::now();
            let summary = run_export(project_path, &out, &options, true)?;
            let elapsed = started.elapsed().as_secs_f64();
            let actual = nle_ffmpeg::frame_count(&out).unwrap_or(frames);
            let effective = if elapsed > 0.0 {
                actual as f64 / elapsed
            } else {
                0.0
            };
            let notes = match (
                summary.contains("CPU fallback"),
                summary.contains("single ("),
            ) {
                (true, true) => "CPU fallback; single",
                (true, false) => "CPU fallback",
                (false, true) => "single",
                (false, false) => "",
            };
            say!(
                "{:<10} {:>3}  {:>7.2}s  {:>7.1}  {notes}",
                preset.label(),
                run,
                elapsed,
                effective
            );
            let _ = std::fs::remove_file(&out);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_to_the_expected_engines() {
        let out = Path::new("out.mp4");
        let preview = ExportOptions {
            preset: QualityPreset::Preview,
            ..Default::default()
        }
        .spec(out);
        assert_eq!(preview.encoder, Encoder::Vaapi);
        assert_eq!(preview.crf, 26);
        assert_eq!(preview.preset, "ultrafast");

        let balanced = ExportOptions::default().spec(out);
        assert_eq!(balanced.encoder, Encoder::Cpu);
        assert_eq!(balanced.crf, 22);

        let max = ExportOptions {
            preset: QualityPreset::Max,
            ..Default::default()
        }
        .spec(out);
        assert_eq!(max.encoder, Encoder::X265);
        assert_eq!(max.crf, 20);
        assert_eq!(max.preset, "medium");
    }

    #[test]
    fn explicit_overrides_win_without_mixing_codec_presets() {
        let out = Path::new("out.mp4");
        let options = ExportOptions {
            preset: QualityPreset::Balanced,
            encoder: Some(Encoder::SvtAv1),
            crf: Some(30),
            ..Default::default()
        };
        let spec = options.spec(out);
        assert_eq!(spec.encoder, Encoder::SvtAv1);
        assert_eq!(spec.crf, 30);
        assert_eq!(spec.preset, "6");

        let cpu = options.cpu_fallback(&spec);
        assert_eq!(cpu.encoder, Encoder::Cpu);
        assert_eq!(cpu.preset, "fast");
    }
}
