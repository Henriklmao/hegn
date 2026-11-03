//! Snapshot metadata model and name validation.

use std::path::PathBuf;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub vm_state: String,
    /// Directory passed to Cloud Hypervisor as `file://<dir>`.
    pub dir: PathBuf,
    /// Instance that owns this snapshot. Optional for older metadata.
    #[serde(default, alias = "sandbox")]
    pub instance: Option<String>,
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub note: Option<String>,
}

impl SnapshotMeta {
    pub fn size_label(&self) -> String {
        human_size(self.size_bytes)
    }

    /// Cloud Hypervisor's required files. Snapshot directories from other VMM
    /// formats are intentionally not accepted.
    pub fn is_restorable(&self) -> bool {
        self.dir.is_dir()
            && self.dir.join("config.json").is_file()
            && self.dir.join("state.json").is_file()
            && self.dir.join("memory-ranges").is_file()
    }

    pub fn source_url(&self) -> String {
        format!("file://{}", self.dir.display())
    }
}

use crate::shared::text::human_bytes as human_size;

/// Paths reserved for a new snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPaths {
    /// Directory the VMM writes into (a staging directory in production).
    pub dir: PathBuf,
    /// Final private snapshot directory once `finalize` has moved it.
    pub final_dir: PathBuf,
    pub meta_path: PathBuf,
}

/// Validate a snapshot name. Names become directory names, so traversal and
/// hidden entries are rejected.
pub fn sanitize_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("snapshot name cannot be empty");
    }
    if name.len() > 100 {
        bail!("snapshot name is too long");
    }
    if name.starts_with('.') || name.contains('/') || name.contains('\\') {
        bail!("snapshot name must not contain path separators or start with '.'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("snapshot name may only contain ASCII letters, digits, '-', '_' and '.'");
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_url_is_a_file_url() {
        let meta = SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: PathBuf::from("/snapshots/demo"),
            instance: None,
            project: None,
            size_bytes: 0,
            note: None,
        };
        assert_eq!(meta.source_url(), "file:///snapshots/demo");
    }

    #[test]
    fn a_missing_snapshot_is_not_restorable() {
        let dir = tempfile::tempdir().unwrap();
        let meta = SnapshotMeta {
            name: "missing".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.path().join("missing"),
            instance: None,
            project: None,
            size_bytes: 0,
            note: None,
        };
        assert!(!meta.is_restorable());
    }

    #[test]
    fn human_sizes_are_readable() {
        assert_eq!(human_size(12), "12 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn snapshot_names_reject_traversal_absolute_and_hidden_entries() {
        for bad in [
            "",
            "   ",
            ".",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            ".hidden",
            "/absolute",
            "with space",
            "ümlaut",
        ] {
            assert!(sanitize_name(bad).is_err(), "{bad:?}");
        }
        assert!(sanitize_name(&"a".repeat(101)).is_err());
        assert!(sanitize_name(&"a".repeat(100)).is_ok());
    }

    #[test]
    fn snapshot_names_are_trimmed_and_keep_inner_dots() {
        assert_eq!(sanitize_name("  demo.v1 ").unwrap(), "demo.v1");
        assert_eq!(
            sanitize_name("2026-01-02_03.04").unwrap(),
            "2026-01-02_03.04"
        );
    }
}
