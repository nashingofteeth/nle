//! nle — terminal-native video editor.
//!
//! M0 status: `probe` and `trim` — the ffmpeg plumbing. The editor itself
//! (EDL model, TUI, mpv monitor) arrives in M1/M2.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use nle_core::{parse_rate, parse_time, Seconds};
use nle_ffmpeg::{probe, Encoder, RawProbe, RawStream, TrimSpec};

#[derive(Parser)]
#[command(name = "nle", version, about = "Terminal-native video editor")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what's inside a media file (streams, codecs, rates)
    Probe {
        /// Media file to inspect
        file: PathBuf,
        /// Print raw ffprobe JSON instead of a summary
        #[arg(long)]
        json: bool,
    },
    /// Cut [in, out) out of a file into a new file
    Trim {
        /// Source file
        input: PathBuf,
        /// Start: seconds ("12.5") or timecode ("0:01:23.500")
        #[arg(long = "in", value_name = "TIME")]
        in_point: String,
        /// End (same formats, exclusive)
        #[arg(long = "out", value_name = "TIME")]
        out_point: String,
        /// Where to write the result
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        /// Encode engine: cpu (libx264), vaapi (Intel GPU), qsv (Quick Sync)
        #[arg(long, value_enum, default_value = "cpu")]
        encoder: EncoderArg,
        /// Stream copy: instant + lossless; starts exactly at --in, end lands
        /// within a frame or two of --out (packet boundaries)
        #[arg(long)]
        copy: bool,
        /// x264 quality, 0-51, lower = better [cpu only]
        #[arg(long, default_value_t = 18)]
        crf: u8,
        /// x264 preset: ultrafast..veryslow [cpu only]
        #[arg(long, default_value = "fast")]
        preset: String,
        /// Target bitrate for hardware encoders [vaapi/qsv]
        #[arg(long, default_value = "8M")]
        bitrate: String,
    },
}

/// CLI spelling of the encoder choices (clap needs ValueEnum to build
/// an argument parser from it).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncoderArg {
    Cpu,
    Vaapi,
    Qsv,
}

impl From<EncoderArg> for Encoder {
    fn from(v: EncoderArg) -> Self {
        match v {
            EncoderArg::Cpu => Encoder::Cpu,
            EncoderArg::Vaapi => Encoder::Vaapi,
            EncoderArg::Qsv => Encoder::Qsv,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Probe { file, json } => cmd_probe(&file, json),
        Cmd::Trim {
            input,
            in_point,
            out_point,
            output,
            encoder,
            copy,
            crf,
            preset,
            bitrate,
        } => cmd_trim(
            &input,
            &in_point,
            &out_point,
            &output,
            encoder.into(),
            copy,
            crf,
            &preset,
            &bitrate,
        ),
    }
}

// ---------------------------------------------------------------- probe

fn cmd_probe(file: &Path, json: bool) -> Result<()> {
    let report = probe(file)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print_summary(file, &report);
    Ok(())
}

fn print_summary(path: &Path, report: &RawProbe) {
    println!("{}", path.display());

    let f = &report.format;
    let mut container = f.format_name.clone();
    if let Some(brand) = f.tags.as_ref().and_then(|t| t.get("major_brand")) {
        container.push_str(&format!(" ({brand})"));
    }
    let mut meta = vec![container];
    if let Some(d) = f.duration.as_deref().and_then(parse_f64) {
        meta.push(format!("{d:.2} s"));
    }
    if let Some(b) = f.size.as_deref().and_then(parse_u64) {
        meta.push(human_bytes(b));
    }
    if let Some(b) = f.bit_rate.as_deref().and_then(parse_u64) {
        meta.push(format!("{} kb/s", b / 1000));
    }
    println!("  container  {}", meta.join("  ·  "));

    for s in &report.streams {
        match s.codec_type.as_deref() {
            Some("video") => println!("  video #{}   {}", s.index, video_desc(s)),
            Some("audio") => println!("  audio #{}   {}", s.index, audio_desc(s)),
            other => println!(
                "  {} #{}   {}",
                other.unwrap_or("data"),
                s.index,
                s.codec_name.as_deref().unwrap_or("?")
            ),
        }
    }
}

fn video_desc(s: &RawStream) -> String {
    let mut bits = Vec::new();
    let codec = s.codec_name.clone().unwrap_or_else(|| "?".into());
    bits.push(match &s.profile {
        Some(p) => format!("{codec} ({p})"),
        None => codec,
    });
    if let (Some(w), Some(h)) = (s.width, s.height) {
        bits.push(format!("{w}×{h}"));
    }
    if let Some(rate) = s.r_frame_rate.as_deref().and_then(parse_rate) {
        let decimal = *rate.numer() as f64 / *rate.denom() as f64;
        bits.push(format!("{}/{} fps ({decimal:.2})", rate.numer(), rate.denom()));
    }
    if let Some(p) = &s.pix_fmt {
        bits.push(p.clone());
    }
    if let Some(b) = s.bit_rate.as_deref().and_then(parse_u64) {
        bits.push(format!("{} kb/s", b / 1000));
    }
    bits.join("  ·  ")
}

fn audio_desc(s: &RawStream) -> String {
    let mut bits = Vec::new();
    bits.push(s.codec_name.clone().unwrap_or_else(|| "?".into()));
    if let Some(sr) = &s.sample_rate {
        bits.push(format!("{sr} Hz"));
    }
    if let Some(layout) = &s.channel_layout {
        bits.push(layout.clone());
    } else if let Some(c) = s.channels {
        bits.push(format!("{c} ch"));
    }
    if let Some(b) = s.bit_rate.as_deref().and_then(parse_u64) {
        bits.push(format!("{} kb/s", b / 1000));
    }
    bits.join("  ·  ")
}

fn parse_f64(s: &str) -> Option<f64> {
    s.parse().ok()
}

fn parse_u64(s: &str) -> Option<u64> {
    s.parse().ok()
}

fn human_bytes(b: u64) -> String {
    const MIB: f64 = (1 << 20) as f64;
    if b >= 1 << 20 {
        format!("{:.1} MiB", b as f64 / MIB)
    } else {
        format!("{:.0} KiB", b as f64 / 1024.0)
    }
}

// ---------------------------------------------------------------- trim

#[allow(clippy::too_many_arguments)]
fn cmd_trim(
    input: &Path,
    in_point: &str,
    out_point: &str,
    output: &Path,
    encoder: Encoder,
    copy: bool,
    crf: u8,
    preset: &str,
    bitrate: &str,
) -> Result<()> {
    let start = parse_time(in_point).with_context(|| format!("invalid --in time: {in_point:?}"))?;
    let end = parse_time(out_point).with_context(|| format!("invalid --out time: {out_point:?}"))?;
    let duration = end.clone() - start.clone();
    if duration <= Seconds::from_integer(0) {
        bail!("--out ({out_point}) must be later than --in ({in_point})");
    }

    let spec = TrimSpec {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        start,
        end,
        encoder,
        copy,
        crf,
        preset: preset.to_string(),
        bitrate: bitrate.to_string(),
    };
    nle_ffmpeg::trim(&spec)?;
    // Report the ACTUAL duration of what we wrote, not what we asked for —
    // stream copy lands within a frame or two of the request.
    let actual = nle_ffmpeg::probe(&spec.output)?;
    let shown = match actual.format.duration.as_deref().and_then(parse_f64) {
        Some(d) => format!("{d:.2} s"),
        None => "?".to_string(),
    };
    println!("✓ {}  ({})", output.display(), shown);
    Ok(())
}
