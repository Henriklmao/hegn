//! Host prerequisite checks shared by `hegn doctor` and the TUI doctor modal.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::config::Config;
use crate::images::{self, AssetStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    /// Required checks make the report fail; optional tools are informational.
    pub required: bool,
    pub detail: String,
}

impl Check {
    pub fn new(
        name: impl Into<String>,
        ok: bool,
        required: bool,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            ok,
            required,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<Check>,
    pub assets: Vec<AssetStatus>,
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
}

/// Run every host check. This is blocking (process and filesystem probes) and
/// belongs off the UI thread only when it becomes expensive; today it is a
/// handful of version commands.
pub fn run(config: &Config) -> DoctorReport {
    let mut checks = Vec::new();
    checks.push(check_tool(
        "firecracker",
        &config.firecracker_bin,
        &["--version"],
        true,
    ));
    checks.push(check_tool(
        "virtiofsd",
        &config.virtiofsd_bin,
        &["--version"],
        true,
    ));
    checks.push(check_tool("git", Path::new("git"), &["--version"], true));
    checks.push(check_tool(
        "lazygit",
        Path::new("lazygit"),
        &["--version"],
        false,
    ));
    checks.push(Check::new(
        "kvm",
        Path::new("/dev/kvm").exists(),
        true,
        if Path::new("/dev/kvm").exists() {
            "/dev/kvm available".to_string()
        } else {
            "/dev/kvm missing (enable KVM / add user to the kvm group)".to_string()
        },
    ));
    checks.push(Check::new(
        "kernel image",
        images::is_regular_file(&config.kernel_image),
        true,
        if images::is_regular_file(&config.kernel_image) {
            config.kernel_image.display().to_string()
        } else {
            format!(
                "{} (run `hegn images download`)",
                config.kernel_image.display()
            )
        },
    ));
    checks.push(Check::new(
        "rootfs image",
        images::is_regular_file(&config.rootfs_image),
        true,
        if images::is_regular_file(&config.rootfs_image) {
            config.rootfs_image.display().to_string()
        } else {
            format!(
                "{} (run `hegn images download`)",
                config.rootfs_image.display()
            )
        },
    ));
    checks.push(Check::new(
        "projects root",
        config.projects_root.is_dir(),
        true,
        config.projects_root.display().to_string(),
    ));
    checks.push(Check::new(
        "serial logs",
        config.logs_dir().is_dir(),
        false,
        format!(
            "{} (created automatically on first session)",
            config.logs_dir().display()
        ),
    ));
    checks.push(Check::new(
        "backend",
        config.firecracker_ready(),
        true,
        format!(
            "{} (configured {}); a VM is mandatory",
            config.effective_backend().label(),
            config.backend.label()
        ),
    ));
    checks.push(Check::new(
        "editor",
        std::env::var_os("EDITOR").is_some() || std::env::var_os("VISUAL").is_some(),
        false,
        std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "$EDITOR is unset".into()),
    ));

    let ok = checks.iter().all(|check| !check.required || check.ok);
    DoctorReport {
        ok,
        checks,
        assets: images::status(config),
    }
}

fn check_tool(name: &str, binary: &Path, args: &[&str], required: bool) -> Check {
    match Command::new(binary).args(args).output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let version = stdout.lines().next().unwrap_or("").trim();
            Check::new(
                name,
                true,
                required,
                if version.is_empty() {
                    output.status.to_string()
                } else {
                    version.to_string()
                },
            )
        }
        Ok(output) => Check::new(
            name,
            false,
            required,
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ),
        Err(err) => Check::new(name, false, required, err.to_string()),
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
        config.firecracker_bin = dir.path().join("missing-firecracker");
        config.virtiofsd_bin = dir.path().join("missing-virtiofsd");
        let report = run(&config);
        assert!(!report.ok);
        assert!(report.failures().any(|check| check.name == "kernel image"));
        assert!(report.failures().any(|check| check.name == "rootfs image"));
        assert!(report.missing_assets().count() >= 2);
        assert!(report.can_download());
    }

    #[test]
    fn missing_editor_is_optional() {
        let report = DoctorReport {
            ok: true,
            checks: vec![Check::new("editor", false, false, "unset")],
            assets: Vec::new(),
        };
        assert!(report.ok);
        assert_eq!(report.failures().count(), 0);
    }
}
