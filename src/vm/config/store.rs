//! Filesystem-only store for `vm.toml` files.
//!
//! The store owns `vms_dir`. Every operation validates the VM name before it
//! becomes a path component, and configuration files are written privately
//! (`0600`, no symlink following, atomic rename).
//!
//! Workspace and runtime directories are exposed as path helpers only. Deleting
//! VMs or their workspaces belongs to a later phase.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::config::Config;
use crate::shared::fs::create_private_dir;

use super::model::{VM_CONFIG_FILE, VmConfig, validate_name};

/// Filesystem-only store for VM configuration.
#[derive(Debug, Clone)]
pub struct VmStore {
    root: PathBuf,
}

impl VmStore {
    pub fn new(config: &Config) -> Self {
        Self {
            root: config.vms_dir.clone(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create `vms_dir` as a private (`0700`) directory, refusing a symlink.
    pub fn ensure_root(&self) -> Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(&self.root)
            && metadata.file_type().is_symlink()
        {
            anyhow::bail!("refusing symlinked VMs directory {}", self.root.display());
        }
        create_private_dir(&self.root)
            .with_context(|| format!("failed to secure {}", self.root.display()))
    }

    /// Directory holding one VM's config, runtime and workspaces.
    pub fn dir(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }

    pub fn config_path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.dir(name)?.join(VM_CONFIG_FILE))
    }

    /// Runtime sockets, pids and locks for a VM.
    pub fn run_dir(&self, name: &str) -> Result<PathBuf> {
        Ok(self.dir(name)?.join("run"))
    }

    /// Root for hegn-managed clone workspaces of a VM.
    pub fn workspaces_dir(&self, name: &str) -> Result<PathBuf> {
        Ok(self.dir(name)?.join("workspaces"))
    }

    pub fn load(&self, name: &str) -> Result<VmConfig> {
        let dir = self.dir(name)?;
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("refusing symlinked VM directory {}", dir.display());
            }
            Ok(metadata) if !metadata.is_dir() => {
                anyhow::bail!("VM path {} is not a directory", dir.display());
            }
            Ok(_) => {}
            // Missing directories fall through to the file read, which reports
            // the missing vm.toml with the usual user-facing message.
            Err(_) => {}
        }
        let path = dir.join(VM_CONFIG_FILE);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("refusing symlinked VM config {}", path.display());
            }
            Ok(metadata) if !metadata.is_file() => {
                anyhow::bail!("VM config {} is not a regular file", path.display());
            }
            Ok(_) => {}
            Err(_) => {}
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("VM '{name}' has no config at {}", path.display()))?;
        let config: VmConfig =
            toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
        config.validate()?;
        if config.name != name {
            anyhow::bail!(
                "VM config {} names '{}' but the directory is '{name}'",
                path.display(),
                config.name
            );
        }
        Ok(config)
    }

    /// Write `config` atomically as a private file.
    pub fn save(&self, config: &VmConfig) -> Result<()> {
        config.validate()?;
        self.ensure_root()?;
        let dir = self.dir(&config.name)?;
        let existed = dir.is_dir();
        create_private_dir(&dir)?;
        if !existed {
            sync_directory(&self.root)?;
        }
        let path = dir.join(VM_CONFIG_FILE);
        let raw = toml::to_string_pretty(config)?;
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let tmp = dir.join(format!(
            ".{VM_CONFIG_FILE}.tmp-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_file(&tmp);
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&tmp)
                .with_context(|| format!("failed to write {}", tmp.display()))?;
            file.write_all(raw.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, &path)
                .with_context(|| format!("failed to replace {}", path.display()))?;
            sync_directory(&dir)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Load every valid VM, skipping foreign, broken and symlinked entries.
    pub fn list(&self) -> Result<Vec<VmConfig>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.root.display()));
            }
        };
        let mut vms = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if validate_name(&name).is_err() {
                continue;
            }
            let path = entry.path();
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                _ => continue,
            }
            match fs::symlink_metadata(path.join(VM_CONFIG_FILE)) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
                _ => continue,
            }
            if let Ok(config) = self.load(&name) {
                vms.push(config);
            }
        }
        vms.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(vms)
    }
}

