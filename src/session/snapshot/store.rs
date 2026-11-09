//! Snapshot store: private directory creation, staging, finalize and listing.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::model::{AUTOSAVE_NOTE, SnapshotMeta, SnapshotPaths, sanitize_name};
use crate::shared::fs::{create_private_dir, dir_size};

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

    /// Verify that loaded metadata points inside the snapshot root.
    ///
    /// Cloud Hypervisor receives the directory as `source_url=file://<dir>`
    /// and Landlock rules are derived from the same path, so a forged
    /// `snapshot.json` must not be able to point at an arbitrary host
    /// directory. A comma or equals sign would also be split as an option
    /// separator on the command line.
    fn contain_dir(&self, meta: &mut SnapshotMeta) -> Result<()> {
        let recorded = meta.dir.to_string_lossy();
        if recorded.contains(',') || recorded.contains('=') {
            bail!(
                "snapshot '{}' records a directory with unsafe characters: {}",
                meta.name,
                recorded
            );
        }
        let root = self
            .root
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", self.root.display()))?;
        let dir = meta
            .dir
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", meta.dir.display()))?;
        if !dir.starts_with(&root) {
            bail!(
                "snapshot '{}' points outside the snapshot root {}",
                meta.name,
                root.display()
            );
        }
        meta.dir = dir;
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
        self.contain_dir(&mut meta)?;
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

    /// Newest restorable automatic snapshot for `instance`.
    ///
    /// Only snapshots tagged with [`AUTOSAVE_NOTE`] are considered: a manual
    /// checkpoint must never be loaded just because hegn restarts.
    pub fn latest_autosave(&self, instance: &str) -> Result<Option<SnapshotMeta>> {
        Ok(self.list()?.into_iter().find(|snapshot| {
            snapshot.note.as_deref() == Some(AUTOSAVE_NOTE)
                && snapshot.instance.as_deref() == Some(instance)
                && snapshot.is_restorable()
        }))
    }

    /// Delete automatic snapshots beyond `keep` for one instance.
    ///
    /// Manual snapshots and other instances' checkpoints are never touched.
    /// Returns the removed names.
    pub fn prune_autosaves(&self, instance: &str, keep: u32) -> Result<Vec<String>> {
        let keep = keep.max(1) as usize;
        let mut removed = Vec::new();
        for snapshot in self
            .list()?
            .into_iter()
            .filter(|snapshot| {
                snapshot.note.as_deref() == Some(AUTOSAVE_NOTE)
                    && snapshot.instance.as_deref() == Some(instance)
            })
            .skip(keep)
        {
            self.delete(&snapshot.name)?;
            removed.push(snapshot.name);
        }
        Ok(removed)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, paths: &SnapshotPaths) -> SnapshotMeta {
        SnapshotMeta {
            name: name.into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: paths.final_dir.clone(),
            instance: Some("demo".into()),
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
    fn latest_autosave_ignores_manual_and_foreign_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let auto = store.prepare("autosave-1").unwrap();
        write_snapshot_files(&auto);
        let mut auto_meta = meta("autosave-1", &auto);
        auto_meta.note = Some(AUTOSAVE_NOTE.into());
        auto_meta.created_at = Utc::now() - chrono::Duration::seconds(10);
        store.save_meta(&auto_meta).unwrap();

        let manual = store.prepare("manual").unwrap();
        write_snapshot_files(&manual);
        let mut manual_meta = meta("manual", &manual);
        manual_meta.note = Some("before-upgrade".into());
        store.save_meta(&manual_meta).unwrap();

        let foreign = store.prepare("autosave-foreign").unwrap();
        write_snapshot_files(&foreign);
        let mut foreign_meta = meta("autosave-foreign", &foreign);
        foreign_meta.note = Some(AUTOSAVE_NOTE.into());
        foreign_meta.instance = Some("other".into());
        store.save_meta(&foreign_meta).unwrap();

        assert_eq!(
            store.latest_autosave("demo").unwrap().unwrap().name,
            "autosave-1"
        );
        assert!(store.latest_autosave("missing").unwrap().is_none());
    }

    #[test]
    fn pruning_keeps_the_newest_autosaves_per_instance() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        for (index, offset) in [("auto-old", 0), ("auto-new", 20)] {
            let paths = store.prepare(index).unwrap();
            write_snapshot_files(&paths);
            let mut m = meta(index, &paths);
            m.note = Some(AUTOSAVE_NOTE.into());
            m.created_at = Utc::now() + chrono::Duration::seconds(offset);
            store.save_meta(&m).unwrap();
        }
        let manual = store.prepare("manual").unwrap();
        write_snapshot_files(&manual);
        store.save_meta(&meta("manual", &manual)).unwrap();

        let removed = store.prune_autosaves("demo", 1).unwrap();
        assert_eq!(removed, vec!["auto-old"]);
        assert!(store.load("auto-new").is_ok());
        assert!(store.load("manual").is_ok());
        assert!(store.load("auto-old").is_err());
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
    fn staged_names_cannot_be_reserved_twice() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let first = store.prepare_staged("demo").unwrap();
        let err = store.prepare_staged("demo").unwrap_err().to_string();
        assert!(err.contains("already being created"), "{err}");
        assert!(first.dir.is_dir());
    }

    #[test]
    fn load_reports_corrupt_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        fs::create_dir(store.root().join("broken")).unwrap();
        fs::write(store.root().join("broken/snapshot.json"), "{not json").unwrap();
        let err = store.load("broken").unwrap_err().to_string();
        assert!(err.contains("failed to parse snapshot"), "{err}");
    }

    #[test]
    fn load_rejects_a_snapshot_directory_outside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let entry = store.root().join("escape");
        fs::create_dir_all(&entry).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let escaped = SnapshotMeta {
            name: "escape".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: outside.clone(),
            instance: Some("demo".into()),
            project: None,
            size_bytes: 0,
            note: None,
        };
        fs::write(
            entry.join("snapshot.json"),
            serde_json::to_string(&escaped).unwrap(),
        )
        .unwrap();

        let err = store.load("escape").unwrap_err().to_string();
        assert!(err.contains("outside"), "{err}");
        assert!(store.list().unwrap().is_empty());
        assert!(outside.is_dir(), "the target must never be touched");
    }

    #[test]
    fn load_rejects_a_snapshot_directory_with_option_separators() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let entry = store.root().join("commas");
        fs::create_dir_all(&entry).unwrap();
        let unsafe_meta = SnapshotMeta {
            name: "commas".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: store.root().join("a,b"),
            instance: Some("demo".into()),
            project: None,
            size_bytes: 0,
            note: None,
        };
        fs::write(
            entry.join("snapshot.json"),
            serde_json::to_string(&unsafe_meta).unwrap(),
        )
        .unwrap();

        let err = store.load("commas").unwrap_err().to_string();
        assert!(err.contains("unsafe"), "{err}");
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn save_meta_creates_a_private_snapshot_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let paths = SnapshotPaths {
            dir: store.root().join("direct"),
            final_dir: store.root().join("direct"),
            meta_path: store.root().join("direct/snapshot.json"),
        };
        store.save_meta(&meta("direct", &paths)).unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.dir), 0o700);
        assert_eq!(mode(&paths.meta_path), 0o600);
    }

    #[test]
    fn list_skips_files_and_the_staging_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        fs::create_dir_all(store.staging_dir()).unwrap();
        fs::write(store.root().join("notes.txt"), b"x").unwrap();
        fs::create_dir(dir.path().join("not-a-snapshot")).unwrap();

        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn delete_refuses_traversal_and_missing_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();

        assert!(store.delete("../outside").is_err());
        assert!(store.delete("/absolute").is_err());
        assert!(store.delete("missing").is_err());
        assert!(outside.is_dir());
    }

    #[test]
    fn suggested_names_fall_back_for_unsafe_prefixes() {
        for prefix in ["日本語", "..", "a/b", ""] {
            let name = SnapshotStore::suggested_name(prefix);
            assert!(name.starts_with("snapshot-"), "{prefix:?} -> {name}");
            sanitize_name(&name).unwrap();
        }
    }

    #[test]
    fn dir_size_counts_only_files_directly_in_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), b"12345").unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/state.json"), b"0123456789").unwrap();

        assert_eq!(dir_size(dir.path()), 5);
    }
}
