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
use nle_core::timeline::{Segment, Track, TrackKind};
use nle_ffmpeg::graph::ExportSpec;
use nle_ffmpeg::{Encoder, ExportError, export};
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
pub fn needs_proxy(seg: &Segment) -> bool {
    match seg {
        Segment::Clip(c) => !c.effects.is_empty(),
        Segment::Transition(_) => true,
        Segment::Gap(_) => false,
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

/// The cached proxy for `seg`, rendering it on demand. The segment is put
/// on a track of its own and handed to the export graph compiler — which
/// already applies effects and blends transitions — so the proxy is the
/// one source of truth for "what this segment looks like".
pub fn proxy_for(project: &Project, seg: &Segment) -> Result<PathBuf, ProxyError> {
    let (w, h) = match &project.video {
        Some(v) => (v.width, v.height),
        None => return Err(ProxyError::NoSettings),
    };
    if project.fps.is_none() {
        return Err(ProxyError::NoSettings);
    }
    let hash = proxy_hash(project, seg, w, h);
    let dir = proxy_cache_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{hash:x}.mp4"));
    if path.exists() {
        return Ok(path);
    }

    let mut p = project.clone();
    p.timeline.tracks = vec![Track {
        kind: TrackKind::Video,
        muted: false,
        segments: vec![seg.clone()],
    }];
    // Write to a temp name (still ending in .mp4 so ffmpeg can infer the
    // muxer) and rename into place: a crash mid-render must not leave a
    // half-written file that `path.exists()` would then serve forever (the
    // hash is stable, so it would never re-render).
    let tmp = dir.join(format!("{hash:x}.tmp-{}.mp4", std::process::id()));
    let spec = ExportSpec {
        output: tmp.clone(),
        encoder: Encoder::Cpu,
        crf: 18,
        preset: "ultrafast".into(),
        bitrate: "8M".into(),
    };
    if let Err(e) = export(&p, &spec) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_core::Seconds;
    use nle_core::timeline::{Clip, Effect, Gap, Transition, TransitionKind, TransitionSource};

    fn clip() -> Segment {
        Segment::Clip(Clip::new(
            1,
            1,
            Seconds::from_integer(0),
            Seconds::from_integer(2),
        ))
    }

    #[test]
    fn needs_proxy_only_for_what_edl_cant_express() {
        // A plain clip and a gap play straight from source.
        assert!(!needs_proxy(&clip()));
        assert!(!needs_proxy(&Segment::Gap(Gap {
            duration: Seconds::from_integer(1)
        })));
        // An effect clip and a transition need rendering.
        let mut c = clip();
        if let Segment::Clip(cl) = &mut c {
            cl.effects = vec![Effect::Scale { w: 2, h: 2 }];
        }
        assert!(needs_proxy(&c), "effects are rendered");
        assert!(needs_proxy(&Segment::Transition(Transition {
            id: 1,
            dur: Seconds::from_integer(1),
            kind: TransitionKind::Dissolve,
            a: TransitionSource::Black,
            b: TransitionSource::Black,
        })));
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
