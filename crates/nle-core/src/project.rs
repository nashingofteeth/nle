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
use crate::time::{format_seconds, rational_ser, Seconds};
use crate::timeline::{Clip, Timeline, TimelineError};

pub const PROJECT_VERSION: u32 = 1;

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
    #[error("project version {0} is not supported (this build reads v1)")]
    Version(u32),
    #[error(transparent)]
    Timeline(#[from] TimelineError),
    #[error("asset #{0}: {1}")]
    Asset(u32, String),
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
    pub version: u32,
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
            for clip in &track.clips {
                if !self.media.iter().any(|a| a.id == clip.asset) {
                    return Err(ProjectError::Timeline(TimelineError::UnknownAsset(clip.asset)));
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

    fn next_asset_id(&self) -> u32 {
        self.media.iter().map(|a| a.id).max().unwrap_or(0) + 1
    }

    fn next_clip_id(&self) -> u32 {
        self.timeline
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.id))
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Place a clip on the video track.
    ///
    /// `at = None` appends after the last clip. Validates the source range
    /// against the asset and rejects overlaps. The first clip added also
    /// fixes the project's fps and output size (sequence settings).
    pub fn add_clip(
        &mut self,
        asset_id: u32,
        at: Option<Seconds>,
        asset_in: Seconds,
        asset_out: Seconds,
    ) -> Result<u32, ProjectError> {
        let asset_fps;
        let asset_size;
        {
            let asset = self
                .asset(asset_id)
                .ok_or_else(|| ProjectError::Asset(asset_id, "not in media pool — run `nle import`".into()))?;
            if !asset.probe.has_video() {
                return Err(ProjectError::Asset(asset_id, "has no video stream".into()));
            }
            let duration = asset
                .duration()
                .ok_or_else(|| ProjectError::Asset(asset_id, "has unknown duration".into()))?;
            if asset_in >= asset_out {
                return Err(ProjectError::Asset(
                    asset_id,
                    format!(
                        "in-point {} must be before out-point {}",
                        format_seconds(&asset_in),
                        format_seconds(&asset_out)
                    ),
                ));
            }
            if asset_out > duration {
                return Err(ProjectError::Asset(
                    asset_id,
                    format!(
                        "out-point {} is beyond the file's duration {}",
                        format_seconds(&asset_out),
                        format_seconds(&duration)
                    ),
                ));
            }
            asset_fps = asset.fps();
            asset_size = asset.video_size();
        }

        // The first clip defines the sequence settings (like a real NLE).
        if self.fps.is_none() {
            self.fps = asset_fps;
        }
        if self.video.is_none() {
            self.video = asset_size.map(|(w, h)| VideoSettings { width: w, height: h });
        }

        let at = match at {
            Some(t) => t,
            None => {
                let track = self
                    .timeline
                    .video_track()
                    .ok_or(ProjectError::Timeline(TimelineError::NoVideoTrack))?;
                track.end()
            }
        };

        let clip = Clip {
            id: self.next_clip_id(),
            asset: asset_id,
            start: at,
            asset_in,
            asset_out,
        };
        let track = self
            .timeline
            .video_track_mut()
            .ok_or(ProjectError::Timeline(TimelineError::NoVideoTrack))?;
        track.try_insert(clip.clone())?;
        self.validate()?;
        Ok(clip.id)
    }

    pub fn remove_clip(&mut self, id: u32) -> Result<(), ProjectError> {
        self.timeline.remove_clip(id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RawFormat, RawStream};

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
        p.add_clip(a0, None, Seconds::from_integer(0), Seconds::from_integer(10))
            .unwrap();
        p.add_clip(a1, Some(Seconds::from_integer(12)), Seconds::new(2, 1), Seconds::new(5, 1))
            .unwrap();
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
    fn clips_append_defaults_and_first_clip_sets_settings() {
        let mut p = Project::new();
        let (a, _) = p.add_asset("a.mp4".into(), probe_report("10", 1920, 1080, "30000/1001", true));
        let c1 = p.add_clip(a, None, Seconds::from_integer(0), Seconds::from_integer(8)).unwrap();
        assert_eq!(c1, 1);
        // first clip defines the sequence settings
        assert_eq!(p.fps, Some(Seconds::new(30000, 1001)));
        assert_eq!(p.video, Some(VideoSettings { width: 1920, height: 1080 }));
        // second clip appends after the first (src 2–4s, within the 10s asset)
        let _c2 = p.add_clip(a, None, Seconds::from_integer(2), Seconds::from_integer(4)).unwrap();
        let track = p.timeline.video_track().unwrap();
        assert_eq!(track.clips[1].start, Seconds::from_integer(8));
        assert_eq!(track.end(), Seconds::from_integer(10)); // 8 + 2s
    }

    #[test]
    fn rejects_overlap_and_bad_ranges() {
        let mut p = sample_project();
        // overlap with clip #1 (0–10)
        assert!(matches!(
            p.add_clip(1, Some(Seconds::from_integer(5)), Seconds::from_integer(0), Seconds::from_integer(2)),
            Err(ProjectError::Timeline(TimelineError::Overlap { .. }))
        ));
        // out beyond asset duration
        assert!(matches!(
            p.add_clip(1, None, Seconds::from_integer(0), Seconds::from_integer(99)),
            Err(ProjectError::Asset(..))
        ));
        // inverted range
        assert!(p.add_clip(1, None, Seconds::from_integer(5), Seconds::from_integer(5)).is_err());
        // unknown asset
        assert!(p.add_clip(42, None, Seconds::from_integer(0), Seconds::from_integer(1)).is_err());
    }

    #[test]
    fn remove_then_readd_uses_fresh_ids() {
        let mut p = sample_project();
        p.remove_clip(1).unwrap();
        assert!(p.remove_clip(1).is_err());
        let id = p.add_clip(2, None, Seconds::from_integer(0), Seconds::from_integer(1)).unwrap();
        assert_eq!(id, 3); // max+1, no id reuse
    }

    #[test]
    fn validate_rejects_missing_asset_refs() {
        let mut p = Project::new();
        p.timeline.video_track_mut().unwrap().clips.push(Clip {
            id: 1,
            asset: 99,
            start: Seconds::from_integer(0),
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(1),
        });
        assert!(matches!(p.validate(), Err(ProjectError::Timeline(TimelineError::UnknownAsset(99)))));
    }

    #[test]
    fn serde_round_trip_is_exact() {
        let p = sample_project();
        let json = serde_json::to_string_pretty(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
        // human-readable exact times in the file itself
        assert!(json.contains("\"30000/1001\""), "{json}");
        assert!(json.contains("\"12/1\""), "{json}");
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
}
