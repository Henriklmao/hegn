//! Atomic JSON file store for [`PersistedState`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::model::PersistedState;

/// JSON file store. Writes are atomic (tmp + rename).
#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
    state: PersistedState,
}

impl StateStore {
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path, state }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn state(&self) -> &PersistedState {
        &self.state
    }

    pub fn touch(&mut self, path: &Path) -> Result<()> {
        self.state.touch(path);
        self.save()
    }

    /// Hide or unhide a project in the picker and persist the change.
    pub fn set_project_hidden(&mut self, path: &Path, hidden: bool) -> Result<()> {
        if hidden {
            self.state.hide_project(path);
        } else {
            self.state.unhide_project(path);
        }
        self.save()
    }

    /// Remove a project from recent state and the hidden list.
    pub fn forget_project(&mut self, path: &Path) -> Result<()> {
        self.state.forget_project(path);
        self.save()
    }

    pub fn is_hidden(&self, path: &Path) -> bool {
        self.state.is_hidden(path)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(&self.state)?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, raw).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("failed to replace {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().join("state.json"));
        assert!(store.state().recent.is_empty());
    }

    #[test]
    fn touch_persists_and_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut store = StateStore::load(&path);
        store.touch(Path::new("/projects/a")).unwrap();
        store.touch(Path::new("/projects/b")).unwrap();
        store.touch(Path::new("/projects/a")).unwrap();

        let reloaded = StateStore::load(&path);
        assert_eq!(reloaded.state().recent.len(), 2);
        assert_eq!(
            reloaded.state().recent[0].path,
            PathBuf::from("/projects/a")
        );
        assert_eq!(
            reloaded.state().last_project.as_deref(),
            Some(Path::new("/projects/a"))
        );
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "{oops").unwrap();
        let store = StateStore::load(&path);
        assert!(store.state().recent.is_empty());
    }

    #[test]
    fn hidden_projects_persist_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut store = StateStore::load(&path);
        store.touch(Path::new("/projects/a")).unwrap();
        store
            .set_project_hidden(Path::new("/projects/a"), true)
            .unwrap();

        let reloaded = StateStore::load(&path);
        assert!(reloaded.is_hidden(Path::new("/projects/a")));
        assert_eq!(reloaded.state().hidden_projects.len(), 1);

        let mut reloaded = reloaded;
        reloaded.forget_project(Path::new("/projects/a")).unwrap();
        assert!(!reloaded.is_hidden(Path::new("/projects/a")));
        assert!(reloaded.state().last_project.is_none());
        assert!(reloaded.state().recent.is_empty());
    }

    #[test]
    fn hiding_is_idempotent() {
        let mut store = StateStore::load(tempfile::tempdir().unwrap().path().join("state.json"));
        store
            .set_project_hidden(Path::new("/projects/a"), true)
            .unwrap();
        store
            .set_project_hidden(Path::new("/projects/a"), true)
            .unwrap();
        assert_eq!(store.state().hidden_projects.len(), 1);
        store
            .set_project_hidden(Path::new("/projects/a"), false)
            .unwrap();
        assert!(store.state().hidden_projects.is_empty());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut store = StateStore::load(&path);
        store.touch(Path::new("/projects/a")).unwrap();

        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "the temporary file must be renamed away");
        let raw = fs::read_to_string(&path).unwrap();
        serde_json::from_str::<crate::session::state::PersistedState>(&raw).unwrap();
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/state.json");
        let mut store = StateStore::load(&path);
        store.touch(Path::new("/projects/a")).unwrap();
        assert!(path.is_file());
        assert_eq!(
            StateStore::load(&path).state().recent[0].path,
            PathBuf::from("/projects/a")
        );
    }

    #[test]
    fn partially_valid_json_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, br#"{"last_project":"/projects/a"}"#).unwrap();
        let store = StateStore::load(&path);
        assert!(store.state().recent.is_empty());
        assert_eq!(
            store.state().last_project.as_deref(),
            Some(Path::new("/projects/a"))
        );
        assert_eq!(store.state().version, 1);
    }

    #[test]
    fn wrong_types_are_ignored_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, br#"{"recent":"not a list"}"#).unwrap();
        let store = StateStore::load(&path);
        assert!(store.state().recent.is_empty());
        assert_eq!(store.state().version, 1);
    }

    #[test]
    fn forget_only_removes_the_named_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut store = StateStore::load(&path);
        store.touch(Path::new("/projects/a")).unwrap();
        store.touch(Path::new("/projects/b")).unwrap();
        store
            .set_project_hidden(Path::new("/projects/b"), true)
            .unwrap();

        store.forget_project(Path::new("/projects/b")).unwrap();

        assert_eq!(store.state().recent.len(), 1);
        assert_eq!(store.state().recent[0].path, PathBuf::from("/projects/a"));
        // Forgetting the most recent project clears the selection instead of
        // silently selecting an older project.
        assert_eq!(store.state().last_project, None);
        assert!(!store.is_hidden(Path::new("/projects/b")));
        assert_eq!(StateStore::load(&path).state().recent.len(), 1);
    }
}
