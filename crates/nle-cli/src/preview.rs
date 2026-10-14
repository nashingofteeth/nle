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

use nle_core::timeline::{Segment as TimelineSegment, Track};
use nle_core::{Project, Seconds, format_seconds};
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
    #[error(transparent)]
    Proxy(#[from] crate::proxy::ProxyError),
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

/// Flatten track `ti` into ordered preview segments. A track shorter than
/// the timeline is padded with a trailing gap, so the monitor spans the
/// whole timeline instead of stopping at the track's own end.
pub fn segments(project: &Project, ti: usize) -> Result<Vec<Segment>, PreviewError> {
    let track = project
        .timeline
        .track(ti)
        .ok_or(PreviewError::NoVideoTrack)?;
    let total = project.timeline.end();
    if track.segments.is_empty() {
        // An empty track is all gap — preview the whole timeline as black
        // (unless the timeline itself is empty).
        return if total > Seconds::from_integer(0) {
            Ok(vec![Segment::Gap { length: total }])
        } else {
            Err(PreviewError::Empty)
        };
    }

    let mut segs = Vec::new();
    for seg in &track.segments {
        // Effects and transitions can't be expressed by `edl://`: play the
        // rendered proxy instead of the raw source.
        if crate::proxy::needs_proxy(seg) {
            let p = crate::proxy::proxy_for(project, seg)?;
            segs.push(Segment::Clip {
                path: p,
                start: Seconds::from_integer(0),
                length: seg.duration(),
            });
            continue;
        }
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
            TimelineSegment::Gap(g) => segs.push(Segment::Gap { length: g.duration }),
            // `needs_proxy` covers transitions; keep a safe fallback.
            TimelineSegment::Transition(tr) => segs.push(Segment::Gap { length: tr.dur }),
        }
    }
    let tail = total - track.end();
    if tail > Seconds::from_integer(0) {
        segs.push(Segment::Gap { length: tail });
    }
    Ok(segs)
}

/// A notice for the monitor when any clip plays at a non-1× speed: mpv's
/// EDL has no speed concept, so the monitor plays those clips at 1× and
/// reads the wrong source span. A clip with effects is retimed by its proxy
/// (see [`composite_segments`]) and is skipped; editing and export are
/// unaffected.
pub fn speed_notice(project: &Project) -> Option<String> {
    let n = project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.segments.iter())
        .filter(|s| {
            matches!(s, TimelineSegment::Clip(c)
                if c.speed != Seconds::from_integer(1) && c.effects.is_empty())
        })
        .count();
    (n > 0).then(|| format!("monitor is 1x: {n} speed clip(s) play at normal speed"))
}

/// Everything the monitor can't show faithfully, joined. Effects and
/// transitions are rendered to proxies (see [`composite_segments`]), so the
/// only remaining gap is un-retimed speed.
pub fn monitor_notices(project: &Project) -> Option<String> {
    let notices: Vec<String> = [speed_notice(project)].into_iter().flatten().collect();
    (!notices.is_empty()).then(|| notices.join(" · "))
}

/// One interval of the composite: the topmost track's material covering it,
/// at `offset` into that segment for `len`, or a gap where every track is a
/// gap. Pure — the layering decision is settled here; resolving an asset path
/// or rendering a proxy happens in [`composite_segments`].
#[derive(Debug)]
enum Window<'a> {
    Segment(&'a TimelineSegment, Seconds, Seconds),
    Gap(Seconds),
}

/// The composite plan: the union of every unmuted video track's cut points,
/// with the topmost segment picked per interval. A clip **or a transition**
/// wins over the layers below it — a transition is rendered material (its
/// proxy), not a hole; a gap falls through.
fn composite_windows(project: &Project) -> Result<Vec<Window<'_>>, PreviewError> {
    let total = project.timeline.end();
    if total <= Seconds::from_integer(0) {
        return Err(PreviewError::Empty);
    }
    // Stacking order is bottom→top, so the last video track is on top.
    let video: Vec<&Track> = project
        .timeline
        .video_tracks()
        .filter(|t| !t.muted)
        .collect();
    if video.is_empty() {
        return Err(PreviewError::NoVideoTrack);
    }

    // Union of every video track's segment boundaries.
    let mut cuts = vec![Seconds::from_integer(0), total];
    for t in &video {
        let mut acc = Seconds::from_integer(0);
        for seg in &t.segments {
            cuts.push(acc);
            acc += seg.duration();
            cuts.push(acc);
        }
    }
    cuts.sort();
    cuts.dedup();

    let mut windows = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b <= a {
            continue;
        }
        // The topmost track with material (a clip or a transition) covering
        // `[a, b)`; a gap falls through to the layer below.
        let picked = video.iter().rev().find_map(|t| {
            let si = t.index_at(a)?;
            match &t.segments[si] {
                TimelineSegment::Clip(_) | TimelineSegment::Transition(_) => {
                    Some((&t.segments[si], t.start_of(si)))
                }
                TimelineSegment::Gap(_) => None,
            }
        });
        windows.push(match picked {
            Some((seg, seg_start)) => Window::Segment(seg, a - seg_start, b - a),
            None => Window::Gap(b - a),
        });
    }
    Ok(windows)
}

