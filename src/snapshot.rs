//! On-disk snapshot metadata.
//!
//! A snapshot is a directory written by Cloud Hypervisor:
//!
//! ```text
//! ~/.local/share/hegn/snapshots/
//!   .staging/<name>/   # the only path the VMM may write
//!   <name>/
//!     snapshot.json    # hegn metadata rendered by the TUI
//!     config.json      # Cloud Hypervisor VM configuration
//!     state.json       # device and CPU state
//!     memory-ranges    # guest memory
//! ```
//!
//! Finished snapshots are renamed out of `.staging` before metadata is saved,
//! so the VMM's Landlock rule never covers completed checkpoints.

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
    /// Directory passed to Cloud Hypervisor as `file://<dir>`.
    pub dir: PathBuf,
    /// Sandbox that owns this snapshot. Optional for older metadata.
    #[serde(default)]
    pub sandbox: Option<String>,
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
    /// Directory the VMM writes into (a staging directory in production).
    pub dir: PathBuf,
    /// Final private snapshot directory once `finalize` has moved it.
    pub final_dir: PathBuf,
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

    /// Directory the VMM is allowed to write snapshots into.
    ///
    /// Keeping this separate from the snapshot root means the VMM never has
    /// write access to existing checkpoints. The `0700` `.staging` name cannot
    /// collide with a user snapshot because `sanitize_name` rejects a leading
    /// dot.
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(".staging")
    }

    /// Create the staging directory with mode `0700`.
    pub fn ensure_staging(&self) -> Result<PathBuf> {
        let staging = self.staging_dir();
        create_private_dir(&staging)
            .with_context(|| format!("failed to create {}", staging.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", staging.display()))?;
        }
        Ok(staging)
    }

    /// Generate a timestamped name such as `hegn-20261022-141500`.
    pub fn suggested_name(prefix: &str) -> String {
        let prefix = sanitize_name(prefix).unwrap_or_else(|_| "snapshot".to_string());
        // Leave room for the dash and the 15-character timestamp so the result
        // still passes `sanitize_name`.
        let prefix = &prefix[..prefix.len().min(80)];
        format!("{prefix}-{}", Utc::now().format("%Y%m%d-%H%M%S"))
    }

    /// Reserve a directory and return the paths Cloud Hypervisor writes to.
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
            meta_path: dir.join("snapshot.json"),
            final_dir: dir.clone(),
            dir,
        })
    }

    /// Reserve a staging directory under `<root>/.staging`.
    ///
    /// Cloud Hypervisor writes here; the finished directory is atomically
    /// renamed into the snapshot root by [`Self::finalize`]. This keeps the
    /// VMM's Landlock write rule away from completed checkpoints.
    pub fn prepare_staged(&self, name: &str) -> Result<SnapshotPaths> {
        let name = sanitize_name(name)?;
        let final_dir = self.root.join(&name);
        if final_dir.exists() {
            bail!("snapshot '{name}' already exists");
        }
        let staging = self.ensure_staging()?;
        let dir = staging.join(&name);
        if dir.exists() {
            bail!("snapshot '{name}' is already being created");
        }
        create_private_dir(&dir)
            .with_context(|| format!("failed to create staging directory {}", dir.display()))?;
        let dir = dir
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", dir.display()))?;
        Ok(SnapshotPaths {
            meta_path: final_dir.join("snapshot.json"),
            final_dir,
            dir,
        })
    }

    /// Atomically move a finished staging directory into the snapshot root.
    pub fn finalize(&self, paths: &SnapshotPaths) -> Result<()> {
        create_private_dir(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        fs::rename(&paths.dir, &paths.final_dir).with_context(|| {
            format!(
                "failed to move snapshot {} to {}",
                paths.dir.display(),
                paths.final_dir.display()
            )
        })
    }

    /// Persist metadata after Cloud Hypervisor has written the snapshot files.
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
            meta.size_bytes = dir_size(&meta.dir);
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
                    meta.size_bytes = dir_size(&meta.dir);
                }
                snapshots.push(meta);
            }
        }
        snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.created_at));
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

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::symlink_metadata(entry.path()).ok())
        .filter(|meta| meta.file_type().is_file())
        .map(|meta| meta.len())
        .sum()
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
            dir: paths.final_dir.clone(),
            sandbox: Some("demo".into()),
            project: Some(PathBuf::from("/workspace/demo")),
            size_bytes: 0,
            note: None,
        }
    }

    fn write_snapshot_files(paths: &SnapshotPaths) {
        fs::write(paths.dir.join("config.json"), b"{}").unwrap();
        fs::write(paths.dir.join("state.json"), vec![0u8; 10]).unwrap();
        fs::write(paths.dir.join("memory-ranges"), vec![0u8; 20]).unwrap();
    }

    #[test]
    fn prepares_saves_lists_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let paths = store.prepare("first").unwrap();
        write_snapshot_files(&paths);
        store.save_meta(&meta("first", &paths)).unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "first");
        assert!(list[0].size_bytes >= 32);
        assert!(list[0].is_restorable());

        store.delete("first").unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.delete("first").is_err());
    }

    #[test]
    fn stages_then_finalizes_a_snapshot() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let paths = store.prepare_staged("demo").unwrap();
        assert!(paths.dir.starts_with(store.staging_dir()));
        assert!(!paths.final_dir.exists());
        write_snapshot_files(&paths);
        store.finalize(&paths).unwrap();
        assert!(!paths.dir.exists());
        assert!(paths.final_dir.is_dir());
        assert_eq!(
            std::fs::metadata(store.staging_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let meta = meta("demo", &paths);
        store.save_meta(&meta).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].dir, paths.final_dir);
    }

    #[test]
    fn staged_names_cannot_collide_with_completed_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let paths = store.prepare_staged("demo").unwrap();
        store.finalize(&paths).unwrap();
        assert!(store.prepare_staged("demo").is_err());
        assert!(store.prepare_staged(".staging").is_err());
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
    fn suggested_names_fit_the_name_limit() {
        let long = "a".repeat(120);
        let name = SnapshotStore::suggested_name(&long);
        assert!(name.len() <= 100, "{}", name.len());
        sanitize_name(&name).unwrap();
        assert!(SnapshotStore::suggested_name("demo").starts_with("demo-"));
    }

    #[test]
    fn source_url_is_a_file_url() {
        let meta = SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: PathBuf::from("/snapshots/demo"),
            sandbox: None,
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
            sandbox: None,
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
}
