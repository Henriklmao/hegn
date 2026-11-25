//! Persisted per-VM configuration (`vm.toml`) and its validation.
//!
//! A [`VmConfig`] is the tool profile and runtime of one named VM. Projects
//! are deliberately *not* listed here: membership lives in the private project
//! registry, so the project stays the primary object and there is only one
//! source of truth.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Harness, SecurityMode};

pub const VM_CONFIG_FILE: &str = "vm.toml";
pub const CONFIG_VERSION: u32 = 2;
const MAX_NAME_BYTES: usize = 64;

/// Description of one VM: harness, resources and runtime policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VmConfig {
    pub version: u32,
    /// VM name; also the directory name below `vms_dir`.
    pub name: String,
    /// Shared tool profile installed into this VM.
    pub harness: Harness,
    pub vcpus: u8,
    pub memory_mib: u32,
    /// Additional apt packages on top of the harness preset.
    pub extra_packages: Vec<String>,
    /// Optional shared root filesystem. `None` means a private copy of the
    /// base image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rootfs_image: Option<PathBuf>,
    /// Picker name of the project that drives `/workspace`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    pub security: SecurityMode,
    pub periodic_snapshots: bool,
    pub snapshot_interval_secs: u64,
    pub snapshot_keep: u32,
    pub auto_resume: bool,
    /// Stable slice of the guarded-network TAP pool, kept across restores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_index: Option<u8>,
    pub created_at: DateTime<Utc>,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            name: String::new(),
            harness: Harness::default(),
            vcpus: 2,
            memory_mib: 2048,
            extra_packages: Vec::new(),
            rootfs_image: None,
            primary: None,
            security: SecurityMode::Enforce,
            periodic_snapshots: true,
            snapshot_interval_secs: 300,
            snapshot_keep: 2,
            auto_resume: true,
            network_index: None,
            created_at: Utc::now(),
        }
    }
}

impl VmConfig {
    /// Build a v2 VM config seeded from the global defaults.
    ///
    /// The share tag is intentionally absent: v2 derives one tag per exported
    /// project from `share_tag_prefix` at boot time.
    pub fn from_config(config: &Config, name: &str, harness: Option<Harness>) -> Self {
        Self {
            name: name.to_string(),
            harness: harness.unwrap_or(config.harness),
            vcpus: config.vcpus,
            memory_mib: config.memory_mib,
            extra_packages: config.extra_packages.clone(),
            security: config.security,
            periodic_snapshots: config.periodic_snapshots,
            snapshot_interval_secs: config.snapshot_interval_secs,
            snapshot_keep: config.snapshot_keep,
            auto_resume: config.auto_resume,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported VM config version {} (expected {CONFIG_VERSION})",
                self.version
            );
        }
        if self.vcpus == 0 || self.vcpus > 32 {
            bail!("VM vcpus must be between 1 and 32 (got {})", self.vcpus);
        }
        if !(128..=262_144).contains(&self.memory_mib) {
            bail!(
                "VM memory_mib must be between 128 and 262144 (got {})",
                self.memory_mib
            );
        }
        for package in &self.extra_packages {
            if !crate::config::safe_package_name(package) {
                bail!("VM extra_packages contains an invalid name: {package:?}");
            }
        }
        if let Some(primary) = &self.primary {
            crate::project::validate_project_name(primary)
                .context("VM primary must be a valid project name")?;
        }
        // A snapshot pauses the guest while memory is written; below 30
        // seconds the VM would spend most of its life paused.
        if self.snapshot_interval_secs < 30 {
            bail!("VM snapshot_interval_secs must be at least 30");
        }
        if !(1..=100).contains(&self.snapshot_keep) {
            bail!(
                "VM snapshot_keep must be between 1 and 100 (got {})",
                self.snapshot_keep
            );
        }
        if let Some(rootfs) = &self.rootfs_image {
            if rootfs.as_os_str().is_empty() || !rootfs.is_absolute() {
                bail!("VM rootfs_image must be an absolute path");
            }
            // Cloud Hypervisor packs this path into `path=...,image_type=...`.
            if rootfs.to_string_lossy().contains(',') {
                bail!("VM rootfs_image must not contain a comma");
            }
        }
        if let Some(index) = self.network_index
            && index >= crate::network::TAP_POOL
        {
            bail!(
                "VM network_index {index} is outside the {} TAP pool",
                crate::network::TAP_POOL
            );
        }
        Ok(())
    }
}

