//! Scene-cut + black-interval detection: the *effect* half of "edit detect".
//!
//! One ffmpeg pass with no output file. `blackdetect` reports black
//! intervals (gaps), and `select='gt(scene,T)',showinfo` reports the first
//! frame of each new shot (a hard cut). Everything is parsed out of stderr.
//!
//! The parsing is a pure function ([`parse_log`]) and the argv is built by
//! [`build_args`], so both are unit-testable without spawning ffmpeg —
//! mirroring `trim::build_args`. Only [`detect`] touches a process.

use std::path::Path;
use std::process::Command;

use nle_core::time::{Seconds, format_seconds};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DetectError {
    #[error("no such file: {0}")]
    MissingFile(String),
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("ffmpeg failed ({status}): {message}")]
    Failed {
        status: std::process::ExitStatus,
        message: String,
    },
}

/// Knobs for a detection pass. Defaults favour hard cuts over flashes.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectOptions {
    /// Scene-change score in 0..1 — a higher score is a bigger change. A
    /// cut to/from black often scores *low*, which is why gaps are found
    /// separately with `blackdetect`.
    pub threshold: f32,
    /// Shortest black interval to report as a gap.
    pub black_min: Seconds,
    /// Luminance (0..1) below which a pixel counts as black.
    pub black_pix: f32,
}

impl Default for DetectOptions {
    fn default() -> Self {
        Self {
            threshold: 0.4,
            black_min: Seconds::new(1, 10),
            black_pix: 0.10,
        }
    }
}

/// Raw detection output: cut points and black intervals, in seconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detection {
    /// Times (first frame of each new shot) where a hard cut was detected.
    pub cuts: Vec<Seconds>,
    /// `(start, end)` of each detected black interval.
    pub blacks: Vec<(Seconds, Seconds)>,
}

/// Build the ffmpeg argv. Exposed so tests can inspect the command without
/// running it. The `-vf` value carries ffmpeg-level `'` quoting around the
/// `select` expression: its comma would otherwise split the filtergraph.
pub fn build_args(path: &Path, opts: &DetectOptions) -> Vec<String> {
    let filter = format!(
        "blackdetect=d={}:pix_th={},select='gt(scene,{})',showinfo",
        format_seconds(&opts.black_min),
        opts.black_pix,
        opts.threshold,
    );
    vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-v".into(),
        "info".into(),
        "-i".into(),
        path.display().to_string(),
        "-an".into(),
        "-vf".into(),
        filter,
        "-f".into(),
        "null".into(),
        "-".into(),
    ]
}

/// Run ffmpeg on `path` and return the cuts + black intervals it reports.
pub fn detect(path: &Path, opts: &DetectOptions) -> Result<Detection, DetectError> {
    if !path.is_file() {
        return Err(DetectError::MissingFile(path.display().to_string()));
    }
    let out = Command::new("ffmpeg")
        .args(build_args(path, opts))
        .output()?;
    if !out.status.success() {
        return Err(DetectError::Failed {
            status: out.status,
            message: String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or_default()
                .to_string(),
        });
    }
    Ok(parse_log(&String::from_utf8_lossy(&out.stderr)))
}

/// Pull cuts and black intervals out of ffmpeg's stderr. Pure.
///
/// `blackdetect` lines look like `black_start:6 black_end:7 black_duration:1`;
/// `showinfo` lines carry `pts_time:<seconds>`. Anything else is ignored.
pub fn parse_log(stderr: &str) -> Detection {
    let mut detection = Detection::default();
    for line in stderr.lines() {
        if let Some(rest) = line.split("black_start:").nth(1) {
            let start = leading_number(rest);
            let end = rest.split("black_end:").nth(1).and_then(leading_number);
            if let (Some(start), Some(end)) = (start, end) {
                detection.blacks.push((start, end));
            }
        } else if let Some(rest) = line.split("pts_time:").nth(1)
            && let Some(t) = leading_number(rest)
        {
            detection.cuts.push(t);
        }
    }
    detection
}

/// The leading decimal number of `s` ("7 black_end:..." → 7). `None` when
/// `s` doesn't start with a number.
fn leading_number(s: &str) -> Option<Seconds> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let value: f64 = s[..end].parse().ok()?;
    Seconds::approximate_float(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slice of real ffmpeg stderr: one showinfo config line (no
    /// `pts_time`), two cut frames, and one black interval.
    const LOG: &str = "\
[Parsed_showinfo_1 @ 0x55] config in time_base: 1/15360, frame_rate: 30/1
[Parsed_showinfo_1 @ 0x55] n:   0 pts:  46080 pts_time:3 duration: 512 duration_time:0.0333333 fmt:yuv420p
[Parsed_blackdetect_0 @ 0x55] black_start:6 black_end:7 black_duration:1
[Parsed_showinfo_1 @ 0x55] n:  12 pts: 107520 pts_time:7 duration: 512 duration_time:0.0333333 fmt:yuv420p
[out#0/null @ 0x55] video:0kB audio:0kB subtitle:0kB other streams:0kB
";

    #[test]
    fn parses_cuts_and_blacks() {
        let d = parse_log(LOG);
        assert_eq!(
            d.cuts,
            vec![Seconds::from_integer(3), Seconds::from_integer(7)]
        );
        assert_eq!(
            d.blacks,
            vec![(Seconds::from_integer(6), Seconds::from_integer(7))]
        );
    }

    #[test]
    fn parses_fractional_times() {
        // ffmpeg prints decimals, not exact rationals; `reconstruct` snaps
        // to the frame grid later, so approximate equality is the contract.
        let d = parse_log("[x] pts_time:0.0333333\n[y] pts_time:12.5\n");
        assert_eq!(d.cuts.len(), 2);
        let first = *d.cuts[0].numer() as f64 / *d.cuts[0].denom() as f64;
        assert!((first - 1.0 / 30.0).abs() < 1e-6);
        assert_eq!(d.cuts[1], Seconds::new(25, 2));
    }

    #[test]
    fn ignores_unrelated_lines() {
        let d = parse_log("frame_rate: 30/1\ntime=00:00:09.00 bitrate=1.0kbits/s\n");
        assert_eq!(d, Detection::default());
    }

    #[test]
    fn build_args_quotes_the_select_expression() {
        let args = build_args(
            Path::new("/tmp/a b.mp4"),
            &DetectOptions {
                threshold: 0.35,
                black_min: Seconds::new(1, 5),
                black_pix: 0.08,
            },
        );
        assert!(args.contains(&"/tmp/a b.mp4".to_string()));
        let vf = args.iter().position(|a| a == "-vf").map(|i| &args[i + 1]);
        let vf = vf.expect("-vf present");
        assert!(vf.contains("blackdetect=d=0.2:pix_th=0.08"));
        assert!(vf.contains("select='gt(scene,0.35)'"));
        assert!(vf.contains("showinfo"));
    }
}
