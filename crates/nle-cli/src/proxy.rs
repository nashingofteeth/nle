//! Proxy render cache for the monitor.
//!
//! `edl://` can neither apply effects nor blend a transition, so those
//! segments are rendered once into `~/.cache/nle/proxy/<hash>.mp4` and the
//! monitor plays the proxy instead of the raw source. The hash covers
//! everything the render reads, so a change (or a replaced file) re-renders.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use nle_core::Project;
use nle_core::Seconds;
use nle_core::time::secs_f64;
use nle_core::timeline::{Clip, Effect, Gap, Segment, Track, TrackKind};
use nle_ffmpeg::graph::ExportSpec;
use nle_ffmpeg::{Encoder, ExportError, export_quiet_progress};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("no output size or frame rate set — add a clip first")]
    NoSettings,
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Does this segment need a proxy (something `edl://` can't express)?
///
/// An EDL entry plays a file the way mpv finds it: no filter, no rate, no
/// scale. So a proxy is needed for
/// - an effect, or a non-1× retime (the raw source would play unfiltered);
/// - a transition (it is a blend of two sides);
/// - a clip whose picture **size** differs from the sequence's. The export
///   scales every clip to the sequence geometry (`scale=w:h,setsar=1`), while
///   mpv re-frames a raw file to *its* aspect (bars) — and the transition
///   beside such a clip *is* scaled, so the framing would jump at the seam.
pub fn needs_proxy(project: &Project, seg: &Segment) -> bool {
    match seg {
        Segment::Clip(c) => {
            !c.effects.is_empty()
                || c.speed != Seconds::from_integer(1)
                || off_geometry(project, c.asset)
        }
        Segment::Transition(_) => true,
        Segment::Gap(_) => false,
    }
}

/// Whether the asset's picture size differs from the sequence's, so `edl://`
/// would preview it at another size/aspect than the export renders.
fn off_geometry(project: &Project, asset: u32) -> bool {
    let Some(v) = &project.video else {
        return false;
    };
    match project.asset(asset).and_then(|a| a.video_size()) {
        Some((w, h)) => (w, h) != (v.width, v.height),
        None => false,
    }
}

/// `~/.cache/nle/proxy` (or `$XDG_CACHE_HOME/nle/proxy`).
pub fn proxy_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("nle").join("proxy")
}

