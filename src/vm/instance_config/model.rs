//! Instance configuration model, constants and validation.
//!
//! An [`InstanceConfig`] is the persisted description of one VM: its project,
//! workspace, harness preset and resource limits. It lives in `hegn.toml` next
//! to the instance's runtime directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Harness, SecurityMode};

pub const CONFIG_FILE: &str = "hegn.toml";
pub const WORKSPACE_DIR: &str = "workspace";
pub(super) const CONFIG_VERSION: u32 = 1;
pub(super) const MAX_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstanceConfig {
    pub version: u32,
    pub name: String,
    /// Original project the instance belongs to.
    pub project: PathBuf,
    /// Directory exported to the guest. For normal instances this is the
    /// project itself; for clones it is `<instance>/workspace`.
    pub workspace: PathBuf,
    /// Optional per-instance root filesystem. When absent, hegn copies the
    /// global image into the instance on first start so concurrent VMs never
    /// share a writable disk.
    pub rootfs_image: Option<PathBuf>,
    pub harness: Harness,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub shared_tag: String,
    pub guest_mount: String,
    /// Clone created for multi-agent work.
    pub ephemeral: bool,
    #[serde(alias = "source_sandbox")]
    pub source_instance: Option<String>,
    pub created_at: DateTime<Utc>,
    pub auto_branch: bool,
    pub security: SecurityMode,
    /// Additional apt packages on top of the harness preset.
    pub extra_packages: Vec<String>,
    /// Stable slice of the guarded-network TAP pool. Assigned on creation so
    /// snapshots restore onto the same device.
    #[serde(default)]
    pub network_index: Option<u8>,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            name: String::new(),
            project: PathBuf::new(),
            workspace: PathBuf::new(),
            rootfs_image: None,
            harness: Harness::default(),
            vcpus: 2,
            memory_mib: 2048,
            shared_tag: "workspace".to_string(),
            guest_mount: "/workspace".to_string(),
            ephemeral: false,
            source_instance: None,
            created_at: Utc::now(),
            auto_branch: true,
            security: SecurityMode::Enforce,
            extra_packages: Vec::new(),
            network_index: None,
        }
    }
}

impl InstanceConfig {
    /// Stable instance name derived from a project directory.
    pub fn suggested_name(project: &Path) -> String {
        let raw = project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "instance".to_string());
        let mut name = String::new();
        let mut last_dash = false;
        for ch in raw.chars() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                name.push(ch);
                last_dash = false;
            } else if !last_dash {
                name.push('-');
                last_dash = true;
            }
        }
        let name = name.trim_matches(['-', '.']).to_string();
        if name.is_empty() {
            "instance".to_string()
        } else {
            name.chars().take(MAX_NAME_BYTES).collect()
        }
    }

    pub fn dir(&self, root: &Path) -> PathBuf {
        root.join(&self.name)
    }

    pub fn config_path(&self, root: &Path) -> PathBuf {
        self.dir(root).join(CONFIG_FILE)
    }

    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported instance config version {} (expected {CONFIG_VERSION})",
                self.version
            );
        }
        if self.project.as_os_str().is_empty() || !self.project.is_absolute() {
            bail!("instance project must be an absolute path");
        }
        if self.workspace.as_os_str().is_empty() || !self.workspace.is_absolute() {
            bail!("instance workspace must be an absolute path");
        }
        if let Some(rootfs) = &self.rootfs_image {
            if rootfs.as_os_str().is_empty() || !rootfs.is_absolute() {
                bail!("instance rootfs_image must be an absolute path");
            }
            // Cloud Hypervisor packs this path into `path=...,image_type=...`.
            if rootfs.to_string_lossy().contains(',') {
                bail!("instance rootfs_image must not contain a comma");
            }
        }
        if self.vcpus == 0 || self.vcpus > 32 {
            bail!("instance vcpus must be between 1 and 32");
        }
        if self.memory_mib < 128 {
            bail!("instance memory_mib must be at least 128");
        }
        crate::vm::cloud_init::safe_token(&self.shared_tag)
            .then_some(())
            .context("instance shared_tag contains unsafe characters")?;
        if self.shared_tag == crate::agent::profile::PROFILE_SHARE_TAG {
            bail!(
                "instance shared_tag '{}' is reserved for the harness profile share",
                crate::agent::profile::PROFILE_SHARE_TAG
            );
        }
        if self.shared_tag.len() > 36 {
            bail!("instance shared_tag must be at most 36 bytes");
        }
        crate::vm::cloud_init::safe_mount(&self.guest_mount)
            .then_some(())
            .context("instance guest_mount must be an absolute, simple path")?;
        if let Some(source) = &self.source_instance {
            validate_name(source)?;
        }
        if let Some(index) = self.network_index
            && index >= crate::network::TAP_POOL
        {
            bail!(
                "instance network_index {index} is outside the {} TAP pool",
                crate::network::TAP_POOL
            );
        }
        for package in &self.extra_packages {
            if !crate::config::safe_package_name(package) {
                bail!("instance extra_packages contains an invalid name: {package:?}");
            }
        }
        Ok(())
    }
}

