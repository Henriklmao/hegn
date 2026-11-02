//! Small persistent UI state (recent projects, last selection, hidden projects).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const MAX_RECENT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub path: PathBuf,
    pub opened_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default = "state_version")]
    pub version: u32,
    #[serde(default)]
    pub recent: Vec<RecentEntry>,
    #[serde(default)]
    pub last_project: Option<PathBuf>,
    /// Projects removed from the picker while keeping their directory on disk.
    #[serde(default)]
    pub hidden_projects: Vec<PathBuf>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: state_version(),
            recent: Vec::new(),
            last_project: None,
            hidden_projects: Vec::new(),
        }
    }
}

fn state_version() -> u32 {
    1
}

impl PersistedState {
    pub fn touch(&mut self, path: &Path) {
        self.recent.retain(|entry| entry.path != path);
        self.recent.insert(
            0,
            RecentEntry {
                path: path.to_path_buf(),
                opened_at: Utc::now(),
            },
        );
        self.recent.truncate(MAX_RECENT);
        self.last_project = Some(path.to_path_buf());
    }

    pub fn opened_at(&self) -> HashMap<PathBuf, DateTime<Utc>> {
        self.recent
            .iter()
            .map(|entry| (entry.path.clone(), entry.opened_at))
            .collect()
    }

    pub fn is_hidden(&self, path: &Path) -> bool {
        self.hidden_projects.iter().any(|hidden| hidden == path)
    }

    pub fn hide_project(&mut self, path: &Path) {
        if !self.is_hidden(path) {
            self.hidden_projects.push(path.to_path_buf());
        }
    }

    pub fn unhide_project(&mut self, path: &Path) {
        self.hidden_projects.retain(|hidden| hidden != path);
    }

    /// Forget a project that no longer exists: it can neither be recent nor
    /// hidden from the picker.
    pub fn forget_project(&mut self, path: &Path) {
        self.recent.retain(|entry| entry.path != path);
        if self.last_project.as_deref() == Some(path) {
            self.last_project = None;
        }
        self.unhide_project(path);
    }
}

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
    fn default_state_uses_current_version() {
        assert_eq!(PersistedState::default().version, state_version());
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().join("state.json"));
        assert_eq!(store.state().version, state_version());
    }

    #[test]
    fn recent_is_capped() {
        let mut state = PersistedState::default();
        for i in 0..(MAX_RECENT + 20) {
            state.touch(Path::new(&format!("/p/{i}")));
        }
        assert_eq!(state.recent.len(), MAX_RECENT);
    }

    #[test]
    fn opened_at_map_matches_recent() {
        let mut state = PersistedState::default();
        state.touch(Path::new("/p/a"));
        let map = state.opened_at();
        assert!(map.contains_key(Path::new("/p/a")));
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
}
