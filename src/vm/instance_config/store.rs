//! Filesystem-only store for instance configuration files.
//!
//! The store owns the `instances_dir` root. Every operation re-validates the
//! instance name before it becomes a path component, and configuration files
//! are written privately (`0600`, no symlink following).

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Config;

use super::clone::create_private_dir;
use super::model::{CONFIG_FILE, InstanceConfig, validate_name};

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
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        Ok(())
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
}
