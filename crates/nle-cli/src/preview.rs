//! Instant preview: translate the timeline into an mpv `edl://` URL.
//!
//! mpv's EDL protocol concatenates trimmed segments of real files into one
//! virtual timeline — so cuts preview instantly, with no rendering, and the
//! playhead mpv reports back is already in *timeline* coordinates.
//!
//! Gaps become segments of a cached black file: mpv refuses protocol URLs
//! (like `av://lavfi:...`) inside EDL entries for security reasons, but
//! plain files are always allowed. Long gaps are split into chunks no
//! longer than the black file (mpv can't address past a segment's source
//! length), so one black file per geometry serves every gap length.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use nle_core::timeline::Segment as TimelineSegment;
use nle_core::{format_seconds, Project, Seconds};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PreviewError {
    #[error("timeline is empty — add clips first")]
    Empty,
    #[error("no video track in timeline")]
    NoVideoTrack,
    #[error("asset #{0} is referenced but missing from the media pool")]
    MissingAsset(u32),
    #[error("timeline has gaps but no output size set — add a clip first")]
    NoSize,
    #[error("no frame rate set — add a clip first")]
    NoFps,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Quote an EDL value with the `%len%` scheme. Allowed (and required) when
/// the value contains `,` or `;` — we always quote, so nothing is special.
pub fn edl_quote(value: &str) -> String {
    format!("%{}%{}", value.len(), value)
}

/// One preview segment in timeline order.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Clip {
        path: PathBuf,
        /// Source in-point (seconds).
        start: Seconds,
        /// Segment duration (seconds).
        length: Seconds,
    },
    Gap {
        length: Seconds,
    },
}

/// Flatten the video track into ordered preview segments.
pub fn segments(project: &Project) -> Result<Vec<Segment>, PreviewError> {
    let track = project
        .timeline
        .video_track()
        .ok_or(PreviewError::NoVideoTrack)?;
    if track.segments.is_empty() {
        return Err(PreviewError::Empty);
    }

    let mut segs = Vec::new();
    for seg in &track.segments {
        match seg {
            TimelineSegment::Clip(c) => {
                let asset = project
                    .asset(c.asset)
                    .ok_or(PreviewError::MissingAsset(c.asset))?;
                segs.push(Segment::Clip {
                    path: asset.path.clone(),
                    start: c.asset_in,
                    length: c.duration(),
                });
            }
            TimelineSegment::Gap(g) => segs.push(Segment::Gap {
                length: g.duration,
            }),
        }
    }
    Ok(segs)
}

/// Length of the cached black filler file in seconds. Gaps longer than
/// this are emitted as multiple EDL entries (mpv can't seek past a
/// segment's source length).
pub const BLACK_FILE_SECS: i64 = 10;

/// Render segments to EDL entries (the part after `edl://`).
/// `black` is required only when a gap exists. Gaps longer than the
/// black file are split into `BLACK_FILE_SECS`-sized chunks.
pub fn assemble_edl(segments: &[Segment], black: Option<&Path>) -> Result<String, PreviewError> {
    let mut entries = Vec::new();
    for seg in segments {
        match seg {
            Segment::Clip { path, start, length } => entries.push(format!(
                "{},{}",
                edl_quote(&path.display().to_string()),
                fmt_entry(*start, *length)
            )),
            Segment::Gap { length } => {
                let black = black.ok_or(PreviewError::NoSize)?;
                let quoted = edl_quote(&black.display().to_string());
                let chunk = Seconds::from_integer(BLACK_FILE_SECS);
                let mut remaining = *length;
                while remaining > chunk {
                    entries.push(format!("{quoted},0,{}", format_seconds(&chunk)));
                    remaining -= chunk;
                }
                entries.push(format!("{quoted},0,{}", format_seconds(&remaining)));
            }
        }
    }
    Ok(entries.join(";"))
}

fn fmt_entry(start: Seconds, length: Seconds) -> String {
    format!("{},{}", format_seconds(&start), format_seconds(&length))
}

/// Build the full `edl://` URL for a project, generating the black gap
/// file if the timeline has gaps.
pub fn build_edl(project: &Project) -> Result<String, PreviewError> {
    let segs = segments(project)?;
    let has_gap = segs.iter().any(|s| matches!(s, Segment::Gap { .. }));
    let black = if has_gap {
        let size = project.video.as_ref().ok_or(PreviewError::NoSize)?;
        let fps = project.fps.ok_or(PreviewError::NoFps)?;
        Some(ensure_black_file(size.width, size.height, &fps)?)
    } else {
        None
    };
    Ok(format!("edl://{}", assemble_edl(&segs, black.as_deref())?))
}

/// Cache dir for black gap files: `$XDG_CACHE_HOME/nle/black` or `~/.cache/nle/black`.
pub fn black_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("nle").join("black")
}

