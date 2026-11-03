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

/// Serializes every test that reads or mutates the process environment.
///
/// Tests in sibling modules (`paths`, `tools`) share this lock so a temporary
/// `HEGN_*` or `PATH` override can never leak into another test.
#[cfg(test)]
pub(super) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Environment variables overridden by the config and path tests in this
/// module tree (`apply_env_overrides` plus `Config::config_path`).
#[cfg(test)]
pub(super) const HEGN_ENV_VARS: &[&str] = &[
    "HEGN_PROJECTS_ROOT",
    "HEGN_STATE_DIR",
    "HEGN_SNAPSHOTS_DIR",
    "HEGN_INSTANCES_DIR",
    "HEGN_TEMPLATES_DIR",
    "HEGN_PROFILES_DIR",
    "HEGN_CLOUD_HYPERVISOR_BIN",
    "HEGN_VIRTIOFSD_BIN",
    "HEGN_KERNEL_IMAGE",
    "HEGN_ROOTFS_IMAGE",
    "HEGN_ROOTFS_FORMAT",
    "HEGN_ROOT_DEVICE",
    "HEGN_KERNEL_URL",
    "HEGN_KERNEL_SHA256",
    "HEGN_ROOTFS_URL",
    "HEGN_ROOTFS_SHA256",
    "HEGN_NETWORK",
    "HEGN_CONFIG",
];

/// Acquire [`ENV_LOCK`], recovering the guard if another test panicked.
#[cfg(test)]
pub(super) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Saves and clears the named variables, then applies overrides.
///
/// The original values are restored in [`Drop`], so a failing assertion cannot
/// leak test-only directories into later tests.
#[cfg(test)]
pub(super) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