/// Persist a directory entry after creating, renaming or removing a child.
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|handle| handle.sync_all())
        .with_context(|| format!("failed to sync {}", path.display()))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.vms_dir = dir.join("vms");
        config
    }

    fn vm(name: &str) -> VmConfig {
        VmConfig {
            name: name.into(),
            ..VmConfig::default()
        }
    }

    #[test]
    fn config_file_is_private_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        let mut config = vm("rust");
        config.memory_mib = 16_384;
        config.primary = Some("parser".into());
        config.rootfs_image = Some(dir.path().join("rust.qcow2"));
        config.network_index = Some(3);
        store.save(&config).unwrap();

        let path = store.config_path("rust").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(store.dir("rust").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let loaded = store.load("rust").unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn absent_options_are_serialized_compactly() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        store.save(&vm("rust")).unwrap();
        let raw = fs::read_to_string(store.config_path("rust").unwrap()).unwrap();
        assert!(!raw.contains("rootfs_image"), "{raw}");
        assert!(!raw.contains("primary"), "{raw}");
        assert!(!raw.contains("network_index"), "{raw}");
        assert!(raw.contains("version = 2"), "{raw}");
    }

    #[test]
    fn store_root_is_private_and_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        store.ensure_root().unwrap();
        assert_eq!(
            fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let target = dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let link = dir.path().join("linked-vms");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut cfg = Config::default();
        cfg.vms_dir = link;
        let linked = VmStore::new(&cfg);
        let err = linked.ensure_root().unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn save_replaces_a_symlinked_config_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        let config = vm("rust");
        store.save(&config).unwrap();
        let path = store.config_path("rust").unwrap();
        fs::remove_file(&path).unwrap();
        let outside = dir.path().join("outside.toml");
        fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();

        store.save(&config).unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        assert!(!path.is_symlink());
        assert_eq!(store.load("rust").unwrap().name, "rust");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn save_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        store.save(&vm("rust")).unwrap();
        let entries: Vec<String> = fs::read_dir(store.dir("rust").unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![VM_CONFIG_FILE.to_string()]);
    }

    #[test]
    fn save_cleans_up_after_a_failed_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        let vm_dir = store.dir("rust").unwrap();
        fs::create_dir_all(vm_dir.join(VM_CONFIG_FILE)).unwrap();

        assert!(store.save(&vm("rust")).is_err());
        let leftovers: Vec<String> = fs::read_dir(&vm_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn list_skips_foreign_broken_and_symlinked_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        store.save(&vm("one")).unwrap();
        let raw = fs::read_to_string(store.config_path("one").unwrap()).unwrap();
        store.ensure_root().unwrap();

        fs::create_dir_all(store.root().join("no-config")).unwrap();
        fs::write(store.root().join("just-a-file"), b"x").unwrap();
        std::os::unix::fs::symlink(store.root().join("one"), store.root().join("linked")).unwrap();
        let invalid_name = store.root().join("bad name");
        fs::create_dir_all(&invalid_name).unwrap();
        fs::write(invalid_name.join(VM_CONFIG_FILE), &raw).unwrap();
        let broken = store.root().join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(VM_CONFIG_FILE), b"{not toml").unwrap();
        let mismatch = store.root().join("mismatch");
        fs::create_dir_all(&mismatch).unwrap();
        fs::write(mismatch.join(VM_CONFIG_FILE), &raw).unwrap();
        let symlinked_config = store.root().join("symlinked");
        fs::create_dir_all(&symlinked_config).unwrap();
        std::os::unix::fs::symlink(
            store.config_path("one").unwrap(),
            symlinked_config.join(VM_CONFIG_FILE),
        )
        .unwrap();

        let names: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|vm| vm.name)
            .collect();
        assert_eq!(names, vec!["one"]);
    }

    #[test]
    fn list_is_empty_when_the_root_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        assert!(!store.root().exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn load_refuses_symlinked_vm_directories_and_configs() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        store.save(&vm("one")).unwrap();

        std::os::unix::fs::symlink(store.dir("one").unwrap(), store.dir("linked").unwrap())
            .unwrap();
        let err = store.load("linked").unwrap_err().to_string();
        assert!(err.contains("symlinked VM directory"), "{err}");

        let linked_config = store.dir("linked-config").unwrap();
        fs::create_dir_all(&linked_config).unwrap();
        std::os::unix::fs::symlink(
            store.config_path("one").unwrap(),
            linked_config.join(VM_CONFIG_FILE),
        )
        .unwrap();
        let err = store.load("linked-config").unwrap_err().to_string();
        assert!(err.contains("symlinked VM config"), "{err}");
    }

    #[test]
    fn load_reports_missing_mismatched_and_invalid_configs() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        fs::create_dir_all(store.dir("demo").unwrap()).unwrap();
        let err = store.load("demo").unwrap_err().to_string();
        assert!(err.contains("has no config"), "{err}");

        store.save(&vm("one")).unwrap();
        let raw = fs::read_to_string(store.config_path("one").unwrap()).unwrap();
        fs::create_dir_all(store.dir("demo").unwrap()).unwrap();
        fs::write(store.config_path("demo").unwrap(), &raw).unwrap();
        let err = store.load("demo").unwrap_err().to_string();
        assert!(err.contains("names 'one'"), "{err}");

        fs::write(
            store.config_path("demo").unwrap(),
            "version = 2\nname = \"demo\"\nvcpus = 0\n",
        )
        .unwrap();
        assert!(store.load("demo").is_err(), "validation applies on load");
    }

    #[test]
    fn dir_paths_reject_unsafe_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::new(&config(dir.path()));
        for name in ["../escape", "a/b", "", ".hidden"] {
            assert!(store.dir(name).is_err(), "{name:?}");
            assert!(store.config_path(name).is_err(), "{name:?}");
            assert!(store.run_dir(name).is_err(), "{name:?}");
            assert!(store.workspaces_dir(name).is_err(), "{name:?}");
        }
        assert_eq!(store.dir("rust").unwrap(), store.root().join("rust"));
        assert_eq!(
            store.config_path("rust").unwrap(),
            store.root().join("rust/vm.toml")
        );
        assert_eq!(
            store.run_dir("rust").unwrap(),
            store.root().join("rust/run")
        );
        assert_eq!(
            store.workspaces_dir("rust").unwrap(),
            store.root().join("rust/workspaces")
        );
    }
}
