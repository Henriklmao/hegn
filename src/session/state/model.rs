//! Persisted state model: recent projects, last selection and hidden projects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::state::StateStore;

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
}