/// Ensure a silent black clip of `BLACK_FILE_SECS` exists for this
/// geometry and return its path. EDL entries reference it in chunks of at
/// most that length, so one file per WxH+fps serves all projects on this
/// machine.
pub fn ensure_black_file(width: u32, height: u32, fps: &Seconds) -> Result<PathBuf, std::io::Error> {
    let dir = black_cache_dir();
    fs::create_dir_all(&dir)?;
    let name = format!("{width}x{height}_{}_{}.mp4", fps.numer(), fps.denom());
    let path = dir.join(name.clone());
    if path.is_file() {
        return Ok(path);
    }
    // Write to a temp name (still ending in .mp4 so ffmpeg can infer the
    // muxer), then rename — a crash mid-generate can't leave a half-written
    // file that later loads "successfully".
    let tmp = dir.join(format!("{name}.tmp-{}.mp4", std::process::id()));
    let fps_s = format!("{}/{}", fps.numer(), fps.denom());
    let len_s = BLACK_FILE_SECS.to_string();
    let status = Command::new("ffmpeg")
        .args([
            "-v", "error", "-y",
            "-f", "lavfi", "-i", &format!("color=c=black:s={width}x{height}:r={fps_s}"),
            "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo",
            "-t", &len_s,
            "-c:v", "libx264", "-preset", "ultrafast", "-crf", "40",
            "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "32k",
        ])
        .arg("--")
        .arg(&tmp)
        .status()?;
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        return Err(std::io::Error::other("ffmpeg failed to generate the black gap file"));
    }
    fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::media::{RawFormat, RawProbe, RawStream};
    use nle_core::timeline::{Clip as TimelineClip, Gap as TimelineGap, Segment as TimelineSegment};
    use nle_core::VideoSettings;

    fn seg_clip(id: u32, ain: i64, aout: i64) -> TimelineSegment {
        TimelineSegment::Clip(TimelineClip {
            id,
            asset: 1,
            asset_in: Seconds::from_integer(ain),
            asset_out: Seconds::from_integer(aout),
        })
    }

    fn project_with_gap() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings { width: 320, height: 240 });
        let (_a, _) = p.add_asset("/media/red.mp4".into(), probe_report("5"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            seg_clip(1, 1, 3),   // [0..2) src 1..3
            TimelineSegment::Gap(TimelineGap { duration: Seconds::from_integer(3) }),
            seg_clip(2, 0, 3),   // [5..8) src 0..3
        ];
        p
    }

    fn probe_report(duration: &str) -> nle_core::RawProbe {
        RawProbe {
            streams: vec![RawStream {
                index: 0,
                codec_type: Some("video".into()),
                codec_name: Some("h264".into()),
                width: Some(320),
                height: Some(240),
                r_frame_rate: Some("30/1".into()),
                ..Default::default()
            }],
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn segments_flatten_clips_and_gaps_in_order() {
        let p = project_with_gap();
        let segs = segments(&p).unwrap();
        assert_eq!(
            segs,
            vec![
                Segment::Clip {
                    path: "/media/red.mp4".into(),
                    start: Seconds::from_integer(1),
                    length: Seconds::from_integer(2),
                },
                Segment::Gap { length: Seconds::from_integer(3) },
                Segment::Clip {
                    path: "/media/red.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(3),
                },
            ]
        );
    }

    #[test]
    fn quote_always_uses_length_prefix() {
        assert_eq!(edl_quote("plain.mp4"), "%9%plain.mp4");
        assert_eq!(edl_quote("a,b;c"), "%5%a,b;c");
    }

    #[test]
    fn assemble_uses_length_not_end() {
        let segs = vec![
            Segment::Clip {
                path: "/a.mp4".into(),
                start: Seconds::from_integer(1),
                length: Seconds::from_integer(2),
            },
            Segment::Gap { length: Seconds::new(3, 2) },
        ];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        // third field is LENGTH: red src [1..3), then 1.5s of black
        assert_eq!(edl, "%6%/a.mp4,1,2;%16%/cache/black.mp4,0,1.5");
    }

    #[test]
    fn gap_without_black_file_is_an_error() {
        let segs = vec![Segment::Gap { length: Seconds::from_integer(1) }];
        assert!(matches!(assemble_edl(&segs, None), Err(PreviewError::NoSize)));
    }

    #[test]
    fn long_gaps_split_into_black_file_chunks() {
        let segs = vec![Segment::Gap { length: Seconds::from_integer(25) }];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        assert_eq!(
            edl,
            "%16%/cache/black.mp4,0,10;%16%/cache/black.mp4,0,10;%16%/cache/black.mp4,0,5"
        );
        // exactly one black file long stays a single entry
        let segs = vec![Segment::Gap { length: Seconds::from_integer(BLACK_FILE_SECS) }];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        assert_eq!(edl, "%16%/cache/black.mp4,0,10");
    }

    #[test]
    fn edl_starts_with_prefix() {
        let p = project_with_gap();
        let segs = segments(&p).unwrap();
        let edl = assemble_edl(&segs, Some(Path::new("/b.mp4"))).unwrap();
        assert!(edl.starts_with("%14%/media/red.mp4,1,2;"), "{edl}");
    }
}
