//! The yank register: one segment plus, for a clip, the asset path it came
//! from, in a sidecar beside the project. Convenience state — a missing or
//! version-skewed file reads as empty.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::edit::EditError;
use crate::project::Project;
use crate::timeline::Segment;

/// Register sidecar format version. Bumped on a shape change; a skewed or
/// unreadable file reads as an empty register (the register is
/// convenience, not critical state — the project file is).
/// v2 carries the yanked clip's asset path.
pub const REGISTER_VERSION: u32 = 2;

/// A yanked segment plus, for a clip, the asset path it came from. Asset
/// ids are **project-scoped**, so a register that outlives its project
/// (a rebuilt `nle.json`, a copied sidecar) must be resolved against the
/// pool by path — pasting the wrong media silently would be worse than
/// refusing.
#[derive(Debug, Clone, PartialEq)]
pub struct Register {
    pub segment: Segment,
    pub path: Option<PathBuf>,
}

impl Register {
    /// The register entry for `segment`, resolving a clip's asset path
    /// from `project`.
    pub fn of(project: &Project, segment: Segment) -> Self {
        let path = match &segment {
            Segment::Clip(c) => project.asset(c.asset).map(|a| a.path.clone()),
            Segment::Gap(_) => None,
            // A transition carries its own asset ids; resolving those across
            // projects is out of scope (register paste of a transition).
            Segment::Transition(_) => None,
        };
        Self { segment, path }
    }

    /// Resolve against `project`: same file, possibly under a different
    /// asset id. Refuses when the media isn't in this pool at all.
    pub fn resolve(&self, project: &Project) -> Result<Segment, EditError> {
        let Segment::Clip(c) = &self.segment else {
            return Ok(self.segment.clone());
        };
        let Some(path) = &self.path else {
            return Ok(self.segment.clone());
        };
        if project.asset(c.asset).map(|a| &a.path) == Some(path) {
            return Ok(self.segment.clone()); // same id, same file
        }
        let Some(asset) = project.media.iter().find(|a| &a.path == path) else {
            return Err(EditError::RegisterForeign(path.display().to_string()));
        };
        let mut segment = self.segment.clone();
        if let Segment::Clip(clip) = &mut segment {
            clip.asset = asset.id;
        }
        Ok(segment)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RegisterFile {
    version: u32,
    segment: Segment,
    #[serde(default)]
    path: Option<PathBuf>,
}

/// The register sidecar for a project (`nle.json` → `nle.clipboard.json`).
pub fn register_path(project_path: &Path) -> PathBuf {
    project_path.with_extension("clipboard.json")
}

/// The yanked segment a project's register holds, if any.
pub fn load_register(project_path: &Path) -> Option<Register> {
    let text = std::fs::read_to_string(register_path(project_path)).ok()?;
    let file: RegisterFile = serde_json::from_str(&text).ok()?;
    (file.version == REGISTER_VERSION).then_some(Register {
        segment: file.segment,
        path: file.path,
    })
}

/// Persist the register; `None` clears it (the file goes away). Atomic,
/// like the project save — a crash can't leave half a register.
pub fn save_register(project_path: &Path, register: Option<&Register>) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = register_path(project_path);
    let Some(register) = register else {
        let _ = std::fs::remove_file(path);
        return Ok(());
    };
    let file = RegisterFile {
        version: REGISTER_VERSION,
        segment: register.segment.clone(),
        path: register.path.clone(),
    };
    let mut json = serde_json::to_string_pretty(&file)
        .map_err(|e| std::io::Error::other(format!("register serialize: {e}")))?;
    json.push('\n');
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
