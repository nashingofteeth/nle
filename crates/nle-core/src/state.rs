//! Per-project state paths.
//!
//! A project keeps its history, clipboard, and disposable render cache in a
//! hidden `.nile/<project-stem>/` directory beside the project file. Keeping
//! them together makes one project self-contained and lets the proxy cache be
//! pruned per project instead of globally.

use std::path::{Path, PathBuf};

pub const STATE_DIR: &str = ".nile";

/// Directory holding all state for one project:
/// `foo/nile.json` → `foo/.nile/nile/`.
pub fn state_dir(project_path: &Path) -> PathBuf {
    let parent = project_path.parent().filter(|p| !p.as_os_str().is_empty());
    let stem = project_path
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("project"));
    match parent {
        Some(parent) => parent.join(STATE_DIR).join(stem),
        None => PathBuf::from(STATE_DIR).join(stem),
    }
}

pub fn history_path(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("history.json")
}

pub fn clipboard_path(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("clipboard.json")
}

pub fn cache_dir(project_path: &Path) -> PathBuf {
    state_dir(project_path).join("cache")
}

/// Pre-state-directory sidecar locations, used for a one-time copy.
pub(crate) fn legacy_history_path(project_path: &Path) -> PathBuf {
    project_path.with_extension("history.json")
}

pub(crate) fn legacy_clipboard_path(project_path: &Path) -> PathBuf {
    project_path.with_extension("clipboard.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_lives_beside_the_project_in_a_stem_directory() {
        let p = Path::new("/videos/trip/nile.json");
        assert_eq!(state_dir(p), PathBuf::from("/videos/trip/.nile/nile"));
        assert_eq!(
            history_path(p),
            PathBuf::from("/videos/trip/.nile/nile/history.json")
        );
        assert_eq!(
            clipboard_path(p),
            PathBuf::from("/videos/trip/.nile/nile/clipboard.json")
        );
        assert_eq!(cache_dir(p), PathBuf::from("/videos/trip/.nile/nile/cache"));
    }

    #[test]
    fn a_bare_project_name_uses_the_current_directory() {
        assert_eq!(
            history_path(Path::new("nile.json")),
            PathBuf::from(".nile/nile/history.json")
        );
    }
}
