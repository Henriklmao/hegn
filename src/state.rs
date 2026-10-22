//! Small persistent UI state (recent projects, last selection).

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default = "state_version")]
    pub version: u32,
    #[serde(default)]
    pub recent: Vec<RecentEntry>,
    #[serde(default)]
    pub last_project: Option<PathBuf>,
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
            .unwrap_or_else(|| PersistedState {
                version: state_version(),
                ..PersistedState::default()
            });
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
}
