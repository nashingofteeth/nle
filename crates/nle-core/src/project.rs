//! The project file: media pool + timeline, serialized as JSON (the EDL).
//!
//! Times are exact rationals serialized as "n/d" strings; positions never
//! drift no matter how many times you edit.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::media::RawProbe;
use crate::time::{rational_ser, Seconds};
use crate::timeline::{Segment, Timeline, TimelineError};

/// Project file format version. No migration path is kept (see
/// README "Compatibility: none, by design"): old files are rejected,
/// never silently misread.
pub const PROJECT_VERSION: u32 = 2;

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("project file {0:?} already exists (use --force to overwrite)")]
    Exists(PathBuf),
    #[error("no project at {0:?} — run `nle init` first")]
    NotFound(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid project file: {0}")]
    Json(#[from] serde_json::Error),
    #[error("project version {0} is not supported (this build reads v{expected})", expected = PROJECT_VERSION)]
    Version(u32),
    #[error(transparent)]
    Timeline(#[from] TimelineError),
    #[error("asset #{0}: {1}")]
    Asset(u32, String),
    #[error("asset #{0} not found")]
    AssetNotFound(u32),
    #[error("asset #{0} is used by clip(s) {1:?} — remove them first")]
    AssetInUse(u32, Vec<u32>),
}

/// Output sequence settings (set from the first clip, changeable later).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
}

/// One file in the media pool. `probe` is stored so the file never
/// needs re-probing — the project is self-describing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: u32,
    pub path: PathBuf,
    pub probe: RawProbe,
}

impl Asset {
    pub fn duration(&self) -> Option<Seconds> {
        self.probe.duration()
    }