/// Flatten the video tracks into one preview sequence, topmost first: at
/// each instant the highest unmuted video track with material wins, and only
/// where every track is a gap does the preview show black. This is what the
/// monitor plays, so a gap in an upper layer reveals the layer below rather
/// than stopping playback.
///
/// A segment with effects, or a transition, can't be expressed by `edl://`:
/// that window plays the rendered proxy (the same one `nle edl` uses),
/// offset into it like any source.
pub fn composite_segments(project: &Project) -> Result<Vec<Segment>, PreviewError> {
    let mut segs: Vec<Segment> = Vec::new();
    for w in composite_windows(project)? {
        match w {
            Window::Gap(len) => push_gap(&mut segs, len),
            Window::Segment(seg, offset, len) => {
                if crate::proxy::needs_proxy(seg) {
                    let path = crate::proxy::proxy_for(project, seg)?;
                    segs.push(Segment::Clip {
                        path,
                        start: offset,
                        length: len,
                    });
                } else if let TimelineSegment::Clip(c) = seg {
                    let asset = project
                        .asset(c.asset)
                        .ok_or(PreviewError::MissingAsset(c.asset))?;
                    segs.push(Segment::Clip {
                        path: asset.path.clone(),
                        // `offset` is inside the clip: offset into its source.
                        start: c.asset_in + offset,
                        length: len,
                    });
                }
                // Nothing else reaches here: only a clip gets through
                // without a proxy, since every transition needs one.
            }
        }
    }
    Ok(segs)
}