/// Everything the render depends on: the segment (effects, transition,
/// source spans, speed), the output geometry and frame rate, and the media it
/// reads (path + mtime, so replacing a file invalidates).
fn proxy_hash(project: &Project, seg: &Segment, w: u32, h: u32) -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Ok(s) = serde_json::to_string(seg) {
        s.hash(&mut hasher);
    }
    hash_lut_files(seg, &mut hasher);
    w.hash(&mut hasher);
    h.hash(&mut hasher);
    // The render bakes the project rate in (`fps=…`), so it belongs in the
    // key too: the same segment in a 24 fps and a 30 fps project must not
    // share an entry.
    project.fps.hash(&mut hasher);
    let mut assets: Vec<u32> = Vec::new();
    match seg {
        Segment::Clip(c) => assets.push(c.asset),
        Segment::Transition(t) => {
            if let Some(a) = t.a.asset() {
                assets.push(a);
            }
            if let Some(b) = t.b.asset() {
                assets.push(b);
            }
        }
        Segment::Gap(_) => {}
    }
    for id in assets {
        id.hash(&mut hasher);
        if let Some(a) = project.asset(id) {
            a.path.hash(&mut hasher);
            if let Ok(m) = std::fs::metadata(&a.path)
                && let Ok(t) = m.modified()
            {
                t.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// A cached entry is a hit only when it is a *complete* file: a zero-length
/// one (a disk that filled, an interrupted copy) would otherwise be served
/// forever — the hash never changes, so nothing would ever re-render it.
fn complete(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
}

/// Hash the LUT files a segment's effects read (path + mtime). The segment JSON
/// carries only the LUT's *path*, so editing a `.cube` in place would otherwise
/// serve the old grade from the cache while export bakes the new one.
fn hash_lut_files(seg: &Segment, hasher: &mut DefaultHasher) {
    let Segment::Clip(c) = seg else { return };
    for e in &c.effects {
        if let Effect::Lut { path } = e {
            path.hash(hasher);
            if let Ok(m) = std::fs::metadata(path)
                && let Ok(t) = m.modified()
            {
                t.hash(hasher);
            }
        }
    }
}

/// Render `project` into the cache under `hash` (if it isn't there yet) and
/// return the path. The render goes to a temp name (still ending in `.mp4` so
/// ffmpeg can infer the muxer) and is renamed into place, so a crash mid-render
/// can't leave a half-written file that a later run would serve.
fn render_cached(
    project: &Project,
    hash: u64,
    duration: f64,
    progress: &mut dyn FnMut(f64),
) -> Result<PathBuf, ProxyError> {
    let dir = proxy_cache_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{hash:x}.mp4"));
    if complete(&path) {
        return Ok(path);
    }
    let tmp = dir.join(format!("{hash:x}.tmp-{}.mp4", std::process::id()));
    let spec = ExportSpec {
        output: tmp.clone(),
        encoder: Encoder::Cpu,
        crf: 18,
        preset: "ultrafast".into(),
        bitrate: "8M".into(),
    };
    // Quiet: this runs from the monitor's reload, so ffmpeg's chatter would
    // paint over the TUI. A failure still carries the reason. The render's
    // `-progress` stream becomes the fraction of `duration` this proxy has
    // finished.
    let mut report = |seconds: f64| {
        if duration > 0.0 {
            progress((seconds / duration).clamp(0.0, 1.0));
        }
    };
    if let Err(e) = export_quiet_progress(project, &spec, &mut report) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// The cached proxy for `seg`, rendering it on demand. The segment is put
/// on a track of its own and handed to the export graph compiler — which
/// already applies effects and blends transitions — so the proxy is the
/// one source of truth for "what this segment looks like".
pub fn proxy_for(project: &Project, seg: &Segment) -> Result<PathBuf, ProxyError> {
    proxy_for_with(project, seg, &mut |_| {})
}

/// [`proxy_for`] reporting this render's fraction (0–1).
pub fn proxy_for_with(
    project: &Project,
    seg: &Segment,
    progress: &mut dyn FnMut(f64),
) -> Result<PathBuf, ProxyError> {
    let (w, h) = match &project.video {
        Some(v) => (v.width, v.height),
        None => return Err(ProxyError::NoSettings),
    };
    if project.fps.is_none() {
        return Err(ProxyError::NoSettings);
    }
    let hash = proxy_hash(project, seg, w, h);
    let duration = secs_f64(&seg.duration());

    let mut p = project.clone();
    p.timeline.tracks = vec![Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![seg.clone()],
    }];
    render_cached(&p, hash, duration, progress)
}

/// The cached composite of `[start, end)` — the span rendered the way the
/// export renders it: every unmuted track's slice, video `overlay`ed and audio
/// `amix`ed. The monitor plays this where more than one track contributes
/// audio, which `edl://` (one source per span) cannot express. Reports this
/// render's fraction (0–1).
pub fn composite_proxy_with(
    project: &Project,
    start: Seconds,
    end: Seconds,
    progress: &mut dyn FnMut(f64),
) -> Result<PathBuf, ProxyError> {
    let (w, h) = match &project.video {
        Some(v) => (v.width, v.height),
        None => return Err(ProxyError::NoSettings),
    };
    if project.fps.is_none() {
        return Err(ProxyError::NoSettings);
    }
    let tracks = slice_tracks(project, start, end);
    let hash = span_hash(project, &tracks, w, h);
    let duration = secs_f64(&(end - start));
    let mut p = project.clone();
    p.timeline.tracks = tracks;
    render_cached(&p, hash, duration, progress)
}

/// Every unmuted track's segments covering `[start, end)`, trimmed at the edges
/// and padded to the span. A span's bounds are window boundaries, so no
/// segment is cut *inside* — but a boundary from one track can fall inside
/// another track's clip, so edge segments are trimmed to fit. Transitions are
/// never cut: `composite_spans` widens a span to contain any it straddles.
fn slice_tracks(project: &Project, start: Seconds, end: Seconds) -> Vec<Track> {
    let span = end - start;
    let mut sliced: Vec<Track> = project
        .timeline
        .tracks
        .iter()
        .filter(|t| !t.muted)
        .map(|track| {
            let mut segments: Vec<Segment> = Vec::new();
            let mut filled = Seconds::from_integer(0);
            let mut pos = Seconds::from_integer(0);
            for seg in &track.segments {
                let seg_end = pos + seg.duration();
                let (lo, hi) = (pos.max(start), seg_end.min(end));
                if hi > lo {
                    let at = lo - start;
                    if at > filled {
                        segments.push(Segment::Gap(Gap {
                            duration: at - filled,
                        }));
                        filled = at;
                    }
                    let piece = trim_segment(seg, lo - pos, hi - lo);
                    filled += piece.duration();
                    segments.push(piece);
                }
                pos = seg_end;
            }
            // The export pads a short track out to the timeline length; do the
            // same so the slice spans exactly `[start, end)` (a track with
            // nothing in the span becomes one gap).
            if filled < span {
                segments.push(Segment::Gap(Gap {
                    duration: span - filled,
                }));
            }
            Track {
                kind: track.kind,
                muted: false,
                segments,
            }
        })
        .collect();
    // A fully-muted video timeline still renders: the export needs a video
    // layer, so give it one of black (a gap) rather than failing.
    if !sliced.iter().any(|t| t.kind == TrackKind::Video) {
        sliced.push(Track {
            kind: TrackKind::Video,
            muted: false,
            segments: vec![Segment::Gap(Gap { duration: span })],
        });
    }
    sliced
}

/// The `[offset, offset + len)` part of `seg`, in span-relative time. Clips and
/// gaps only: a span is widened to contain a transition whole, because a
/// partial crossfade isn't expressible.
fn trim_segment(seg: &Segment, offset: Seconds, len: Seconds) -> Segment {
    match seg {
        Segment::Gap(_) => Segment::Gap(Gap { duration: len }),
        Segment::Clip(c) => Segment::Clip(Clip {
            asset_in: c.source_at(offset),
            asset_out: c.source_at(offset + len),
            ..c.clone()
        }),
        Segment::Transition(t) => {
            // `composite_spans` widens a span to contain any transition whole,
            // so it arrives uncut (`offset == 0`, `len == dur`); keep it as-is
            // and let the export graph render the crossfade.
            debug_assert_eq!(offset, Seconds::from_integer(0));
            debug_assert_eq!(len, t.dur);
            seg.clone()
        }
    }
}

/// The cache key for a composite span: every sliced track (kinds, effects,
/// transitions, speeds), the output geometry and rate, and each asset the
/// slice reads (path + mtime, so replacing a file invalidates).
fn span_hash(project: &Project, tracks: &[Track], w: u32, h: u32) -> u64 {
    let mut hasher = DefaultHasher::new();
    for track in tracks {
        if let Ok(json) = serde_json::to_string(track) {
            json.hash(&mut hasher);
        }
    }
    w.hash(&mut hasher);
    h.hash(&mut hasher);
    project.fps.hash(&mut hasher);
    for track in tracks {
        for seg in &track.segments {
            hash_lut_files(seg, &mut hasher);
            let mut assets: Vec<u32> = Vec::new();
            match seg {
                Segment::Clip(c) => assets.push(c.asset),
                Segment::Transition(t) => {
                    assets.extend(t.a.asset());
                    assets.extend(t.b.asset());
                }
                Segment::Gap(_) => {}
            }
            for id in assets {
                id.hash(&mut hasher);
                if let Some(a) = project.asset(id) {
                    a.path.hash(&mut hasher);
                    if let Ok(m) = std::fs::metadata(&a.path)
                        && let Ok(t) = m.modified()
                    {
                        t.hash(&mut hasher);
                    }
                }
            }
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::timeline::{Clip, Effect, Gap, Transition, TransitionKind, TransitionSource};

    fn clip() -> Segment {
        Segment::Clip(Clip::new(
            1,
            1,
            Seconds::from_integer(0),
            Seconds::from_integer(2),
        ))
    }

    fn probe_size(w: u32, h: u32) -> nle_core::RawProbe {
        nle_core::RawProbe {
            streams: vec![nle_core::RawStream {
                index: 0,
                codec_type: Some("video".into()),
                width: Some(w),
                height: Some(h),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn needs_proxy_only_for_what_edl_cant_express() {
        let p = Project::new();
        // A plain clip and a gap play straight from source.
        assert!(!needs_proxy(&p, &clip()));
        assert!(!needs_proxy(
            &p,
            &Segment::Gap(Gap {
                duration: Seconds::from_integer(1)
            })
        ));
        // An effect clip and a transition need rendering.
        let mut c = clip();
        if let Segment::Clip(cl) = &mut c {
            cl.effects = vec![Effect::Scale { w: 2, h: 2 }];
        }
        assert!(needs_proxy(&p, &c), "effects are rendered");
        // …and so does a retimed clip: an EDL entry plays its source at 1×.
        let mut s = clip();
        if let Segment::Clip(cl) = &mut s {
            cl.speed = Seconds::new(1, 2);
        }
        assert!(needs_proxy(&p, &s), "speed is rendered");
        assert!(needs_proxy(
            &p,
            &Segment::Transition(Transition {
                id: 1,
                dur: Seconds::from_integer(1),
                kind: TransitionKind::Dissolve,
                a: TransitionSource::Black,
                b: TransitionSource::Black,
            })
        ));
    }

    /// A clip whose picture size differs from the sequence's is scaled by the
    /// export but re-framed (bars) by mpv through `edl://`, so it needs a
    /// proxy too — otherwise the framing jumps at the transition beside it.
    #[test]
    fn a_clip_off_the_sequence_geometry_needs_a_proxy() {
        let mut p = Project::new();
        p.video = Some(nle_core::VideoSettings {
            width: 320,
            height: 240,
        });
        let (wide, _) = p.add_asset("/media/wide.mp4".into(), probe_size(640, 360));
        let (same, _) = p.add_asset("/media/same.mp4".into(), probe_size(320, 240));
        let clip_of = |asset| {
            Segment::Clip(Clip::new(
                1,
                asset,
                Seconds::from_integer(0),
                Seconds::from_integer(2),
            ))
        };
        assert!(needs_proxy(&p, &clip_of(wide)), "off the sequence size");
        assert!(
            !needs_proxy(&p, &clip_of(same)),
            "the sequence size plays raw"
        );
    }

    /// A span boundary from one track can fall inside another track's clip, so
    /// the slice trims the edges — and a sped-up clip's source span follows its
    /// rate, not the timeline delta. Muted tracks drop out (the export drops
    /// them too).
    #[test]
    fn slice_tracks_trims_edges_and_pads() {
        let secs = Seconds::from_integer;
        let mut p = Project::new();
        p.fps = Some(secs(30));
        p.video = Some(nle_core::VideoSettings {
            width: 320,
            height: 240,
        });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_size(320, 240));
        // v1: a 2× clip reading source 0..8 → timeline [0,4).
        p.timeline.tracks[0].segments = vec![Segment::Clip(Clip {
            id: 1,
            asset: a,
            asset_in: secs(0),
            asset_out: secs(8),
            speed: Seconds::from_integer(2),
            effects: Vec::new(),
        })];
        // v2: a clip at [1,3), and nothing after it.
        let vi = p.timeline.add_track(nle_core::timeline::TrackKind::Video);
        p.timeline.tracks[vi].segments = vec![
            Segment::Gap(Gap { duration: secs(1) }),
            Segment::Clip(Clip::new(2, a, secs(0), secs(2))),
            Segment::Gap(Gap { duration: secs(1) }),
        ];

        let slice = slice_tracks(&p, secs(1), secs(3));
        assert_eq!(slice.len(), 2);
        // v1: trimmed to [1,3) → source 2..6 (2 s).
        match &slice[0].segments[..] {
            [Segment::Clip(c)] => {
                assert_eq!((c.asset_in, c.asset_out), (secs(2), secs(6)));
                assert_eq!(c.duration(), secs(2));
            }
            other => panic!("expected one trimmed clip, got {other:?}"),
        }
        // v2: the leading gap is dropped, the clip is whole.
        match &slice[1].segments[..] {
            [Segment::Clip(c)] => assert_eq!((c.asset_in, c.asset_out), (secs(0), secs(2))),
            other => panic!("expected the whole clip, got {other:?}"),
        }
        for track in &slice {
            assert_eq!(track.end(), secs(2), "every slice spans the span");
        }

        // A track that has nothing in the span becomes one gap of the span.
        let short = slice_tracks(&p, secs(3), secs(4));
        match &short[1].segments[..] {
            [Segment::Gap(g)] => assert_eq!(g.duration, secs(1)),
            other => panic!("expected a gap, got {other:?}"),
        }

        // Muted tracks are not part of the export (or the composite).
        p.timeline.tracks[1].muted = true;
        assert_eq!(slice_tracks(&p, secs(1), secs(3)).len(), 1);
    }

    /// A span is widened to contain a transition whole, so the slice must keep
    /// it as-is — not try to cut a crossfade (the monitor used to panic here).
    #[test]
    fn slice_tracks_keeps_a_transition_whole() {
        let secs = Seconds::from_integer;
        let mut p = Project::new();
        p.fps = Some(secs(30));
        p.video = Some(nle_core::VideoSettings {
            width: 320,
            height: 240,
        });
        let (a, _) = p.add_asset("/media/red.mp4".into(), probe_size(320, 240));
        // [clip 0..1][transition 1..2][clip 2..3]
        p.timeline.tracks[0].segments = vec![
            Segment::Clip(Clip::new(1, a, secs(0), secs(1))),
            Segment::Transition(Transition {
                id: 3,
                dur: secs(1),
                kind: TransitionKind::Dissolve,
                a: TransitionSource::Clip {
                    asset: a,
                    src_in: secs(0),
                    src_out: secs(1),
                },
                b: TransitionSource::Clip {
                    asset: a,
                    src_in: secs(0),
                    src_out: secs(1),
                },
            }),
            Segment::Clip(Clip::new(2, a, secs(0), secs(1))),
        ];
        let slice = slice_tracks(&p, secs(0), secs(3));
        assert!(
            slice[0]
                .segments
                .iter()
                .any(|s| matches!(s, Segment::Transition(_))),
            "the blend is kept whole: {:?}",
            slice[0].segments
        );
    }

    /// Editing a LUT file in place must re-render: the segment JSON carries
    /// only its path, so the file's mtime is the signal that the grade changed.
    #[test]
    fn proxy_hash_tracks_lut_files() {
        let dir = tempfile::tempdir().unwrap();
        let lut = dir.path().join("g.cube");
        std::fs::write(&lut, "LUT_3D_SIZE 2\n").unwrap();
        let mut c = clip();
        if let Segment::Clip(cl) = &mut c {
            cl.effects = vec![Effect::Lut {
                path: lut.display().to_string(),
            }];
        }
        let p = Project::new();
        let before = proxy_hash(&p, &c, 320, 240);
        // Rewrite the file with a later mtime.
        let f = std::fs::File::options().write(true).open(&lut).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        assert_ne!(
            proxy_hash(&p, &c, 320, 240),
            before,
            "a changed LUT file invalidates the proxy"
        );
    }

    #[test]
    fn proxy_hash_tracks_the_segment() {
        let p = Project::new();
        let a = clip();
        let mut b = clip();
        if let Segment::Clip(cl) = &mut b {
            cl.effects = vec![Effect::Scale { w: 2, h: 2 }];
        }
        let ha = proxy_hash(&p, &a, 320, 240);
        assert_eq!(ha, proxy_hash(&p, &a, 320, 240), "stable");
        assert_ne!(ha, proxy_hash(&p, &b, 320, 240), "effects change it");
        let mut s = clip();
        if let Segment::Clip(cl) = &mut s {
            cl.speed = Seconds::new(1, 2);
        }
        assert_ne!(ha, proxy_hash(&p, &s, 320, 240), "speed changes it");
        assert_ne!(
            ha,
            proxy_hash(&p, &a, 640, 480),
            "geometry is part of the key"
        );
        // The render bakes the frame rate in, so it must be too.
        let mut p24 = Project::new();
        p24.fps = Some(Seconds::from_integer(24));
        let mut p30 = Project::new();
        p30.fps = Some(Seconds::from_integer(30));
        assert_ne!(
            proxy_hash(&p24, &a, 320, 240),
            proxy_hash(&p30, &a, 320, 240),
            "frame rate is part of the key"
        );
    }
}
