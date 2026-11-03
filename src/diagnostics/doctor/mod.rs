//! Host prerequisite checks shared by `hegn doctor` and the TUI doctor modal.

use serde::Serialize;

use crate::config::Config;
use crate::vm::images::{self, AssetStatus};

mod checks;
mod setup;

pub use checks::Check;
pub use setup::{SetupActions, setup_actions};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<Check>,
    pub assets: Vec<AssetStatus>,
    /// Which automatic repairs are available.
    pub setup: SetupActions,
}

impl DoctorReport {
    /// Checks that block starting a VM.
    pub fn failures(&self) -> impl Iterator<Item = &Check> {
        self.checks
            .iter()
            .filter(|check| check.required && !check.ok)
    }

    pub fn missing_assets(&self) -> impl Iterator<Item = &AssetStatus> {
        self.assets.iter().filter(|asset| asset.missing())
    }

    pub fn can_download(&self) -> bool {
        self.missing_assets().next().is_some()
    }

    pub fn can_setup(&self) -> bool {
        self.setup.any()
    }
}

/// Run every host check. This is blocking (process and filesystem probes) and
/// belongs off the UI thread only when it becomes expensive; today it is a
/// handful of version commands.
pub fn run(config: &Config) -> DoctorReport {
    let checks = checks::host_checks(config);

    let ok = checks.iter().all(|check| !check.required || check.ok);
    DoctorReport {
        ok,
        checks,
        assets: images::status(config),
        setup: setup_actions(config),
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn missing_assets_and_tools_make_the_report_fail() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.state_dir = dir.path().join("state");
        config.projects_root = dir.path().join("projects");
        config.kernel_image = dir.path().join("missing-kernel");
        config.rootfs_image = dir.path().join("missing-rootfs");
        config.cloud_hypervisor_bin = dir.path().join("missing-cloud-hypervisor");
        config.virtiofsd_bin = dir.path().join("missing-virtiofsd");
        let report = run(&config);
        assert!(!report.ok);
        assert!(report.failures().any(|check| check.name == "kernel image"));
        assert!(
            report
                .failures()
                .any(|check| check.name == "cloud-hypervisor")
        );
        assert!(report.failures().any(|check| check.name == "rootfs image"));
        assert!(
            report.failures().any(
                |check| check.name == "virtiofsd" && check.detail.contains("not found on PATH")
            )
        );
        assert!(report.missing_assets().count() >= 2);
        assert!(report.can_download());
    }

    #[test]
    fn missing_editor_is_optional() {
        let report = DoctorReport {
            ok: true,
            checks: vec![Check::new("editor", false, false, "unset")],
            assets: Vec::new(),
            setup: SetupActions::default(),
        };
        assert!(report.ok);
        assert_eq!(report.failures().count(), 0);
        assert!(!report.can_setup());
    }
}
