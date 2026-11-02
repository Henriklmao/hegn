//! Default values for [`Config`] and the project template seeded into new
//! workspaces.

use std::path::PathBuf;

use crate::agent::launch::LaunchConfig;

use super::Config;
use super::types::{BackendKind, Harness, NetworkMode, SecurityMode, VirtiofsSandbox};

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let config_dir = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        let state_dir = dirs::state_dir().unwrap_or_else(|| home.join(".local/state"));
        let data_dir = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));

        Self {
            projects_root: home.join("Projects/hegn"),
            state_dir: state_dir.join("hegn"),
            snapshots_dir: data_dir.join("hegn/snapshots"),
            instances_dir: data_dir.join("hegn/instances"),
            templates_dir: config_dir.join("hegn/templates"),
            profiles_dir: config_dir.join("hegn/profiles"),
            cloud_hypervisor_bin: PathBuf::from("cloud-hypervisor"),
            virtiofsd_bin: PathBuf::from("virtiofsd"),
            kernel_image: data_dir
                .join("hegn/images")
                .join(crate::vm::images::KERNEL_ARTIFACT),
            rootfs_image: data_dir
                .join("hegn/images")
                .join(crate::vm::images::ROOTFS_ARTIFACT),
            rootfs_format: crate::vm::images::ROOTFS_FORMAT,
            root_device: "/dev/vda1".to_string(),
            landlock: true,
            seccomp: true,
            cloud_init: true,
            kernel_url: crate::vm::images::KERNEL_URL.to_string(),
            kernel_sha256: crate::vm::images::KERNEL_SHA256.to_string(),
            rootfs_url: crate::vm::images::ROOTFS_URL.to_string(),
            rootfs_sha256: crate::vm::images::ROOTFS_SHA256.to_string(),
            vcpus: 2,
            memory_mib: 2048,
            shared_tag: "workspace".to_string(),
            guest_mount: "/workspace".to_string(),
            virtiofs_sandbox: VirtiofsSandbox::Namespace,
            backend: BackendKind::Auto,
            harness: Harness::Shell,
            launch: LaunchConfig::default(),
            network: NetworkMode::Guarded,
            local_endpoints: Vec::new(),
            extra_packages: Vec::new(),
            auto_branch: true,
            security: SecurityMode::Enforce,
        }
    }
}

/// `AGENTS.md` seeded into new projects from `templates_dir`.
pub const DEFAULT_AGENTS_MD: &str = r#"# Agent instructions

This project runs inside a hegn instance.

## Rules

- Keep changes inside the workspace. The only writable mount is `/workspace`.
- Run the test suite before declaring a task complete.
- Do not print, copy, or transmit secrets (`.env`, SSH keys, tokens).
- Prefer small, reviewable commits. hegn may create a safety checkpoint branch.
- Ask before destructive operations (`rm -rf`, `git clean -fdx`, history rewrites).

## Layout

- Source: `/workspace/src`
- Tests: `/workspace/tests`
"#;

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::types::RootfsFormat;

    #[test]
    fn defaults_are_valid() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.guest_mount, "/workspace");
        assert_eq!(config.shared_tag, "workspace");
        assert_eq!(config.security, SecurityMode::Enforce);
        assert!(
            config.projects_root.ends_with("Projects/hegn"),
            "new projects belong in Projects/hegn: {}",
            config.projects_root.display()
        );
        assert!(crate::vm::images::is_sha256(&config.kernel_sha256));
        assert!(crate::vm::images::is_sha256(&config.rootfs_sha256));
        assert!(config.kernel_url.starts_with("https://"));
        assert!(config.rootfs_url.starts_with("https://"));
        assert_eq!(config.root_device, "/dev/vda1");
        assert_eq!(config.rootfs_format, RootfsFormat::Qcow2);
        assert!(config.landlock);
        assert!(config.seccomp);
    }
}
