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
    setup_actions_with(config, crate::network::guard_ready(config))
}

fn setup_actions_with(config: &Config, network_ready: bool) -> SetupActions {
    SetupActions {
        create_dirs: !config.projects_root.is_dir()
            || !config.state_dir.is_dir()
            || !config.snapshots_dir.is_dir()
            || !config.vms_dir.is_dir()
            || !config.templates_dir.is_dir()
            || !config.profiles_dir.is_dir()
            || !config.logs_dir().is_dir(),
        init_config: !Config::config_path().exists(),
        init_profiles: crate::agent::profile::ProfileStore::new(config).needs_templates(),
        download_images: images::status(config).iter().any(AssetStatus::missing),
        // Every mode needs the one-time setup: `off` still gets control TAPs
        // for SSH, guarded additionally needs the data TAPs and egress rules.
        configure_network: !network_ready,
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
            vms_dir: dir.path().join("instances"),
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

    #[test]
    fn network_setup_is_required_for_off_and_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            state_dir: dir.path().join("state"),
            projects_root: dir.path().join("projects"),
            snapshots_dir: dir.path().join("snapshots"),
            vms_dir: dir.path().join("instances"),
            templates_dir: dir.path().join("templates"),
            ..Config::default()
        };
        for mode in [
            crate::config::NetworkMode::Off,
            crate::config::NetworkMode::Guarded,
        ] {
            config.network = mode;
            assert!(
                setup_actions_with(&config, false).configure_network,
                "{mode:?} needs network setup"
            );
            assert!(
                !setup_actions_with(&config, true).configure_network,
                "{mode:?} is ready"
            );
        }
    }
}
