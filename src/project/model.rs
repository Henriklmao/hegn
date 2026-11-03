//! Project data model: git summaries, picker entries, per-project metadata
//! and delete outcomes.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::git::read_git_summary;

/// Snapshot of `git status` for a project directory.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitSummary {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
    pub ahead: usize,
    pub behind: usize,
    pub error: Option<String>,
}

impl GitSummary {
    pub fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.untracked > 0
    }

    /// Short one-line badge for the project list.
    pub fn badge(&self) -> String {
        if !self.is_repo {
            return "not a repo".to_string();
        }
        let branch = self.branch.as_deref().unwrap_or("detached");
        let mut out = branch.to_string();
        if self.is_dirty() {
            out.push('*');
        }
        if self.ahead > 0 {
            out.push_str(&format!(" ↑{}", self.ahead));
        }
        if self.behind > 0 {
            out.push_str(&format!(" ↓{}", self.behind));
        }
        out
    }
}

/// A project shown in the picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub git: GitSummary,
    pub last_opened: Option<DateTime<Utc>>,
}

impl Project {
    pub fn load(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            name,
            git: read_git_summary(&path),
            path,
            last_opened: None,
        }
    }

    pub fn display_path(&self) -> String {
        let home = dirs::home_dir();
        if let Some(home) = home
            && let Ok(rest) = self.path.strip_prefix(&home)
        {
            return format!("~/{}", rest.display());
        }
        self.path.display().to_string()
    }
}

/// Optional per-project hegn settings stored in `hegn.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectMeta {
    pub harness: Option<crate::config::Harness>,
    pub packages: Vec<String>,
    pub template: Option<String>,
    /// Launch command override for this project (`harness`, `shell`, or a
    /// command name from the global `[launch]` table).
    pub launch: Option<String>,
    /// Terminal integration override for this project.
    pub integration: Option<crate::agent::launch::Integration>,
    /// Whether starting this project's VM also launches the configured
    /// command. Overrides the global `launch.auto_start`.
    pub auto_start: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDeleteOutcome {
    /// True when a real project directory was deleted. Import links and
    /// kept directories leave the original files on disk.
    pub files_deleted: bool,
    /// Instances that were stopped and removed with the project.
    pub instances: Vec<String>,
}