/// Validate an instance name. It becomes a directory name, so traversal and
/// hidden entries are rejected.
pub fn validate_name(name: &str) -> Result<()> {
    if name != name.trim() {
        bail!("instance name must not start or end with whitespace");
    }
    let name = name.trim();
    if name.is_empty() {
        bail!("instance name cannot be empty");
    }
    if name.len() > MAX_NAME_BYTES {
        bail!("instance name is too long");
    }
    if name.starts_with('.') || name.contains('/') || name.contains('\\') {
        bail!("instance name must not contain path separators or start with '.'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("instance name may only contain ASCII letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> InstanceConfig {
        InstanceConfig {
            name: "demo".into(),
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from("/projects/demo"),
            ..InstanceConfig::default()
        }
    }

    #[test]
    fn names_reject_traversal_separators_and_hidden_entries() {
        for name in [
            "",
            " ",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            ".hidden",
            ".ssh",
            "a b",
            " demo ",
            "demo ",
            " demo",
            "\tdemo",
            "a\0b",
            "café",
            "~root",
        ] {
            assert!(validate_name(name).is_err(), "{name:?} must be rejected");
        }
        assert!(validate_name(&"a".repeat(MAX_NAME_BYTES + 1)).is_err());
    }

    #[test]
    fn names_accept_ascii_letters_digits_and_safe_punctuation() {
        let longest = "a".repeat(MAX_NAME_BYTES);
        for name in [
            "demo",
            "demo-1",
            "demo_agent",
            "release.2026",
            "A1",
            &longest,
        ] {
            validate_name(name).unwrap_or_else(|err| panic!("{name:?}: {err}"));
        }
    }

    #[test]
    fn validation_requires_absolute_project_and_workspace_paths() {
        let mut instance = valid();
        instance.project = PathBuf::from("relative/project");
        assert!(
            instance
                .validate()
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );

        let mut instance = valid();
        instance.workspace = PathBuf::from("");
        assert!(
            instance
                .validate()
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );

        let mut instance = valid();
        instance.rootfs_image = Some(PathBuf::from("relative.qcow2"));
        assert!(
            instance
                .validate()
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );

        assert!(valid().validate().is_ok());
    }

    #[test]
    fn validation_rejects_a_comma_in_the_rootfs_path() {
        let mut instance = valid();
        instance.rootfs_image = Some(PathBuf::from("/disks/demo,backup.qcow2"));
        let err = instance.validate().unwrap_err().to_string();
        assert!(err.contains("comma"), "{err}");
    }

    #[test]
    fn validation_bounds_vcpus_and_memory() {
        for vcpus in [0, 33, u8::MAX] {
            let mut instance = valid();
            instance.vcpus = vcpus;
            assert!(instance.validate().is_err(), "vcpus={vcpus}");
        }
        for vcpus in [1, 32] {
            let mut instance = valid();
            instance.vcpus = vcpus;
            assert!(instance.validate().is_ok(), "vcpus={vcpus}");
        }
        for memory in [0, 127] {
            let mut instance = valid();
            instance.memory_mib = memory;
            assert!(instance.validate().is_err(), "memory={memory}");
        }
        let mut instance = valid();
        instance.memory_mib = 128;
        assert!(instance.validate().is_ok());
    }

    #[test]
    fn validation_rejects_reserved_unsafe_and_oversized_share_tags() {
        let oversized = "t".repeat(37);
        for tag in [
            crate::agent::profile::PROFILE_SHARE_TAG,
            "",
            "with space",
            "trailing/slash",
            "quote\"tag",
            &oversized,
        ] {
            let mut instance = valid();
            instance.shared_tag = tag.into();
            assert!(instance.validate().is_err(), "tag={tag:?}");
        }
        let mut instance = valid();
        instance.shared_tag = "workspace-1".into();
        assert!(instance.validate().is_ok());
    }

    #[test]
    fn validation_bounds_the_network_index_to_the_tap_pool() {
        let mut instance = valid();
        instance.network_index = Some(crate::network::TAP_POOL);
        let err = instance.validate().unwrap_err().to_string();
        assert!(err.contains("TAP pool"), "{err}");

        let mut instance = valid();
        instance.network_index = Some(crate::network::TAP_POOL - 1);
        assert!(instance.validate().is_ok());
    }

    #[test]
    fn validation_rejects_unsafe_guest_mounts_and_packages() {
        for mount in ["", "relative", "/", "/workspace path", "/work;rm"] {
            let mut instance = valid();
            instance.guest_mount = mount.into();
            assert!(instance.validate().is_err(), "mount={mount:?}");
        }
        let mut instance = valid();
        instance.guest_mount = "/work/space".into();
        assert!(instance.validate().is_ok());

        for package in ["", "-bad", "bad package", "pkg;rm -rf /"] {
            let mut instance = valid();
            instance.extra_packages = vec![package.into()];
            assert!(instance.validate().is_err(), "package={package:?}");
        }
        let mut instance = valid();
        instance.extra_packages = vec!["build-essential".into(), "python3.12".into()];
        assert!(instance.validate().is_ok());
    }

    #[test]
    fn validation_rejects_unknown_versions_and_bad_source_names() {
        let mut instance = valid();
        instance.version = CONFIG_VERSION + 1;
        assert!(
            instance
                .validate()
                .unwrap_err()
                .to_string()
                .contains("version")
        );

        let mut instance = valid();
        instance.source_instance = Some("../escape".into());
        assert!(instance.validate().is_err());

        let mut instance = valid();
        instance.source_instance = Some("demo-1".into());
        assert!(instance.validate().is_ok());
    }

    #[test]
    fn suggested_name_sanitizes_the_project_directory() {
        assert_eq!(
            InstanceConfig::suggested_name(Path::new("/projects/My Project!")),
            "My-Project"
        );
        assert_eq!(
            InstanceConfig::suggested_name(Path::new("/projects/---")),
            "instance"
        );
        assert_eq!(InstanceConfig::suggested_name(Path::new("/")), "instance");
        let long = "a".repeat(MAX_NAME_BYTES + 20);
        let name = InstanceConfig::suggested_name(&PathBuf::from(format!("/projects/{long}")));
        assert_eq!(name.len(), MAX_NAME_BYTES);
    }

    #[test]
    fn instance_paths_are_rooted_at_instances_dir() {
        let instance = valid();
        let root = Path::new("/state/instances");
        assert_eq!(instance.dir(root), root.join("demo"));
        assert_eq!(instance.config_path(root), root.join("demo/hegn.toml"));
    }
}