#[cfg(test)]
impl EnvVarGuard {
    /// Callers must hold [`ENV_LOCK`] for the lifetime of the returned guard.
    pub(super) fn set(names: &[&'static str], overrides: &[(&'static str, &str)]) -> Self {
        let saved: Vec<_> = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        for name in names {
            // Safety: every caller holds ENV_LOCK, so no other test thread can
            // read or write the process environment concurrently.
            unsafe { std::env::remove_var(name) };
        }
        for (name, value) in overrides {
            // Safety: see above; overrides are applied while ENV_LOCK is held.
            unsafe { std::env::set_var(name, value) };
        }
        Self { saved }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            // Safety: the guard is declared after the lock guard in every test,
            // so ENV_LOCK is still held while these values are restored.
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn applies_every_supported_override() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set(
            HEGN_ENV_VARS,
            &[
                ("HEGN_PROJECTS_ROOT", "/tmp/hegn-env/projects"),
                ("HEGN_STATE_DIR", "/tmp/hegn-env/state"),
                ("HEGN_SNAPSHOTS_DIR", "/tmp/hegn-env/snapshots"),
                ("HEGN_INSTANCES_DIR", "/tmp/hegn-env/instances"),
                ("HEGN_TEMPLATES_DIR", "/tmp/hegn-env/templates"),
                ("HEGN_PROFILES_DIR", "/tmp/hegn-env/profiles"),
                (
                    "HEGN_CLOUD_HYPERVISOR_BIN",
                    "/tmp/hegn-env/bin/cloud-hypervisor",
                ),
                ("HEGN_VIRTIOFSD_BIN", "/tmp/hegn-env/bin/virtiofsd"),
                ("HEGN_KERNEL_IMAGE", "/tmp/hegn-env/images/vmlinux"),
                ("HEGN_ROOTFS_IMAGE", "/tmp/hegn-env/images/rootfs.qcow2"),
                ("HEGN_ROOTFS_FORMAT", " QCOW2 "),
                ("HEGN_ROOT_DEVICE", "/dev/vdb"),
                ("HEGN_KERNEL_URL", "https://example.test/vmlinux"),
                ("HEGN_KERNEL_SHA256", "kernel-digest"),
                ("HEGN_ROOTFS_URL", "https://example.test/rootfs"),
                ("HEGN_ROOTFS_SHA256", "rootfs-digest"),
                ("HEGN_NETWORK", " Disabled "),
            ],
        );

        let mut config = Config::default();
        config.apply_env_overrides();

        assert_eq!(
            config.projects_root,
            PathBuf::from("/tmp/hegn-env/projects")
        );
        assert_eq!(config.state_dir, PathBuf::from("/tmp/hegn-env/state"));
        assert_eq!(
            config.snapshots_dir,
            PathBuf::from("/tmp/hegn-env/snapshots")
        );
        assert_eq!(
            config.instances_dir,
            PathBuf::from("/tmp/hegn-env/instances")
        );
        assert_eq!(
            config.templates_dir,
            PathBuf::from("/tmp/hegn-env/templates")
        );
        assert_eq!(config.profiles_dir, PathBuf::from("/tmp/hegn-env/profiles"));
        assert_eq!(
            config.cloud_hypervisor_bin,
            PathBuf::from("/tmp/hegn-env/bin/cloud-hypervisor")
        );
        assert_eq!(
            config.virtiofsd_bin,
            PathBuf::from("/tmp/hegn-env/bin/virtiofsd")
        );
        assert_eq!(
            config.kernel_image,
            PathBuf::from("/tmp/hegn-env/images/vmlinux")
        );
        assert_eq!(
            config.rootfs_image,
            PathBuf::from("/tmp/hegn-env/images/rootfs.qcow2")
        );
        assert_eq!(config.rootfs_format, RootfsFormat::Qcow2);
        assert_eq!(config.root_device, "/dev/vdb");
        assert_eq!(config.kernel_url, "https://example.test/vmlinux");
        assert_eq!(config.kernel_sha256, "kernel-digest");
        assert_eq!(config.rootfs_url, "https://example.test/rootfs");
        assert_eq!(config.rootfs_sha256, "rootfs-digest");
        assert_eq!(config.network, NetworkMode::Off);
    }

    #[test]
    fn overrides_win_over_values_loaded_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "projects_root = \"/from/file/projects\"\n\
             state_dir = \"/from/file/state\"\n\
             vcpus = 4\n",
        )
        .unwrap();

        let _lock = env_lock();
        let _guard = EnvVarGuard::set(
            HEGN_ENV_VARS,
            &[
                ("HEGN_PROJECTS_ROOT", "/from/env/projects"),
                ("HEGN_STATE_DIR", "/from/env/state"),
            ],
        );

        let mut config = Config::load_from(&path).unwrap();
        assert_eq!(config.projects_root, PathBuf::from("/from/file/projects"));
        config.apply_env_overrides();
        assert_eq!(config.projects_root, PathBuf::from("/from/env/projects"));
        assert_eq!(config.state_dir, PathBuf::from("/from/env/state"));
        assert_eq!(config.vcpus, 4, "unrelated file values must be preserved");
    }

    #[test]
    fn unknown_enum_values_keep_the_previous_setting() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set(
            HEGN_ENV_VARS,
            &[("HEGN_ROOTFS_FORMAT", "ext4"), ("HEGN_NETWORK", "full")],
        );

        let mut config = Config::default();
        config.rootfs_format = RootfsFormat::Qcow2;
        config.network = NetworkMode::Off;
        config.apply_env_overrides();
        assert_eq!(config.rootfs_format, RootfsFormat::Qcow2);
        assert_eq!(config.network, NetworkMode::Off);
    }

    #[test]
    fn absent_variables_leave_the_configuration_untouched() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set(HEGN_ENV_VARS, &[]);

        let mut config = Config::default();
        config.projects_root = PathBuf::from("/custom/projects");
        config.rootfs_format = RootfsFormat::Qcow2;
        config.network = NetworkMode::Off;
        config.apply_env_overrides();

        assert_eq!(config.projects_root, PathBuf::from("/custom/projects"));
        assert_eq!(config.rootfs_format, RootfsFormat::Qcow2);
        assert_eq!(config.network, NetworkMode::Off);
    }
}