    pub fn fps(&self) -> Option<Seconds> {
        self.probe.fps()
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.probe.size()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// Project file format — v2 is the segment model. (v1 files are not
    /// readable; the only v1 project was the regenerable demo.)
    version: u32,
    /// Output frame rate — None until the first clip sets it.
    #[serde(default, with = "rational_ser::opt")]
    pub fps: Option<Seconds>,
    /// Output size — None until the first clip sets it.
    #[serde(default)]
    pub video: Option<VideoSettings>,
    #[serde(default)]
    pub media: Vec<Asset>,
    #[serde(default)]
    pub timeline: Timeline,
}

impl Default for Project {
    fn default() -> Self {
        Self::new()
    }
}

impl Project {
    pub fn new() -> Self {
        Self {
            version: PROJECT_VERSION,
            fps: None,
            video: None,
            media: Vec::new(),
            timeline: Timeline::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProjectError::NotFound(path.to_path_buf()))
            }
            Err(e) => return Err(e.into()),
        };
        let project: Project = serde_json::from_str(&text)?;
        if project.version != PROJECT_VERSION {
            return Err(ProjectError::Version(project.version));
        }
        project.validate()?;
        Ok(project)
    }

    /// Atomic save: write a temp file in the same directory, then rename
    /// over the target. A crash mid-write can't corrupt the project.
    pub fn save(&self, path: &Path) -> Result<(), ProjectError> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let tmp = NamedTempFile::new_in(dir)?;
        tmp.as_file().write_all(json.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Structural sanity of the whole project.
    pub fn validate(&self) -> Result<(), ProjectError> {
        self.timeline.validate()?;
        for track in &self.timeline.tracks {
            for seg in &track.segments {
                if let Segment::Clip(clip) = seg
                    && !self.media.iter().any(|a| a.id == clip.asset) {
                        return Err(ProjectError::Asset(clip.asset, "not in the media pool".into()));
                    }
            }
        }
        Ok(())
    }

    /// Add an asset, or return the existing one. Returns (id, was_new).
    pub fn add_asset(&mut self, path: PathBuf, probe: RawProbe) -> (u32, bool) {
        if let Some(a) = self.media.iter().find(|a| a.path == path) {
            return (a.id, false);
        }
        let id = self.next_asset_id();
        self.media.push(Asset { id, path, probe });
        (id, true)
    }

    pub fn asset(&self, id: u32) -> Option<&Asset> {
        self.media.iter().find(|a| a.id == id)
    }

    pub fn asset_by_path(&self, path: &Path) -> Option<&Asset> {
        self.media.iter().find(|a| a.path == path)
    }

    /// Clip ids across the timeline that reference asset `id` (video track
    /// now; all tracks once M4 lands).
    pub fn asset_usage(&self, id: u32) -> Vec<u32> {
        self.timeline
            .tracks
            .iter()
            .flat_map(|t| t.segments.iter())
            .filter_map(|s| match s {
                Segment::Clip(c) if c.asset == id => Some(c.id),
                _ => None,
            })
            .collect()
    }

    /// Remove an asset from the pool. Refuses while any clip references it
    /// (the caller lists the clips); no clip is ever silently dropped.
    pub fn remove_asset(&mut self, id: u32) -> Result<Asset, ProjectError> {
        let used = self.asset_usage(id);
        if !used.is_empty() {
            return Err(ProjectError::AssetInUse(id, used));
        }
        let pos = self
            .media
            .iter()
            .position(|a| a.id == id)
            .ok_or(ProjectError::AssetNotFound(id))?;
        Ok(self.media.remove(pos))
    }

    fn next_asset_id(&self) -> u32 {
        self.media.iter().map(|a| a.id).max().unwrap_or(0) + 1
    }

    pub(crate) fn next_clip_id(&self) -> u32 {
        self.timeline
            .tracks
            .iter()
            .flat_map(|t| {
                t.segments.iter().filter_map(|s| match s {
                    Segment::Clip(c) => Some(c.id),
                    Segment::Gap(_) => None,
                })
            })
            .max()
            .unwrap_or(0)
            + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{self, History};
    use crate::media::{RawFormat, RawStream};
    use crate::time::Seconds;
    use crate::timeline::Segment;
    use crate::Clip;

    fn probe_report(duration: &str, w: u32, h: u32, rate: &str, audio: bool) -> RawProbe {
        let mut streams = vec![RawStream {
            index: 0,
            codec_type: Some("video".into()),
            codec_name: Some("h264".into()),
            width: Some(w),
            height: Some(h),
            r_frame_rate: Some(rate.into()),
            pix_fmt: Some("yuv420p".into()),
            ..Default::default()
        }];
        if audio {
            streams.push(RawStream {
                index: 1,
                codec_type: Some("audio".into()),
                codec_name: Some("aac".into()),
                sample_rate: Some("48000".into()),
                channels: Some(2),
                channel_layout: Some("stereo".into()),
                ..Default::default()
            });
        }
        RawProbe {
            streams,
            format: RawFormat {
                format_name: "mp4".into(),
                duration: Some(duration.into()),
                ..Default::default()
            },
        }
    }

    fn sample_project() -> Project {
        let mut p = Project::new();
        let (a0, _) = p.add_asset("a.mp4".into(), probe_report("10", 1920, 1080, "30/1", true));
        let (a1, _) = p.add_asset("b.mp4".into(), probe_report("8", 1280, 720, "30000/1001", false));
        // [0..2) a.mp4 src 0..2 · [2..5) b.mp4 src 2..5
        let mut h = History::default();
        let batch = edit::paste_ripple(
            &p,
            &Segment::Clip(Clip { id: 0, asset: a0, asset_in: Seconds::from_integer(0), asset_out: Seconds::from_integer(2) }),
            0.0,
        )
        .unwrap();
        h.apply(&mut p, batch, 0).unwrap();
        let batch = edit::paste_ripple(
            &p,
            &Segment::Clip(Clip { id: 0, asset: a1, asset_in: Seconds::from_integer(2), asset_out: Seconds::from_integer(5) }),
            2.0,
        )
        .unwrap();
        println!("second batch: {batch:?}");
        h.apply(&mut p, batch, 0).unwrap();
        p
    }

    #[test]
    fn asset_dedup_by_path() {
        let mut p = Project::new();
        let (id1, added1) = p.add_asset("/x/a.mp4".into(), probe_report("10", 1920, 1080, "30/1", true));
        let (id2, added2) = p.add_asset("/x/a.mp4".into(), probe_report("10", 1920, 1080, "30/1", true));
        assert!(added1 && !added2);
        assert_eq!(id1, id2);
        assert_eq!(p.media.len(), 1);
    }

    #[test]
    fn asset_usage_and_guarded_removal() {
        let mut p = sample_project(); // two assets, both used by a clip
        assert_eq!(p.asset_usage(1), vec![1]);
        assert_eq!(p.asset_usage(2), vec![2]);
        // In use: refuses, naming the clips.
        assert!(matches!(p.remove_asset(1), Err(ProjectError::AssetInUse(1, ref v)) if v == &vec![1]));
        assert_eq!(p.media.len(), 2);
        // Unused: an imported-but-unplaced asset removes cleanly.
        let (id, _) = p.add_asset("/x/spare.mp4".into(), probe_report("5", 320, 240, "30/1", false));
        assert!(p.asset_usage(id).is_empty());
        assert!(p.remove_asset(id).is_ok());
        assert!(p.remove_asset(id).is_err(), "already gone");
    }

    #[test]
    fn clip_ids_are_fresh_across_inserts() {
        let p = sample_project();
        let ids: Vec<u32> = p.timeline.video_track().unwrap().segments.iter().filter_map(|s| match s {
            Segment::Clip(c) => Some(c.id),
            Segment::Gap(_) => None,
        }).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn validate_rejects_missing_asset_refs() {
        let mut p = sample_project();
        if let Segment::Clip(c) = &mut p.timeline.video_track_mut().unwrap().segments[0] {
            c.asset = 99;
        }
        assert!(matches!(p.validate(), Err(ProjectError::Asset(99, _))));
    }

    #[test]
    fn serde_round_trip_is_exact() {
        let p = sample_project();
        let json = serde_json::to_string_pretty(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
        // human-readable exact times in the file itself
        assert!(json.contains("\"30000/1001\""), "{json}");
    }

    #[test]
    fn save_and_load_round_trip() {
        let p = sample_project();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        p.save(&path).unwrap();
        let loaded = Project::load(&path).unwrap();
        assert_eq!(p, loaded);
    }

    #[test]
    fn rejects_other_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        std::fs::write(&path, "{\"version\": 1}").unwrap();
        assert!(matches!(Project::load(&path), Err(ProjectError::Version(1))));
    }

    #[test]
    fn version_error_names_the_supported_version() {
        let msg = ProjectError::Version(1).to_string();
        // The message must name the version THIS build reads (the constant),
        // not a stale hardcoded one.
        assert!(msg.contains(&format!("v{PROJECT_VERSION}")), "{msg}");
        assert!(msg.contains("version 1"), "{msg}");
    }
}
