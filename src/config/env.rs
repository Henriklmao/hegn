//! Environment-variable overrides applied after reading `config.toml`.
//!
//! These keep `hegn` scriptable in CI without touching the user's real
//! directories. An unknown value leaves the field unchanged.

use std::path::PathBuf;

use super::Config;
use super::types::{NetworkMode, RootfsFormat};

impl Config {
    pub(super) fn apply_env_overrides(&mut self) {
        fn path_var(name: &str) -> Option<PathBuf> {
            std::env::var_os(name).map(PathBuf::from)
        }
        if let Some(value) = path_var("HEGN_PROJECTS_ROOT") {
            self.projects_root = value;
        }
        if let Some(value) = path_var("HEGN_STATE_DIR") {
            self.state_dir = value;
        }
        if let Some(value) = path_var("HEGN_SNAPSHOTS_DIR") {
            self.snapshots_dir = value;
        }
        if let Some(value) = path_var("HEGN_INSTANCES_DIR") {
            self.instances_dir = value;
        }
        if let Some(value) = path_var("HEGN_TEMPLATES_DIR") {
            self.templates_dir = value;
        }
        if let Some(value) = path_var("HEGN_PROFILES_DIR") {
            self.profiles_dir = value;
        }
        if let Some(value) = path_var("HEGN_CLOUD_HYPERVISOR_BIN") {
            self.cloud_hypervisor_bin = value;
        }
        if let Some(value) = path_var("HEGN_VIRTIOFSD_BIN") {
            self.virtiofsd_bin = value;
        }
        if let Some(value) = path_var("HEGN_KERNEL_IMAGE") {
            self.kernel_image = value;
        }
        if let Some(value) = path_var("HEGN_ROOTFS_IMAGE") {
            self.rootfs_image = value;
        }
        if let Ok(value) = std::env::var("HEGN_ROOTFS_FORMAT") {
            self.rootfs_format = match value.trim().to_ascii_lowercase().as_str() {
                "raw" => RootfsFormat::Raw,
                "qcow2" => RootfsFormat::Qcow2,
                _ => self.rootfs_format,
            };
        }
        if let Ok(value) = std::env::var("HEGN_ROOT_DEVICE") {
            self.root_device = value;
        }
        if let Ok(value) = std::env::var("HEGN_KERNEL_URL") {
            self.kernel_url = value;
        }
        if let Ok(value) = std::env::var("HEGN_KERNEL_SHA256") {
            self.kernel_sha256 = value;
        }
        if let Ok(value) = std::env::var("HEGN_ROOTFS_URL") {
            self.rootfs_url = value;
        }
        if let Ok(value) = std::env::var("HEGN_ROOTFS_SHA256") {
            self.rootfs_sha256 = value;
        }
        if let Ok(value) = std::env::var("HEGN_NETWORK") {
            self.network = match value.trim().to_ascii_lowercase().as_str() {
                "off" | "disabled" => NetworkMode::Off,
                "guarded" | "web" => NetworkMode::Guarded,
                _ => self.network,
            };
        }
    }
}
