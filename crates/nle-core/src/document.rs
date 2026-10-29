//! The project and its undo stack, owned together.
//!
//! They are only meaningful as a pair: undo verifies the exact topology the
//! change was recorded from, so a project swapped without its history (or a
//! history carried across a reload) fails at runtime with "history entry does
//! not apply". Owning both makes that drift unrepresentable.
//!
//! It is also the only type that saves a project, and `save` clamps to the
//! media first — so no front-end can persist a clip that reads past EOF, even
//! a new command that never heard of `clamp_project`. Construction is still a
//! plain [`Project`] (imports build one from an EDL/OTIO/detection); adoption
//! goes through [`Document::new`] or [`Document::rebuild`].

use std::ops::Deref;
use std::path::Path;

use crate::action::{self, Action, Outcome, Register};
use crate::edit::{self, Batch, EditError, History, UndoOutcome};
use crate::project::{Project, ProjectError, VideoSettings};
use crate::time::Seconds;

/// A project plus the undo stack recorded against it.
#[derive(Debug, Default)]
pub struct Document {
    project: Project,
    history: History,
}

impl Document {
    /// Adopt a project as-is, with an empty stack (load, tests).
    pub fn new(project: Project) -> Self {
        Self {
            project,
            history: History::default(),
        }
    }

    /// Adopt a project and the stack persisted beside it (a TUI/CLI reload).
    pub fn with_history(project: Project, history: History) -> Self {
        Self { project, history }
    }

    /// Adopt a freshly built project — init, import, detect — clamped to its
    /// media, with a fresh stack. Returns the clamp notes for the caller to
    /// print; the project is already corrected either way.
    pub fn rebuild(project: Project) -> (Self, Vec<String>) {
        let mut doc = Self::new(project);
        let notes = edit::clamp_project(&mut doc.project);
        (doc, notes)
    }

    pub fn set_history(&mut self, history: History) {
        self.history = history;
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn into_parts(self) -> (Project, History) {
        (self.project, self.history)
    }

    pub fn set_playhead(&mut self, at: f64) -> f64 {
        self.project.set_playhead(at)
    }

    /// Fill the sequence settings from the first clip (a real NLE sets the
    /// project's rate and size once, from the clip that defines it).
    pub fn set_sequence(&mut self, fps: Option<Seconds>, video: Option<VideoSettings>) {
        if self.project.fps.is_none() {
            self.project.fps = fps;
        }
        if self.project.video.is_none() {
            self.project.video = video;
        }
    }

    /// Run one catalog action, recording it in the history.
    pub fn edit(
        &mut self,
        action: &Action,
        from: f64,
        register: &mut Option<Register>,
    ) -> Result<Outcome, EditError> {
        action::run_action(&mut self.project, &mut self.history, register, action, from)
    }

    /// Apply a prepared batch (an extend commit builds one before committing).
    pub fn apply(&mut self, batch: Batch, playhead: i64) -> Result<(), EditError> {
        self.history.apply(&mut self.project, batch, playhead)
    }

    pub fn undo(&mut self) -> Result<UndoOutcome, EditError> {
        self.history.undo(&mut self.project)
    }

    pub fn redo(&mut self) -> Result<UndoOutcome, EditError> {
        self.history.redo(&mut self.project)
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    pub fn depth(&self) -> usize {
        self.history.depth()
    }

    pub fn add_asset(
        &mut self,
        path: std::path::PathBuf,
        report: crate::media::RawProbe,
        playhead: i64,
    ) -> (u32, String) {
        self.history
            .add_asset(&mut self.project, path, report, playhead)
    }

    pub fn remove_asset(&mut self, id: u32, playhead: i64) -> Result<String, EditError> {
        self.history.remove_asset(&mut self.project, id, playhead)
    }

    pub fn edit_tracks<F>(&mut self, playhead: i64, edit: F) -> Result<String, EditError>
    where
        F: FnOnce(&mut Project) -> Result<String, EditError>,
    {
        self.history.edit_tracks(&mut self.project, playhead, edit)
    }

    /// Clamp, then save. The only persisted write path.
    pub fn save(&mut self, path: &Path) -> Result<(), ProjectError> {
        edit::clamp_project(&mut self.project);
        self.project.save(path)
    }

    /// Persist the undo stack beside the project.
    pub fn save_history(&self, path: &Path) -> std::io::Result<()> {
        self.history.save(path)
    }

    /// Drop the stack and remove its sidecar (a rebuild re-baselines).
    pub fn reset_history(&mut self, project_path: &Path) {
        self.history = History::default();
        let _ = std::fs::remove_file(History::sidecar_path(project_path));
    }
}

impl Deref for Document {
    type Target = Project;

    fn deref(&self) -> &Project {
        &self.project
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RawFormat, RawProbe, RawStream};
    use crate::time::Seconds;
    use crate::timeline::{Clip, Segment};

    fn probe(duration: &str) -> RawProbe {
        RawProbe {
            streams: vec![RawStream {
                index: 0,
                codec_type: Some("video".into()),
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

    fn document() -> Document {
        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (asset, _) = p.add_asset("/media/red.mp4".into(), probe("4"));
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 1,
            asset,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        Document::new(p)
    }

    #[test]
    fn save_clamps_a_clip_that_reads_past_its_media() {
        let mut doc = document();
        if let Segment::Clip(c) = &mut doc.project.timeline.video_track_mut().unwrap().segments[0] {
            c.asset_out = Seconds::from_integer(9);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nle.json");
        doc.save(&path).unwrap();
        let loaded = Project::load(&path).unwrap();
        let seg = &loaded.timeline.video_track().unwrap().segments[0];
        assert!(
            seg.duration() <= Seconds::from_integer(4),
            "saved {} s of a 4 s asset",
            crate::time::format_seconds(&seg.duration())
        );
    }

    #[test]
    fn rebuild_clamps_and_drops_the_old_stack() {
        let mut doc = document();
        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        assert!(doc.can_undo());

        let mut p = Project::new();
        p.fps = Some(Seconds::new(30, 1));
        let (asset, _) = p.add_asset("/media/red.mp4".into(), probe("4"));
        p.timeline.video_track_mut().unwrap().segments = vec![Segment::Clip(Clip {
            id: 7,
            asset,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(9),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        })];
        let (doc, notes) = Document::rebuild(p);
        assert_eq!(doc.depth(), 0);
        assert!(!notes.is_empty(), "the clamp reported the adjustment");
        let seg = &doc.timeline.video_track().unwrap().segments[0];
        assert!(seg.duration() <= Seconds::from_integer(4));
    }

    #[test]
    fn an_edit_is_recorded_and_undone() {
        let mut doc = document();
        let batch = edit::split_on(&doc, 0, 1.0).unwrap();
        doc.apply(batch, 0).unwrap();
        assert_eq!(doc.depth(), 1);
        assert_eq!(doc.timeline.video_track().unwrap().segments.len(), 2);
        doc.undo().unwrap();
        assert_eq!(doc.timeline.video_track().unwrap().segments.len(), 1);
    }
}
