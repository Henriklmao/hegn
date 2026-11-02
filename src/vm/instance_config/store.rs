//! Filesystem-only store for instance configuration files.
//!
//! The store owns the `instances_dir` root. Every operation re-validates the
//! instance name before it becomes a path component, and configuration files
//! are written privately (`0600`, no symlink following).

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::config::Config;

use super::model::{CONFIG_FILE, InstanceConfig, validate_name};
use crate::shared::fs::create_private_dir;

/// Filesystem-only store for instance configuration and cloned workspaces.
#[derive(Debug, Clone)]
pub struct InstanceStore {
    pub(super) root: PathBuf,
    pub(super) defaults: Config,
}

impl InstanceStore {
    pub fn new(config: Config) -> Self {
        Self {
            root: config.instances_dir.clone(),
            defaults: config,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_root(&self) -> Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(&self.root)
            && metadata.file_type().is_symlink()
        {
            bail!(
                "refusing symlinked instances directory {}",
                self.root.display()
            );
        }
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", self.root.display()))?;
        }
        Ok(())
    }

    pub fn dir(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }

    pub fn config_path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.dir(name)?.join(CONFIG_FILE))
    }

    pub fn load(&self, name: &str) -> Result<InstanceConfig> {
        let dir = self.dir(name)?;
        let path = dir.join(CONFIG_FILE);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("instance '{name}' has no config at {}", path.display()))?;
        let config: InstanceConfig =
            toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
        config.validate()?;
        if config.name != name {
            bail!(
                "instance config {} names '{}' but the directory is '{name}'",
                path.display(),
                config.name
            );
        }
        Ok(config)
    }

    pub fn save(&self, config: &InstanceConfig) -> Result<()> {
        config.validate()?;
        let dir = self.dir(&config.name)?;
        create_private_dir(&dir)?;
        let path = dir.join(CONFIG_FILE);
        let raw = toml::to_string_pretty(config)?;
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let tmp = dir.join(format!(
            ".{CONFIG_FILE}.tmp-{}-{unique}",
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
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    pub fn list(&self) -> Result<Vec<InstanceConfig>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.root.display()));
            }
        };
        let mut instances = Vec::new();
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
            if !path.join(CONFIG_FILE).is_file() {
                continue;
            }
            if let Ok(config) = self.load(&name) {
                instances.push(config);
            }
        }
        instances.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(instances)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::instance_config::test_support::{config, project};

    #[test]
    fn config_file_is_private_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        let mut instance = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        instance.memory_mib = 4096;
        store.save(&instance).unwrap();
        let path = store.config_path("demo").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(store.load("demo").unwrap().memory_mib, 4096);
    }

    #[test]
    fn save_and_load_preserve_optional_instance_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        let mut instance = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        instance.rootfs_image = Some(dir.path().join("private.qcow2"));
        instance.network_index = Some(3);
        instance.extra_packages = vec!["build-essential".into(), "python3.12".into()];
        instance.auto_branch = false;
        instance.security = crate::config::SecurityMode::Warn;
        store.save(&instance).unwrap();

        let loaded = store.load("demo").unwrap();
        assert_eq!(loaded.rootfs_image, instance.rootfs_image);
        assert_eq!(loaded.network_index, Some(3));
        assert_eq!(loaded.extra_packages, instance.extra_packages);
        assert!(!loaded.auto_branch);
        assert_eq!(loaded.security, crate::config::SecurityMode::Warn);
        assert_eq!(loaded.project, instance.project);
        assert_eq!(loaded.workspace, instance.workspace);
    }

    #[test]
    fn instance_directory_and_config_file_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        store
            .create_from_project(&project, None, None, false)
            .unwrap();

        let instance_dir = store.dir("demo").unwrap();
        assert_eq!(
            fs::metadata(&instance_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.config_path("demo").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store.ensure_root().unwrap();
        assert_eq!(
            fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn list_ignores_foreign_broken_and_symlinked_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        store
            .create_from_project(&project, Some("one"), None, false)
            .unwrap();
        let raw = fs::read_to_string(store.config_path("one").unwrap()).unwrap();

        fs::create_dir_all(store.root().join("no-config")).unwrap();
        fs::write(store.root().join("just-a-file"), b"x").unwrap();
        std::os::unix::fs::symlink(store.root().join("one"), store.root().join("linked")).unwrap();
        let invalid_name = store.root().join("bad name");
        fs::create_dir_all(&invalid_name).unwrap();
        fs::write(invalid_name.join(CONFIG_FILE), &raw).unwrap();
        let broken = store.root().join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(CONFIG_FILE), b"{not toml").unwrap();
        let mismatch = store.root().join("mismatch");
        fs::create_dir_all(&mismatch).unwrap();
        fs::write(mismatch.join(CONFIG_FILE), &raw).unwrap();

        let names: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|instance| instance.name)
            .collect();
        assert_eq!(names, vec!["one"]);
    }

    #[test]
    fn list_is_empty_when_the_root_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        assert!(!store.root().exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn load_reports_a_missing_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        fs::create_dir_all(store.dir("demo").unwrap()).unwrap();
        let err = store.load("demo").unwrap_err().to_string();
        assert!(err.contains("has no config"), "{err}");
    }

    #[test]
    fn load_rejects_a_config_whose_name_disagrees_with_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        store
            .create_from_project(&project, Some("one"), None, false)
            .unwrap();
        let raw = fs::read_to_string(store.config_path("one").unwrap()).unwrap();
        fs::create_dir_all(store.dir("demo").unwrap()).unwrap();
        fs::write(store.config_path("demo").unwrap(), raw).unwrap();

        let err = store.load("demo").unwrap_err().to_string();
        assert!(err.contains("names 'one'"), "{err}");
    }

    #[test]
    fn ensure_root_refuses_a_symlinked_instances_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let root = dir.path().join("instances");
        std::os::unix::fs::symlink(&target, &root).unwrap();
        let store = InstanceStore::new(config(dir.path()));

        let err = store.ensure_root().unwrap_err().to_string();
        assert!(err.contains("symlinked instances directory"), "{err}");
    }

    #[test]
    fn save_replaces_a_symlinked_config_file_without_following_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        let instance = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        let config_path = store.config_path("demo").unwrap();
        fs::remove_file(&config_path).unwrap();
        let outside = dir.path().join("outside.toml");
        fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, &config_path).unwrap();

        store.save(&instance).unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        assert!(!config_path.is_symlink());
        assert_eq!(store.load("demo").unwrap().name, "demo");
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn dir_rejects_unsafe_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(config(dir.path()));
        for name in ["../escape", "a/b", "", ".hidden"] {
            assert!(store.dir(name).is_err(), "{name:?}");
        }
        assert_eq!(store.dir("demo").unwrap(), store.root().join("demo"));
    }
}