/// Append a gap, fusing it with a preceding gap so a black span is one
/// entry.
fn push_gap(segs: &mut Vec<Segment>, length: Seconds) {
    if let Some(Segment::Gap { length: last }) = segs.last_mut() {
        *last += length;
    } else {
        segs.push(Segment::Gap { length });
    }
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
            Segment::Clip {
                path,
                start,
                length,
            } => entries.push(format!(
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

/// A single-source preview for extend mode: mpv position 0 corresponds
/// to `origin` in timeline coordinates, so seeks translate by
/// subtracting it (`mpv = playhead - origin`). Clip previews show the
/// armed asset's real frames across the whole workable range; black
/// previews (gap work, which has no material) show blank.
pub struct ExtendPreview {
    pub url: String,
    /// Timeline position of mpv position 0.
    pub origin: f64,
}

/// Material preview for a clip-armed edge: the asset trimmed to the
/// source span covering the workable range. `origin` is the timeline
/// position mpv 0 maps to; `src_start`/`length` are asset-relative.
pub fn build_clip_preview(
    path: &Path,
    src_start: Seconds,
    length: Seconds,
    origin: f64,
) -> ExtendPreview {
    ExtendPreview {
        url: format!(
            "edl://{},{}",
            edl_quote(&path.display().to_string()),
            fmt_entry(src_start, length)
        ),
        origin,
    }
}

/// Blank preview for gap-armed edges: the cached black file, repeated
/// in `BLACK_FILE_SECS` chunks to cover `length` (mpv can't address
/// past a segment's source length, so — like timeline gaps — long spans
/// become multiple entries). No loop flag needed: seeks stay valid
/// across the whole span.
pub fn build_black_preview(
    project: &Project,
    origin: f64,
    length: Seconds,
) -> Result<ExtendPreview, PreviewError> {
    let size = project.video.as_ref().ok_or(PreviewError::NoSize)?;
    let fps = project.fps.ok_or(PreviewError::NoFps)?;
    let black = ensure_black_file(size.width, size.height, &fps)?;
    let url = format!(
        "edl://{}",
        assemble_edl(&[Segment::Gap { length }], Some(&black))?
    );
    Ok(ExtendPreview { url, origin })
}

/// How far past the timeline end a gap-armed preview reaches. Repeating
/// the 10s black file costs ~40 bytes per chunk in the URL, so an hour
/// of navigable black is cheap; the terminal row projects beyond it
/// anyway (mpv simply holds black past the last chunk).
pub const EXTEND_PREVIEW_SECS: i64 = 3600;

/// Build the full `edl://` URL for track `ti`, generating the black gap
/// file if the timeline has gaps. Used by the CLI (`nle edl --track N`); the
/// monitor plays the composite (see `build_composite_edl`).
pub fn build_edl(project: &Project, ti: usize) -> Result<String, PreviewError> {
    edl_from_segments(project, &segments(project, ti)?)
}

/// Build the `edl://` URL for the whole timeline — the composite of every
/// unmuted video track, topmost clip winning. This is what the monitor
/// plays, so a gap falls through to the layer below.
pub fn build_composite_edl(project: &Project) -> Result<String, PreviewError> {
    edl_from_segments(project, &composite_segments(project)?)
}

/// Render preview segments to an `edl://` URL, generating the cached black
/// filler file when a gap is present.
fn edl_from_segments(project: &Project, segs: &[Segment]) -> Result<String, PreviewError> {
    let has_gap = segs.iter().any(|s| matches!(s, Segment::Gap { .. }));
    let black = if has_gap {
        let size = project.video.as_ref().ok_or(PreviewError::NoSize)?;
        let fps = project.fps.ok_or(PreviewError::NoFps)?;
        Some(ensure_black_file(size.width, size.height, &fps)?)
    } else {
        None
    };
    Ok(format!("edl://{}", assemble_edl(segs, black.as_deref())?))
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
pub fn ensure_black_file(
    width: u32,
    height: u32,
    fps: &Seconds,
) -> Result<PathBuf, std::io::Error> {
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
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=black:s={width}x{height}:r={fps_s}"),
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-t",
            &len_s,
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "40",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
        ])
        .arg("--")
        .arg(&tmp)
        .status()?;
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        return Err(std::io::Error::other(
            "ffmpeg failed to generate the black gap file",
        ));
    }
    fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::VideoSettings;
    use nle_core::media::{RawFormat, RawProbe, RawStream};
    use nle_core::timeline::{
        Clip as TimelineClip, Effect, Gap as TimelineGap, Segment as TimelineSegment, Transition,
        TransitionKind, TransitionSource,
    };

    fn seg_clip(id: u32, ain: i64, aout: i64) -> TimelineSegment {
        TimelineSegment::Clip(TimelineClip {
            id,
            asset: 1,
            asset_in: Seconds::from_integer(ain),
            asset_out: Seconds::from_integer(aout),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })
    }

    #[test]
    fn speed_notice_flags_non_unit_speed() {
        let mut p = project_with_gap();
        assert!(speed_notice(&p).is_none());
        if let TimelineSegment::Clip(c) = &mut p.timeline.video_track_mut().unwrap().segments[0] {
            c.speed = Seconds::new(1, 2);
        }
        let notice = speed_notice(&p).expect("notice");
        assert!(notice.contains("1 speed clip"), "{notice}");
    }

    #[test]
    fn a_transition_is_no_longer_a_monitor_caveat() {
        // A transition is no longer a caveat: the monitor plays its proxy.
        let mut p = project_with_gap();
        p.timeline.video_track_mut().unwrap().segments.insert(
            1,
            TimelineSegment::Transition(Transition {
                id: 9,
                dur: Seconds::from_integer(1),
                kind: TransitionKind::Dissolve,
                a: TransitionSource::Black,
                b: TransitionSource::Black,
            }),
        );
        assert!(monitor_notices(&p).is_none(), "no caveat to report");
    }

    // The composite picks a transition over the layer below (it is rendered
    // material) and routes it through the proxy cache.
    #[test]
    fn composite_plan_treats_a_transition_as_material_to_proxy() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (low, _) = p.add_asset("/media/low.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![seg_clip(1, 0, 4)];
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![TimelineSegment::Transition(Transition {
            id: 9,
            dur: Seconds::from_integer(1),
            kind: TransitionKind::Dissolve,
            a: TransitionSource::Clip {
                asset: low,
                src_in: Seconds::from_integer(0),
                src_out: Seconds::from_integer(1),
            },
            b: TransitionSource::Black,
        })];

        let windows = composite_windows(&p).unwrap();
        assert_eq!(windows.len(), 2, "the transition's cuts split the low clip");
        match &windows[0] {
            Window::Segment(seg, offset, len) => {
                assert!(matches!(seg, TimelineSegment::Transition(_)));
                assert_eq!(*offset, Seconds::from_integer(0));
                assert_eq!(*len, Seconds::from_integer(1));
                assert!(
                    crate::proxy::needs_proxy(seg),
                    "a transition is rendered to a proxy"
                );
            }
            other => panic!("expected the transition to win, got {other:?}"),
        }
        // Below the top track's end, the low clip shows through.
        match &windows[1] {
            Window::Segment(TimelineSegment::Clip(c), offset, len) => {
                assert_eq!((c.asset, *offset, *len), (low, 1.into(), 3.into()));
            }
            other => panic!("expected the low clip, got {other:?}"),
        }
    }

    #[test]
    fn composite_plan_proxies_a_clip_with_effects() {
        let mut p = composite_project();
        if let TimelineSegment::Clip(c) = &mut p.timeline.tracks[1].segments[1] {
            c.effects = vec![Effect::Scale { w: 2, h: 2 }];
        }
        let windows = composite_windows(&p).unwrap();
        match &windows[1] {
            Window::Segment(seg, ..) => assert!(crate::proxy::needs_proxy(seg), "{seg:?}"),
            other => panic!("expected the top clip window, got {other:?}"),
        }
    }

    fn project_with_gap() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        p.video = Some(VideoSettings {
            width: 320,
            height: 240,
        });
        let (_a, _) = p.add_asset("/media/red.mp4".into(), probe_report("5"));
        p.timeline.video_track_mut().unwrap().segments = vec![
            seg_clip(1, 1, 3), // [0..2) src 1..3
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(3),
            }),
            seg_clip(2, 0, 3), // [5..8) src 0..3
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
        let segs = segments(&p, 0).unwrap();
        assert_eq!(
            segs,
            vec![
                Segment::Clip {
                    path: "/media/red.mp4".into(),
                    start: Seconds::from_integer(1),
                    length: Seconds::from_integer(2),
                },
                Segment::Gap {
                    length: Seconds::from_integer(3)
                },
                Segment::Clip {
                    path: "/media/red.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(3),
                },
            ]
        );
    }

    #[test]
    fn shorter_track_previews_its_tail_as_a_gap() {
        let mut p = project_with_gap(); // track 0 ends at 8 s
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![seg_clip(9, 0, 2)]; // 2 s
        let segs = segments(&p, ti).unwrap();
        assert_eq!(
            segs,
            vec![
                Segment::Clip {
                    path: "/media/red.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(2),
                },
                // Padded to the 8 s timeline so playback doesn't stop at 2 s.
                Segment::Gap {
                    length: Seconds::from_integer(6)
                },
            ]
        );
    }

    #[test]
    fn empty_track_previews_the_whole_timeline_as_black() {
        let mut p = project_with_gap(); // timeline ends at 8 s
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        let segs = segments(&p, ti).unwrap();
        assert_eq!(
            segs,
            vec![Segment::Gap {
                length: Seconds::from_integer(8)
            }]
        );
    }

    // Two video tracks: low holds a 4 s clip; high is gap / 1 s clip / gap.
    fn composite_project() -> Project {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (low, _) = p.add_asset("/media/low.mp4".into(), probe_report("10"));
        let (high, _) = p.add_asset("/media/high.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![TimelineSegment::Clip(TimelineClip {
            id: 1,
            asset: low,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(4),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(1),
            }),
            TimelineSegment::Clip(TimelineClip {
                id: 2,
                asset: high,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(1),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
            TimelineSegment::Gap(TimelineGap {
                duration: Seconds::from_integer(2),
            }),
        ];
        p
    }

    #[test]
    fn composite_picks_the_topmost_clip_and_falls_through_gaps() {
        let p = composite_project();
        let segs = composite_segments(&p).unwrap();
        assert_eq!(
            segs,
            vec![
                // [0,1) top is a gap → the low clip shows, source 0..1.
                Segment::Clip {
                    path: "/media/low.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(1),
                },
                // [1,2) top clip wins.
                Segment::Clip {
                    path: "/media/high.mp4".into(),
                    start: Seconds::from_integer(0),
                    length: Seconds::from_integer(1),
                },
                // [2,4) top is a gap again → low clip, source 2..4.
                Segment::Clip {
                    path: "/media/low.mp4".into(),
                    start: Seconds::from_integer(2),
                    length: Seconds::from_integer(2),
                },
            ]
        );
    }

    #[test]
    fn composite_top_clip_wins_over_a_lower_clip() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let _ = p.add_asset("/media/low.mp4".into(), probe_report("10"));
        let (high, _) = p.add_asset("/media/high.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![seg_clip(1, 0, 2)]; // asset 1 (low)
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![TimelineSegment::Clip(TimelineClip {
            id: 2,
            asset: high,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        let segs = composite_segments(&p).unwrap();
        assert_eq!(
            segs,
            vec![Segment::Clip {
                path: "/media/high.mp4".into(),
                start: Seconds::from_integer(0),
                length: Seconds::from_integer(2),
            }]
        );
    }

    #[test]
    fn composite_is_black_only_when_every_track_is_a_gap() {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let _ = p.add_asset("/media/low.mp4".into(), probe_report("10"));
        p.timeline.tracks[0].segments = vec![TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(3),
        })];
        let ti = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[ti].segments = vec![TimelineSegment::Gap(TimelineGap {
            duration: Seconds::from_integer(2),
        })];
        // Timeline end is the longer track (3 s); both gaps read black.
        let segs = composite_segments(&p).unwrap();
        assert_eq!(
            segs,
            vec![Segment::Gap {
                length: Seconds::from_integer(3)
            }]
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
            Segment::Gap {
                length: Seconds::new(3, 2),
            },
        ];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        // third field is LENGTH: red src [1..3), then 1.5s of black
        assert_eq!(edl, "%6%/a.mp4,1,2;%16%/cache/black.mp4,0,1.5");
    }

    #[test]
    fn gap_without_black_file_is_an_error() {
        let segs = vec![Segment::Gap {
            length: Seconds::from_integer(1),
        }];
        assert!(matches!(
            assemble_edl(&segs, None),
            Err(PreviewError::NoSize)
        ));
    }

    #[test]
    fn long_gaps_split_into_black_file_chunks() {
        let segs = vec![Segment::Gap {
            length: Seconds::from_integer(25),
        }];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        assert_eq!(
            edl,
            "%16%/cache/black.mp4,0,10;%16%/cache/black.mp4,0,10;%16%/cache/black.mp4,0,5"
        );
        // exactly one black file long stays a single entry
        let segs = vec![Segment::Gap {
            length: Seconds::from_integer(BLACK_FILE_SECS),
        }];
        let edl = assemble_edl(&segs, Some(Path::new("/cache/black.mp4"))).unwrap();
        assert_eq!(edl, "%16%/cache/black.mp4,0,10");
    }

    #[test]
    fn extend_clip_preview_maps_origin_to_zero() {
        let pv = build_clip_preview(
            Path::new("/media/red.mp4"),
            Seconds::from_integer(1),
            Seconds::from_integer(4),
            2.0,
        );
        // asset trimmed to the material span; mpv 0 == timeline 2.0
        assert_eq!(pv.url, "edl://%14%/media/red.mp4,1,4");
        assert_eq!(pv.origin, 2.0);
    }

    #[test]
    fn extend_black_preview_needs_geometry() {
        // geometry errors surface without touching the cache
        let mut bare = Project::new();
        assert!(matches!(
            build_black_preview(&bare, 5.0, Seconds::from_integer(25)),
            Err(PreviewError::NoSize)
        ));
        bare.video = Some(VideoSettings {
            width: 320,
            height: 240,
        });
        assert!(matches!(
            build_black_preview(&bare, 5.0, Seconds::from_integer(25)),
            Err(PreviewError::NoFps)
        ));
    }

    #[test]
    fn edl_starts_with_prefix() {
        let p = project_with_gap();
        let segs = segments(&p, 0).unwrap();
        let edl = assemble_edl(&segs, Some(Path::new("/b.mp4"))).unwrap();
        assert!(edl.starts_with("%14%/media/red.mp4,1,2;"), "{edl}");
    }
}
