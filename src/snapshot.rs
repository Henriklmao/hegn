//! On-disk snapshot metadata.
//!
//! A snapshot is a directory:
//!
//! ```text
//! ~/.local/share/hegn/snapshots/<name>/
//!   snapshot.json   # metadata rendered by the TUI
//!   vm.snap         # Firecracker snapshot state
//!   vm.mem          # guest memory
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub vm_state: String,
    pub snapshot_path: PathBuf,
    pub mem_file_path: PathBuf,
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
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Paths reserved for a new snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPaths {
    pub dir: PathBuf,
    pub snapshot_path: PathBuf,
    pub mem_file_path: PathBuf,
    pub meta_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Generate a timestamped name such as `hegn-20261022-141500`.
    pub fn suggested_name(prefix: &str) -> String {
        let prefix = sanitize_name(prefix).unwrap_or_else(|_| "snapshot".to_string());
        format!("{prefix}-{}", Utc::now().format("%Y%m%d-%H%M%S"))
    }

    /// Reserve a directory and return the paths Firecracker should write to.
    ///
    /// Snapshot memory can contain credentials, so the per-snapshot directory
    /// is created with mode 0700 instead of inheriting the process umask.
    pub fn prepare(&self, name: &str) -> Result<SnapshotPaths> {
        let name = sanitize_name(name)?;
        let dir = self.root.join(&name);
        if dir.exists() {
            bail!("snapshot '{name}' already exists");
        }
        create_private_dir(&dir)
            .with_context(|| format!("failed to create snapshot directory {}", dir.display()))?;
        let dir = dir
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", dir.display()))?;
        Ok(SnapshotPaths {
            snapshot_path: dir.join("vm.snap"),
            mem_file_path: dir.join("vm.mem"),
            meta_path: dir.join("snapshot.json"),
            dir,
        })
    }

    /// Persist metadata after Firecracker has written the snapshot files.
    pub fn save_meta(&self, meta: &SnapshotMeta) -> Result<()> {
        let name = sanitize_name(&meta.name)?;
        let dir = self.root.join(name);
        create_private_dir(&dir)?;
        let path = dir.join("snapshot.json");
        let raw = serde_json::to_string_pretty(meta)?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
        // The containing directory is private; keep the metadata private too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to secure {}", path.display()))?;
        }
        Ok(())
    }

    /// Load one snapshot's metadata.
    pub fn load(&self, name: &str) -> Result<SnapshotMeta> {
        let name = sanitize_name(name)?;
        let path = self.root.join(name).join("snapshot.json");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read snapshot {}", path.display()))?;
        let mut meta: SnapshotMeta = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse snapshot {}", path.display()))?;
        if meta.size_bytes == 0 {
            meta.size_bytes = file_sizes(&meta.snapshot_path, &meta.mem_file_path);
        }
        Ok(meta)
    }

    /// List all snapshots, newest first. Invalid entries are skipped.
    pub fn list(&self) -> Result<Vec<SnapshotMeta>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut snapshots = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(mut meta) = self.load(&name) {
                if meta.size_bytes == 0 {
                    meta.size_bytes = file_sizes(&meta.snapshot_path, &meta.mem_file_path);
                }
                snapshots.push(meta);
            }
        }
        snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(snapshots)
    }

    /// Delete a snapshot directory. Refuses names that escape the root.
    pub fn delete(&self, name: &str) -> Result<()> {
        let name = sanitize_name(name)?;
        let dir = self.root.join(&name);
        if !dir.exists() {
            bail!("snapshot '{name}' does not exist");
        }
        fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to delete snapshot {}", dir.display()))?;
        Ok(())
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

fn file_sizes(snapshot: &Path, mem: &Path) -> u64 {
    let size = |path: &Path| fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    size(snapshot) + size(mem)
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

    fn meta(name: &str, paths: &SnapshotPaths) -> SnapshotMeta {
        SnapshotMeta {
            name: name.into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            snapshot_path: paths.snapshot_path.clone(),
            mem_file_path: paths.mem_file_path.clone(),
            project: Some(PathBuf::from("/workspace/demo")),
            size_bytes: 0,
            note: None,
        }
    }

    #[test]
    fn prepares_saves_lists_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let paths = store.prepare("first").unwrap();
        fs::write(&paths.snapshot_path, vec![0u8; 10]).unwrap();
        fs::write(&paths.mem_file_path, vec![0u8; 20]).unwrap();
        store.save_meta(&meta("first", &paths)).unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "first");
        assert_eq!(list[0].size_bytes, 30);

        store.delete("first").unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.delete("first").is_err());
    }

    #[test]
    fn sorts_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        for (name, offset) in [("old", 0), ("new", 10)] {
            let paths = store.prepare(name).unwrap();
            let mut m = meta(name, &paths);
            m.created_at = Utc::now() + chrono::Duration::seconds(offset);
            store.save_meta(&m).unwrap();
        }
        let list = store.list().unwrap();
        assert_eq!(list[0].name, "new");
        assert_eq!(list[1].name, "old");
    }

    #[test]
    fn snapshot_directories_and_metadata_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let paths = store.prepare("private").unwrap();
        store.save_meta(&meta("private", &paths)).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&paths.dir), 0o700);
            assert_eq!(mode(&paths.meta_path), 0o600);
        }
    }

    #[test]
    fn rejects_duplicate_and_unsafe_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        store.prepare("one").unwrap();
        assert!(store.prepare("one").is_err());
        for bad in ["", "../escape", ".hidden", "a/b", "a b"] {
            assert!(store.prepare(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn skips_corrupt_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        fs::create_dir(dir.path().join("broken")).unwrap();
        fs::write(dir.path().join("broken/snapshot.json"), "{not json").unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn human_sizes_are_readable() {
        assert_eq!(human_size(12), "12 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
    }
}
