//! Individual host prerequisite checks shared by the doctor report.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::config::{Config, VIRTIOFSD_FALLBACKS};
use crate::vm::images;

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

/// Run every individual host check, in report order.
pub(super) fn host_checks(config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(match config.resolved_cloud_hypervisor() {
        Some(binary) => check_tool("cloud-hypervisor", &binary, &["--version"], true),
        None => Check::new(
            "cloud-hypervisor",
            false,
            true,
            format!(
                "{} not found; install cloud-hypervisor (or cloud-hypervisor-static) \
                 or set `cloud_hypervisor_bin`",
                config.cloud_hypervisor_bin.display()
            ),
        ),
    });
    checks.push(match config.resolved_virtiofsd() {
        Some(binary) => check_tool("virtiofsd", &binary, &["--version"], true),
        None => Check::new(
            "virtiofsd",
            false,
            true,
            format!(
                "{} not found on PATH or in {}; install virtiofsd or set `virtiofsd_bin`",
                config.virtiofsd_bin.display(),
                VIRTIOFSD_FALLBACKS.join(", ")
            ),
        ),
    });
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
    let landlock = landlock_active();
    checks.push(Check::new(
        "landlock",
        landlock,
        config.landlock,
        if landlock {
            "Landlock active (VMM filesystem sandbox)".to_string()
        } else if config.landlock {
            "Landlock is not active in /sys/kernel/security/lsm; install a kernel with Landlock \
             or set `landlock = false` (not recommended)"
                .to_string()
        } else {
            "disabled in config (not recommended)".to_string()
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
    let guarded = config.network == crate::config::NetworkMode::Guarded;
    let marker = crate::network::ready_marker();
    let guard_ready = crate::network::guard_ready(config);
    checks.push(Check::new(
        "network guard",
        guard_ready || !guarded,
        guarded,
        if !guarded {
            "disabled in config (air-gapped guest)".to_string()
        } else if guard_ready {
            let endpoints = config.local_endpoints.len();
            let mut detail = if endpoints == 0 {
                format!(
                    "guarded egress: public HTTP(S)+DNS only, {} TAP slices",
                    crate::network::TAP_POOL
                )
            } else {
                format!(
                    "guarded egress + {endpoints} local endpoint(s), {} TAP slices",
                    crate::network::TAP_POOL
                )
            };
            if let Some(firewall) = marker
                .as_ref()
                .map(|marker| marker.host_firewall)
                .filter(|firewall| *firewall != crate::network::HostFirewall::None)
            {
                // Without the carve-out a host firewall's routed policy drops
                // everything hegn accepted, which looks like the guest being
                // offline. Naming it here turns that into a one-line answer.
                detail.push_str(&format!("; routed through {}", firewall.label()));
            }
            detail
        } else if marker
            .as_ref()
            .is_some_and(|marker| marker.endpoints != config.endpoint_ports())
        {
            "local endpoints changed; re-run `sudo hegn network setup --user $USER`".to_string()
        } else if marker.is_some() {
            "TAP pool incomplete (deleted or renamed); re-run `sudo hegn network setup --user \
             $USER`"
                .to_string()
        } else {
            "not configured (or written by an older hegn); run `sudo hegn network setup --user \
             $USER`"
                .to_string()
        },
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
        config.vm_assets_ready(),
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
    checks
}

/// Is the Landlock ABI version Cloud Hypervisor requires (V3) available?
///
/// Cloud Hypervisor builds its ruleset with `HardRequirement`. Probing the
/// syscall is more reliable than parsing securityfs, which may not be mounted.
fn landlock_active() -> bool {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        // The Landlock syscall is number 444 on the generic and x86_64 tables.
        const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
        const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
        // Safety: this is the documented version query: a null ruleset, zero
        // size and the VERSION flag. It has no side effects.
        let abi = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        abi >= 3
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        std::fs::read_to_string("/sys/kernel/security/lsm")
            .map(|lsm| lsm.split(',').any(|entry| entry.trim() == "landlock"))
            .unwrap_or(false)
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
                match (version.is_empty(), binary.components().count() > 1) {
                    (false, true) => format!("{version} ({})", binary.display()),
                    (false, false) => version.to_string(),
                    (true, _) => output.status.to_string(),
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
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn check_tool_accepts_a_working_executable() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "fake-tool", "echo 'fake-tool 1.2.3'");

        let check = check_tool("fake-tool", &tool, &["--version"], true);
        assert!(check.ok, "{check:?}");
        assert!(check.required);
        assert!(check.detail.contains("fake-tool 1.2.3"), "{check:?}");
        // An explicit path is included so the user knows which binary ran.
        assert!(
            check.detail.contains(&tool.display().to_string()),
            "{check:?}"
        );
    }

    #[test]
    fn check_tool_reports_a_missing_executable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");

        let check = check_tool("missing", &missing, &["--version"], true);
        assert!(!check.ok);
        assert!(check.required);
        assert!(!check.detail.is_empty());
    }

    #[test]
    fn check_tool_reports_a_non_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-executable");
        std::fs::write(&path, b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let check = check_tool("noexec", &path, &["--version"], true);
        assert!(!check.ok);
        assert!(!check.detail.is_empty());
    }

    #[test]
    fn check_tool_reports_a_failing_executable_and_its_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "failing", "echo 'boom' >&2\nexit 3");

        let check = check_tool("failing", &tool, &["--version"], false);
        assert!(!check.ok);
        assert!(!check.required);
        assert_eq!(check.detail, "boom");
    }

    #[test]
    fn check_tool_reports_success_without_a_version_line() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "silent", "exit 0");

        let check = check_tool("silent", &tool, &[], true);
        assert!(check.ok);
        assert!(check.detail.contains("exit status"), "{check:?}");
    }
}