/// Validate a VM name. It becomes a directory name, so traversal and hidden
/// entries are rejected.
pub fn validate_name(name: &str) -> Result<()> {
    if name != name.trim() {
        bail!("VM name must not start or end with whitespace");
    }
    if name.is_empty() {
        bail!("VM name cannot be empty");
    }
    if name.len() > MAX_NAME_BYTES {
        bail!("VM name is too long");
    }
    if name.starts_with('.') || name.contains('/') || name.contains('\\') {
        bail!("VM name must not contain path separators or start with '.'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("VM name may only contain ASCII letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn valid() -> VmConfig {
        VmConfig {
            name: "rust".into(),
            ..VmConfig::default()
        }
    }

    #[test]
    fn from_config_seeds_the_global_defaults() {
        let mut config = Config::default();
        config.vcpus = 8;
        config.memory_mib = 16_384;
        config.extra_packages = vec!["rustup".into()];
        config.security = SecurityMode::Warn;
        config.snapshot_keep = 5;
        config.auto_resume = false;

        let vm = VmConfig::from_config(&config, "rust", Some(Harness::Opencode));
        assert_eq!(vm.version, CONFIG_VERSION);
        assert_eq!(vm.name, "rust");
        assert_eq!(vm.harness, Harness::Opencode);
        assert_eq!(vm.vcpus, 8);
        assert_eq!(vm.memory_mib, 16_384);
        assert_eq!(vm.extra_packages, vec!["rustup"]);
        assert_eq!(vm.security, SecurityMode::Warn);
        assert_eq!(vm.snapshot_keep, 5);
        assert!(!vm.auto_resume);
        assert!(vm.primary.is_none());
        assert!(vm.rootfs_image.is_none());
        assert!(vm.validate().is_ok());
    }

    #[test]
    fn from_config_uses_the_global_harness_when_none_is_given() {
        let mut config = Config::default();
        config.harness = Harness::Codex;
        let vm = VmConfig::from_config(&config, "rust", None);
        assert_eq!(vm.harness, Harness::Codex);
    }

    #[test]
    fn validation_bounds_resources_and_snapshots() {
        for vcpus in [0, 33, u8::MAX] {
            let mut vm = valid();
            vm.vcpus = vcpus;
            assert!(vm.validate().is_err(), "vcpus={vcpus}");
        }
        for vcpus in [1, 32] {
            let mut vm = valid();
            vm.vcpus = vcpus;
            assert!(vm.validate().is_ok(), "vcpus={vcpus}");
        }
        for memory in [0, 127, 262_145, u32::MAX] {
            let mut vm = valid();
            vm.memory_mib = memory;
            assert!(vm.validate().is_err(), "memory={memory}");
        }
        for memory in [128, 262_144] {
            let mut vm = valid();
            vm.memory_mib = memory;
            assert!(vm.validate().is_ok(), "memory={memory}");
        }

        let mut vm = valid();
        vm.snapshot_interval_secs = 29;
        assert!(vm.validate().is_err());
        vm.snapshot_interval_secs = 30;
        assert!(vm.validate().is_ok());

        for keep in [0, 101] {
            let mut vm = valid();
            vm.snapshot_keep = keep;
            assert!(vm.validate().is_err(), "keep={keep}");
        }
        for keep in [1, 100] {
            let mut vm = valid();
            vm.snapshot_keep = keep;
            assert!(vm.validate().is_ok(), "keep={keep}");
        }
    }

    #[test]
    fn validation_rejects_bad_names_versions_and_packages() {
        for name in ["", " ", "..", "../escape", "a/b", ".hidden", "a b", "café"] {
            let mut vm = valid();
            vm.name = name.into();
            assert!(vm.validate().is_err(), "{name:?}");
        }

        let mut vm = valid();
        vm.version = 3;
        let err = vm.validate().unwrap_err().to_string();
        assert!(err.contains("version"), "{err}");

        let mut vm = valid();
        vm.extra_packages = vec!["pkg;rm -rf /".into()];
        assert!(vm.validate().is_err());
        vm.extra_packages = vec!["build-essential".into(), "python3.12".into()];
        assert!(vm.validate().is_ok());
    }

    #[test]
    fn validation_checks_primary_rootfs_and_network() {
        let mut vm = valid();
        vm.primary = Some("../escape".into());
        assert!(vm.validate().is_err());
        vm.primary = Some("parser".into());
        assert!(vm.validate().is_ok());

        let mut vm = valid();
        vm.rootfs_image = Some(PathBuf::from("relative.qcow2"));
        assert!(vm.validate().is_err());
        vm.rootfs_image = Some(PathBuf::from("/disks/vm,backup.qcow2"));
        assert!(vm.validate().is_err());
        vm.rootfs_image = Some(PathBuf::from("/disks/rust.qcow2"));
        assert!(vm.validate().is_ok());

        let mut vm = valid();
        vm.network_index = Some(crate::network::TAP_POOL);
        let err = vm.validate().unwrap_err().to_string();
        assert!(err.contains("TAP pool"), "{err}");
        vm.network_index = Some(crate::network::TAP_POOL - 1);
        assert!(vm.validate().is_ok());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = toml::from_str::<VmConfig>("name = \"rust\"\nsurprise = true\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }
}
