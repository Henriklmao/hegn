//! Automatic host repairs that a first launch or the doctor modal can offer.

use serde::Serialize;

use crate::config::Config;
use crate::vm::images::{self, AssetStatus};

/// Host preparation that hegn can perform for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct SetupActions {
    /// Create missing state, projects, template and log directories.
    pub create_dirs: bool,
    /// Write `~/.config/hegn/config.toml` when it does not exist yet.
    pub init_config: bool,
    /// Write missing per-harness profile templates.
    pub init_profiles: bool,
    /// Download and hash-check missing kernel/rootfs images.
    pub download_images: bool,
    /// Create the guarded-network TAP pool and nftables rules (needs root).
    pub configure_network: bool,
}

impl SetupActions {
    pub fn any(self) -> bool {
        self.create_dirs
            || self.init_config
            || self.init_profiles
            || self.download_images
            || self.configure_network
    }
}

/// What a first launch or the doctor modal can fix automatically.
pub fn setup_actions(config: &Config) -> SetupActions {
    SetupActions {
        create_dirs: !config.projects_root.is_dir()
            || !config.state_dir.is_dir()
            || !config.snapshots_dir.is_dir()
            || !config.instances_dir.is_dir()
            || !config.templates_dir.is_dir()
            || !config.profiles_dir.is_dir()
            || !config.logs_dir().is_dir(),
        init_config: !Config::config_path().exists(),
        init_profiles: crate::agent::profile::ProfileStore::new(config).needs_templates(),
        download_images: images::status(config).iter().any(AssetStatus::missing),
        configure_network: config.network == crate::config::NetworkMode::Guarded
            && !crate::network::guard_ready(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_images_are_an_automatic_setup_action() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            projects_root: dir.path().join("projects"),
            snapshots_dir: dir.path().join("snapshots"),
            instances_dir: dir.path().join("instances"),
            templates_dir: dir.path().join("templates"),
            kernel_image: dir.path().join("missing-kernel"),
            rootfs_image: dir.path().join("missing-rootfs"),
            ..Config::default()
        };
        let actions = setup_actions(&config);
        assert!(actions.create_dirs);
        assert!(actions.download_images);
        assert!(actions.any());
    }
}
