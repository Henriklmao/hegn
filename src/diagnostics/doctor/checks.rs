//! Individual host prerequisite checks shared by the doctor report.

use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::config::{Config, NetworkMode, VIRTIOFSD_FALLBACKS};
use crate::network::{CONTROL_TAP_POOL, HostFirewall, TAP_POOL, control_tap_name, tap_name};
use crate::session::snapshot::SnapshotStore;
use crate::session::ssh::SshIdentity;
use crate::vm::config::VmStore;
use crate::vm::images;
use crate::vm::manager::VmManager;

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

    /// One line for the human-readable report.
    ///
    /// `✓` is a passing check, `✗` a failed required check and `!` a legacy or
    /// optional finding. Only `✗` makes the report fail, matching the exit
    /// status.
    pub fn line(&self) -> String {
        let marker = match (self.ok, self.required) {
            (true, _) => "✓",
            (false, true) => "✗",
            (false, false) => "!",
        };
        format!("{marker} {:<18} {}", self.name, self.detail)
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
    checks.push(ssh_client_check(Path::new("ssh")));
    checks.push(ssh_keygen_check(Path::new("ssh-keygen")));
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
        true,
        if landlock {
            "Landlock active (VMM filesystem sandbox)".to_string()
        } else {
            "Landlock is not active in /sys/kernel/security/lsm; hegn requires a kernel with \
             Landlock (Linux 6.2+)"
                .to_string()
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
    let usage = filesystem_usage(&config.vms_dir).or_else(|| filesystem_usage(&config.state_dir));
    let (disk_ok, disk_detail) = match usage {
        Some((used, total)) if total > 0 => {
            let percent = used.saturating_mul(100) / total;
            (
                percent < u64::from(config.disk_usage_warn_percent),
                format!(
                    "{}% used on the VM filesystem (warn at {}%)",
                    percent, config.disk_usage_warn_percent
                ),
            )
        }
        _ => (true, "VM filesystem not created yet".to_string()),
    };
    checks.push(Check::new("disk space", disk_ok, false, disk_detail));
    let marker = crate::network::ready_marker();
    let control_taps = control_taps_present();
    let data_taps = data_taps_present();
    let live = crate::network::live_guard_status(&config.endpoint_ports());
    let guard_ready = crate::network::guard_ready(config);
    checks.push(network_check(
        config,
        marker.as_ref(),
        control_taps,
        data_taps,
        live,
        guard_ready,
    ));
    checks.extend(running_vm_checks(config));
    checks.push(Check::new(
        "serial logs",
        config.logs_dir().is_dir(),
        false,
        format!(
            "{} (created automatically on first session)",
            config.logs_dir().display()
        ),
    ));
    let health = latest_guest_health(config);
    let now = chrono::Utc::now().timestamp();
    checks.push(Check::new(
        "guest health",
        health.as_ref().is_some_and(|health| health.healthy(now)),
        false,
        match &health {
            Some(health) => health.summary(now),
            None => "no hegn-health line yet (boot a VM)".to_string(),
        },
    ));
    checks.push(snapshot_control_check(config));
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
    let data_root = dirs::data_dir().map(|root| root.join("hegn"));
    checks.extend(legacy_checks(
        config,
        data_root.as_deref(),
        &Config::config_path(),
    ));
    checks
}

/// The OpenSSH client is mandatory for every attach and `open`.
///
/// `ssh -V` prints its version on stderr and exits successfully. A missing
/// binary is a required failure with an install hint, not just a raw `ENOENT`.
fn ssh_client_check(binary: &Path) -> Check {
    match Command::new(binary).arg("-V").output() {
        Ok(output) if output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let version = stderr
                .lines()
                .chain(stdout.lines())
                .next()
                .unwrap_or("")
                .trim();
            let detail = if version.is_empty() {
                format!("{} available", binary.display())
            } else {
                format!("{version} ({})", binary.display())
            };
            Check::new("ssh", true, true, detail)
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = if stderr.trim().is_empty() {
                output.status.to_string()
            } else {
                stderr.trim().to_string()
            };
            Check::new(
                "ssh",
                false,
                true,
                format!("`{} -V` failed: {detail}", binary.display()),
            )
        }
        Err(err) => Check::new(
            "ssh",
            false,
            true,
            format!(
                "{}: {err}; install openssh-client (Debian/Ubuntu) or openssh (Fedora/Arch) and \
                 make sure `ssh` is on PATH",
                binary.display()
            ),
        ),
    }
}

/// `ssh-keygen` creates and validates every per-VM client key.
///
/// The tool has no `--version` flag. A known-hosts lookup against `/dev/null`
/// is side-effect free: exit 0 means "found", exit 1 means "not found"; both
/// prove the binary ran. Any other status is a broken install.
fn ssh_keygen_check(binary: &Path) -> Check {
    let output = Command::new(binary)
        .args(["-F", "hegn-doctor-probe", "-f", "/dev/null"])
        .output();
    match output {
        Ok(output) if matches!(output.status.code(), Some(0 | 1)) => Check::new(
            "ssh-keygen",
            true,
            true,
            format!("sshd key generator available ({})", binary.display()),
        ),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = if stderr.trim().is_empty() {
                output.status.to_string()
            } else {
                stderr.trim().to_string()
            };
            Check::new(
                "ssh-keygen",
                false,
                true,
                format!("`{}` failed: {detail}", binary.display()),
            )
        }
        Err(err) => Check::new(
            "ssh-keygen",
            false,
            true,
            format!(
                "{}: {err}; install openssh (it ships the `ssh-keygen` key generator)",
                binary.display()
            ),
        ),
    }
}

/// Every VM needs a control TAP for SSH, regardless of its data-network mode.
///
/// A version 3 marker records the control pool, but only the kernel knows
/// whether the devices still exist. A missing marker is also the version gate:
/// markers written before v3 do not vouch for `hegnc*` at all.
fn network_check(
    config: &Config,
    marker: Option<&crate::network::ReadyMarker>,
    control_taps: bool,
    data_taps: bool,
    live: Option<bool>,
    ready: bool,
) -> Check {
    let guarded = config.network == NetworkMode::Guarded;
    if ready {
        let mut detail = if guarded {
            let endpoints = config.local_endpoints.len();
            if endpoints == 0 {
                format!(
                    "guarded egress: public HTTP(S)+DNS only, {CONTROL_TAP_POOL} control TAPs + \
                     {TAP_POOL} data TAP slices"
                )
            } else {
                format!(
                    "guarded egress + {endpoints} local endpoint(s), {CONTROL_TAP_POOL} control \
                     TAPs + {TAP_POOL} data TAP slices"
                )
            }
        } else {
            format!(
                "control TAP pool ready for SSH ({CONTROL_TAP_POOL} control TAPs); egress \
                 disabled (network = off)"
            )
        };
        if let Some(marker) = marker {
            detail.push_str(&format!("; marker v{}", marker.version));
            // Without the carve-out a host firewall's routed policy drops
            // everything hegn accepted, which looks like the guest being
            // offline. Naming it here turns that into a one-line answer.
            if guarded && marker.host_firewall != HostFirewall::None {
                detail.push_str(&format!(
                    "; routed through {}",
                    marker.host_firewall.label()
                ));
            }
        }
        match live {
            Some(true) => detail.push_str("; rules verified live"),
            None => detail.push_str("; marker only (run as root to verify rules)"),
            Some(false) => {}
        }
        return Check::new("network guard", true, true, detail);
    }

    let mut problems = Vec::new();
    if marker.is_none() {
        problems.push(
            "network marker v3 missing (not configured, or written by an older hegn without the \
             control TAP pool)"
                .to_string(),
        );
    }
    if marker.is_some_and(|marker| marker.endpoints != config.endpoint_ports()) {
        problems.push("local endpoints changed since setup".to_string());
    }
    if !control_taps {
        problems.push(format!(
            "control TAP pool {}..{} incomplete",
            control_tap_name(0),
            control_tap_name(CONTROL_TAP_POOL - 1)
        ));
    }
    if guarded && !data_taps {
        problems.push(format!(
            "data TAP pool {}..{} incomplete",
            tap_name(0),
            tap_name(TAP_POOL - 1)
        ));
    }
    match live {
        Some(false) => problems.push("nftables guard or control rules missing/incomplete".into()),
        None if crate::network::is_privileged() => {
            problems.push("cannot verify nftables rules (is `nft` installed?)".into());
        }
        _ => {}
    }
    if problems.is_empty() {
        problems.push("host network setup is not ready".to_string());
    }
    Check::new(
        "network guard",
        false,
        true,
        format!(
            "{}; run `sudo hegn network setup --user $USER`",
            problems.join("; ")
        ),
    )
}

/// Whether every TAP of one family exists below `root`.
fn tap_family_present_at(root: &Path, pool: u8, name: impl Fn(u8) -> String) -> bool {
    (0..pool).all(|index| root.join(name(index)).exists())
}

fn control_taps_present() -> bool {
    tap_family_present_at(
        Path::new("/sys/class/net"),
        CONTROL_TAP_POOL,
        control_tap_name,
    )
}

fn data_taps_present() -> bool {
    tap_family_present_at(Path::new("/sys/class/net"), TAP_POOL, tap_name)
}

/// The SSH-relevant fields of a running VM's session record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningVm {
    name: String,
    guest_ip: Option<Ipv4Addr>,
    ssh_user: Option<String>,
}

/// Every VM whose recorded Cloud Hypervisor process is still alive.
///
/// A stale `session.json` is not a running VM. `VmManager::is_running` verifies
/// the process identity, so a recycled PID never turns into a doctor finding.
fn running_vms(config: &Config) -> Vec<RunningVm> {
    let store = VmStore::new(config);
    let Ok(vms) = store.list() else {
        return Vec::new();
    };
    let mut running = Vec::new();
    for vm in vms {
        let Ok(manager) = VmManager::new(config.clone(), vm) else {
            continue;
        };
        if !manager.is_running() {
            continue;
        }
        let Some(session) = manager.session() else {
            continue;
        };
        running.push(RunningVm {
            name: manager.vm().name.clone(),
            guest_ip: session.guest_ip,
            ssh_user: session.ssh_user.filter(|user| !user.is_empty()),
        });
    }
    running.sort_by(|left, right| left.name.cmp(&right.name));
    running
}

fn running_vm_checks(config: &Config) -> Vec<Check> {
    running_vms(config)
        .iter()
        .map(|vm| running_vm_check(config, vm))
        .collect()
}

/// SSH readiness of one running VM.
///
/// A record written before SSH support is a warning: the serial console still
/// works, so an old VM must not turn an otherwise healthy host red. A current
/// record with a missing client key is a hard failure: that VM cannot be
/// attached over SSH until it is re-provisioned.
fn running_vm_check(config: &Config, vm: &RunningVm) -> Check {
    let name = format!("ssh {}", vm.name);
    let key = SshIdentity::new(&config.vms_dir, &vm.name)
        .key()
        .to_path_buf();
    let has_key = is_regular_file(&key);
    let has_record = vm.guest_ip.is_some() && vm.ssh_user.is_some();
    if has_record && has_key {
        let user = vm.ssh_user.as_deref().unwrap_or("?");
        let host = vm.guest_ip.map(|ip| ip.to_string()).unwrap_or_default();
        return Check::new(
            name,
            true,
            true,
            format!("{user}@{host} ready (key {})", key.display()),
        );
    }

    let mut problems = Vec::new();
    let mut missing = Vec::new();
    if vm.guest_ip.is_none() {
        missing.push("guest_ip");
    }
    if vm.ssh_user.is_none() {
        missing.push("ssh_user");
    }
    if !missing.is_empty() {
        problems.push(format!(
            "session.json lacks {} (VM started before SSH support)",
            missing.join(" and ")
        ));
    }
    if !has_key {
        problems.push(format!("SSH client key {} is missing", key.display()));
    }
    Check::new(
        name,
        false,
        has_record,
        format!(
            "{}; run `hegn vm apply {}` and restart the VM, or use `hegn attach {} --serial`",
            problems.join("; "),
            vm.name,
            vm.name
        ),
    )
}

/// Snapshots whose embedded device tree has no control NIC predate SSH support.
///
/// Restoring one would boot a guest that is unreachable over the control TAP,
/// so the restore path already rejects it. The doctor only reports the hint:
/// snapshots are never changed or deleted here, and this is not a hard failure.
fn snapshot_control_check(config: &Config) -> Check {
    let store = SnapshotStore::new(&config.snapshots_dir);
    let snapshots = match store.list() {
        Ok(snapshots) => snapshots,
        Err(err) => {
            return Check::new(
                "snapshot control NIC",
                false,
                false,
                format!(
                    "could not list snapshots in {}: {err}",
                    config.snapshots_dir.display()
                ),
            );
        }
    };
    let mut outdated = Vec::new();
    for snapshot in snapshots.iter().filter(|snapshot| !snapshot.is_legacy()) {
        let path = snapshot.dir.join("config.json");
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let has_control = value
            .get("net")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|devices| {
                devices.iter().any(|device| {
                    device
                        .get("tap")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|tap| tap.starts_with("hegnc"))
                })
            });
        if !has_control {
            outdated.push(snapshot.name.clone());
        }
    }
    if outdated.is_empty() {
        return Check::new(
            "snapshot control NIC",
            true,
            false,
            format!(
                "none found in {} (restorable snapshots include the control NIC)",
                config.snapshots_dir.display()
            ),
        );
    }
    outdated.sort();
    Check::new(
        "snapshot control NIC",
        false,
        false,
        format!(
            "{} snapshot(s) predate the control NIC and need a cold boot: {}; restart the VM and \
             take a fresh snapshot",
            outdated.len(),
            outdated.join(", ")
        ),
    )
}

/// Legacy v1 findings. Every check is informational: v2 ignores this state and
/// the report never changes or deletes it.
///
/// `data_root` is the parent of the default `vms_dir` (usually
/// `~/.local/share/hegn`). Tests pass a temporary root so no real home path is
/// inspected. `config_path` is the user's `config.toml`.
fn legacy_checks(config: &Config, data_root: Option<&Path>, config_path: &Path) -> Vec<Check> {
    vec![
        legacy_instance_check(config, data_root),
        legacy_snapshot_check(config),
        deprecated_project_keys_check(config),
        deprecated_config_keys_check(config_path),
    ]
}

/// Existing v1 instance directories that v2 ignores.
///
/// v1 stored instances next to the snapshot root as `instances` and later
/// `sandboxes`. The default data directory is checked too, so a custom
/// `vms_dir` cannot hide the old default directories. A reused `vms_dir` may
/// also contain v1 instances directly; a v2 VM has `vm.toml`, a v1 instance
/// has `hegn.toml` instead.
fn legacy_instance_dirs(config: &Config, data_root: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut siblings = Vec::new();
    if let Some(parent) = config.vms_dir.parent() {
        siblings.push(parent.join("instances"));
        siblings.push(parent.join("sandboxes"));
    }
    if let Some(root) = data_root {
        siblings.push(root.join("instances"));
        siblings.push(root.join("sandboxes"));
    }
    candidates.extend(siblings.into_iter().filter(|path| {
        path != &config.vms_dir
            && !path.starts_with(&config.vms_dir)
            && !config.vms_dir.starts_with(path)
    }));

    if let Ok(entries) = fs::read_dir(&config.vms_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if is_real_dir(&path)
                && is_regular_file(&path.join(crate::project::PROJECT_META_FILE))
                && !is_regular_file(&path.join(crate::vm::config::VM_CONFIG_FILE))
            {
                candidates.push(path);
            }
        }
    }

    candidates.retain(|path| is_real_dir(path));
    candidates.sort();
    candidates.dedup();
    candidates
}

/// A real directory; symlinks are never legacy state or VMs.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// A regular file; a symlinked marker does not make a directory a v1 instance.
fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn legacy_instance_check(config: &Config, data_root: Option<&Path>) -> Check {
    let dirs = legacy_instance_dirs(config, data_root);
    if dirs.is_empty() {
        return Check::new(
            "legacy instance dirs",
            true,
            false,
            "none found (v2 ignores v1 instance directories and never deletes them)",
        );
    }
    let paths = dirs
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Check::new(
        "legacy instance dirs",
        false,
        false,
        format!(
            "ignored by v2: {paths}; verify they are unused, then remove them manually \
             (v2 never starts, restores or deletes them)"
        ),
    )
}

/// Snapshots written by v1 have no boot fingerprint and can never be restored.
///
/// The count and names come from the normal [`SnapshotStore`] listing, so the
/// doctor sees exactly what `hegn snapshot list` sees. Nothing is selected,
/// restored or deleted here.
fn legacy_snapshot_check(config: &Config) -> Check {
    let store = SnapshotStore::new(&config.snapshots_dir);
    let snapshots = match store.list() {
        Ok(snapshots) => snapshots,
        Err(err) => {
            return Check::new(
                "legacy snapshots",
                false,
                false,
                format!(
                    "could not list snapshots in {}: {err}",
                    config.snapshots_dir.display()
                ),
            );
        }
    };
    let legacy = snapshots
        .iter()
        .filter(|snapshot| snapshot.is_legacy() && snapshot.legacy_label().is_some())
        .collect::<Vec<_>>();
    if legacy.is_empty() {
        return Check::new(
            "legacy snapshots",
            true,
            false,
            format!(
                "none found in {} (legacy snapshots are never restored or auto-deleted)",
                config.snapshots_dir.display()
            ),
        );
    }
    let mut names = legacy
        .iter()
        .map(|snapshot| snapshot.name.as_str())
        .collect::<Vec<_>>();
    names.sort();
    let names = names.join(", ");
    let noun = if legacy.len() == 1 {
        "snapshot"
    } else {
        "snapshots"
    };
    Check::new(
        "legacy snapshots",
        false,
        false,
        format!(
            "{} legacy v1 {noun} (no boot fingerprint; not restorable): {names}; never restored \
             or auto-deleted, manual cleanup only",
            legacy.len()
        ),
    )
}

/// Deprecated repo-local keys in the project picker and registry.
///
/// `project::deprecated_legacy_keys` follows the same file preference as
/// `read_meta`: the first readable, valid `hegn.toml` or `.hegn.toml` wins.
/// Registry-only External projects outside `projects_root` are scanned too, so
/// an imported project that is not visible in the picker still gets a finding.
fn deprecated_project_keys_check(config: &Config) -> Check {
    let mut candidates = Vec::new();
    let mut scan_error = None;
    match fs::read_dir(&config.projects_root) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    candidates.push(path);
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            scan_error = Some(format!(
                "could not scan {} for deprecated keys: {err}",
                config.projects_root.display()
            ));
        }
    }

    let mut registry_error = None;
    match crate::project::ProjectRegistry::load(&config.state_dir) {
        Ok(registry) => candidates.extend(
            registry
                .entries()
                .iter()
                .filter(|entry| entry.kind == crate::project::ProjectKind::External)
                .map(|entry| entry.path.clone()),
        ),
        Err(err) => {
            registry_error = Some(format!(
                "could not read project registry {}: {err}",
                config
                    .state_dir
                    .join(crate::project::REGISTRY_FILE)
                    .display()
            ));
        }
    }

    // A picker symlink and its canonical registry path are the same project.
    let mut unique = std::collections::BTreeMap::new();
    for path in candidates {
        unique.entry(project_scan_key(&path)).or_insert(path);
    }
    let mut paths = unique.into_values().collect::<Vec<_>>();
    paths.sort();

    let mut findings = Vec::new();
    for path in &paths {
        if !path.is_dir() {
            continue;
        }
        let keys = crate::project::deprecated_legacy_keys(path);
        if keys.is_empty() {
            continue;
        }
        let file = deprecated_project_meta_file(path).unwrap_or("hegn.toml");
        findings.push(format!(
            "{} (`{file}`: {})",
            path.display(),
            keys.join(", ")
        ));
    }
    findings.sort();

    let scan_failed = scan_error.is_some();
    let mut notes = Vec::new();
    if let Some(error) = scan_error {
        notes.push(error);
    }
    if let Some(error) = registry_error {
        notes.push(error);
    }
    let suffix = if notes.is_empty() {
        String::new()
    } else {
        format!("; {}", notes.join("; "))
    };

    if findings.is_empty() {
        return Check::new(
            "legacy project keys",
            !scan_failed,
            false,
            format!(
                "none found under {} (harness, packages and template belong in vm.toml){suffix}",
                config.projects_root.display()
            ),
        );
    }
    Check::new(
        "legacy project keys",
        false,
        false,
        format!(
            "{}; these keys belong in vm.toml or `hegn vm create` now and are ignored for control \
             in v2{suffix}",
            findings.join("; ")
        ),
    )
}

/// Canonical path when possible, otherwise an absolute lexical path.
///
/// This lets a picker symlink and its registry entry collapse to one scan.
fn project_scan_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// Which project metadata file `deprecated_legacy_keys` read.
fn deprecated_project_meta_file(path: &Path) -> Option<&'static str> {
    for name in [crate::project::PROJECT_META_FILE, ".hegn.toml"] {
        let file = path.join(name);
        let Ok(raw) = fs::read_to_string(&file) else {
            continue;
        };
        let Ok(value) = toml::from_str::<toml::Value>(&raw) else {
            continue;
        };
        if value.as_table().is_some() {
            return Some(name);
        }
    }
    None
}

/// Deprecated global keys still present in `config.toml`.
fn deprecated_config_keys_check(path: &Path) -> Check {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Check::new(
                "deprecated config keys",
                true,
                false,
                format!("none found ({} does not exist)", path.display()),
            );
        }
        Err(err) => {
            return Check::new(
                "deprecated config keys",
                false,
                false,
                format!("could not read {}: {err}", path.display()),
            );
        }
    };
    let keys = crate::config::deprecated_keys(&raw);
    if keys.is_empty() {
        return Check::new(
            "deprecated config keys",
            true,
            false,
            format!("none found in {}", path.display()),
        );
    }
    let pairs = keys
        .iter()
        .map(|key| format!("{key} -> {}", modern_config_key(key)))
        .collect::<Vec<_>>()
        .join(", ");
    Check::new(
        "deprecated config keys",
        false,
        false,
        format!(
            "{}: {pairs}; rewrite them to the modern names (the deprecated aliases still work)",
            path.display()
        ),
    )
}

/// Replacement for a key returned by `config::deprecated_keys`.
///
/// `config::deprecated::replacement` is private to the config module, so doctor
/// keeps this small mapping next to the report that names it.
fn modern_config_key(key: &str) -> &'static str {
    match key {
        "instances_dir" | "sandboxes_dir" => "vms_dir",
        "shared_tag" => "share_tag_prefix",
        "guest_mount" => "guest_projects_base",
        _ => "the documented key",
    }
}

/// Latest `hegn-health` line from the guest's serial logs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GuestHealth {
    version: u32,
    apt: String,
    checked: i64,
    reason: String,
    provision: String,
    mount: String,
}

impl GuestHealth {
    fn healthy(&self, now: i64) -> bool {
        self.reason == "ok"
            && self.provision != "failed"
            && self.mount != "missing"
            && now.saturating_sub(self.checked) <= 24 * 60 * 60
    }

    fn summary(&self, now: i64) -> String {
        let minutes = now.saturating_sub(self.checked).max(0) / 60;
        format!(
            "v{} apt={} provision={} mount={} reason={} (checked {} min ago)",
            self.version, self.apt, self.provision, self.mount, self.reason, minutes
        )
    }
}

/// Read the newest health line from the most recent session logs.
fn latest_guest_health(config: &Config) -> Option<GuestHealth> {
    let store = crate::session::logs::LogStore::new(config.logs_dir());
    for entry in store.list().ok()?.into_iter().take(5) {
        let Ok(raw) = crate::session::logs::tail_bytes(&entry.path, 4_000) else {
            continue;
        };
        let text = String::from_utf8_lossy(&raw);
        if let Some(health) = text.lines().rev().find_map(parse_health) {
            return Some(health);
        }
    }
    None
}

/// Parse `hegn-health v2 apt=… checked=… reason=… provision=… mount=…`.
fn parse_health(line: &str) -> Option<GuestHealth> {
    let start = line.find("hegn-health v")?;
    let mut tokens = line[start..].split_whitespace();
    if tokens.next()? != "hegn-health" {
        return None;
    }
    let version = tokens.next()?.strip_prefix('v')?.parse().ok()?;
    let mut fields = std::collections::HashMap::new();
    for token in tokens {
        let (key, value) = token.split_once('=')?;
        fields.insert(key, value);
    }
    Some(GuestHealth {
        version,
        apt: fields.get("apt")?.to_string(),
        checked: fields.get("checked")?.parse().ok()?,
        reason: fields.get("reason")?.to_string(),
        provision: fields
            .get("provision")
            .copied()
            .unwrap_or("unknown")
            .to_string(),
        mount: fields
            .get("mount")
            .copied()
            .unwrap_or("unknown")
            .to_string(),
    })
}

/// Used and total bytes of the filesystem holding `path`.
fn filesystem_usage(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // Safety: `c_path` is NUL-terminated and `stat` is a valid out pointer.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let total = stat.f_blocks.checked_mul(stat.f_frsize)?;
    let free = stat.f_bavail.checked_mul(stat.f_frsize)?;
    Some((total.saturating_sub(free), total))
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

    #[test]
    fn check_tool_accepts_a_working_executable() {
        // `/bin/echo` is stable, unlike a freshly written temp script, which
        // can hit ETXTBSY ("Text file busy") under a loaded test runner.
        let check = check_tool(
            "fake-tool",
            Path::new("/bin/echo"),
            &["fake-tool 1.2.3"],
            true,
        );
        assert!(check.ok, "{check:?}");
        assert!(check.required);
        assert!(check.detail.contains("fake-tool 1.2.3"), "{check:?}");
        // An explicit path is included so the user knows which binary ran.
        assert!(check.detail.contains("/bin/echo"), "{check:?}");
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
        // `/bin/sh -c` is stable, unlike a freshly written temp script, which
        // can hit ETXTBSY ("Text file busy") under a loaded test runner.
        let check = check_tool(
            "failing",
            Path::new("/bin/sh"),
            &["-c", "echo 'boom' >&2; exit 3"],
            false,
        );
        assert!(!check.ok);
        assert!(!check.required);
        assert_eq!(check.detail, "boom");
    }

    #[test]
    fn check_tool_reports_success_without_a_version_line() {
        // `/bin/true` is a stable executable that exits successfully without
        // printing anything; a temp script is more prone to transient exec
        // failures under a loaded test runner.
        let check = check_tool("silent", Path::new("/bin/true"), &[], true);
        assert!(check.ok);
        assert!(check.detail.contains("exit status"), "{check:?}");
    }

    #[test]
    fn health_lines_are_parsed_and_classified() {
        let now = 1_700_000_000;
        let line = "hegn-health v2 apt=5 checked=1699999900 reason=ok provision=ok mount=ok";
        let health = parse_health(line).unwrap();
        assert_eq!(health.version, 2);
        assert_eq!(health.apt, "5");
        assert_eq!(health.provision, "ok");
        assert!(health.healthy(now));
        assert!(health.summary(now).contains("1 min ago"));

        let failed = parse_health(
            "hegn-health v2 apt=0 checked=1699999900 reason=ok provision=failed mount=ok",
        )
        .unwrap();
        assert!(!failed.healthy(now));
        let missing = parse_health(
            "hegn-health v2 apt=0 checked=1699999900 reason=ok provision=ok mount=missing",
        )
        .unwrap();
        assert!(!missing.healthy(now));
        let stale =
            parse_health("hegn-health v2 apt=0 checked=1 reason=ok provision=ok mount=ok").unwrap();
        assert!(!stale.healthy(now));

        // Version 1 lines predate the provision/mount fields but stay readable.
        let old = parse_health("hegn-health v1 apt=2 checked=1699999900 reason=ok").unwrap();
        assert_eq!(old.provision, "unknown");
        assert_eq!(old.mount, "unknown");
        assert!(old.healthy(now));

        assert!(parse_health("booting linux").is_none());
        assert!(parse_health("hegn-health v2 apt=1").is_none());
    }

    #[test]
    fn latest_health_reads_the_newest_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            ..Config::default()
        };
        let logs = config.logs_dir();
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("session-a.log"),
            "boot\nhegn-health v2 apt=1 checked=1699999900 reason=ok provision=ok mount=ok\n",
        )
        .unwrap();
        let health = latest_guest_health(&config).expect("health line");
        assert_eq!(health.apt, "1");
        assert!(health.healthy(1_700_000_000));
    }

    fn isolated_config(dir: &Path) -> Config {
        Config {
            projects_root: dir.join("projects"),
            state_dir: dir.join("state"),
            snapshots_dir: dir.join("snapshots"),
            vms_dir: dir.join("data/vms"),
            ..Config::default()
        }
    }

    #[test]
    fn legacy_instance_dirs_lists_existing_v1_directories_only() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let data_root = dir.path().join("data");

        assert!(legacy_instance_dirs(&config, Some(&data_root)).is_empty());

        fs::create_dir_all(&config.vms_dir).unwrap();
        assert!(
            legacy_instance_dirs(&config, Some(&data_root)).is_empty(),
            "the active vms_dir is never a legacy directory"
        );

        fs::create_dir_all(data_root.join("sandboxes")).unwrap();
        fs::create_dir_all(data_root.join("instances")).unwrap();
        assert_eq!(
            legacy_instance_dirs(&config, Some(&data_root)),
            vec![data_root.join("instances"), data_root.join("sandboxes")]
        );
    }

    #[test]
    fn legacy_instance_dirs_finds_v1_children_inside_a_reused_vms_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let data_root = dir.path().join("data");

        let active = config.vms_dir.join("active");
        fs::create_dir_all(&active).unwrap();
        fs::write(active.join("vm.toml"), "version = 2\n").unwrap();
        let legacy = config.vms_dir.join("old");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("hegn.toml"), "harness = \"opencode\"\n").unwrap();
        // A stray directory with neither marker is not a v1 instance.
        fs::create_dir_all(config.vms_dir.join("stray")).unwrap();

        assert_eq!(
            legacy_instance_dirs(&config, Some(&data_root)),
            vec![legacy]
        );
    }

    #[test]
    fn legacy_instance_dirs_skips_sibling_candidates_that_are_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = isolated_config(dir.path());
        let data_root = dir.path().join("data");
        config.vms_dir = data_root.join("instances/vms");
        fs::create_dir_all(&config.vms_dir).unwrap();
        fs::create_dir_all(data_root.join("instances")).unwrap();

        assert!(
            legacy_instance_dirs(&config, Some(&data_root)).is_empty(),
            "an ancestor of vms_dir is not a legacy instance directory"
        );
    }

    #[test]
    fn legacy_instance_dirs_skips_symlinked_children() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let data_root = dir.path().join("data");
        fs::create_dir_all(&config.vms_dir).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("hegn.toml"), "harness = \"opencode\"\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), config.vms_dir.join("linked")).unwrap();

        assert!(
            legacy_instance_dirs(&config, Some(&data_root)).is_empty(),
            "a symlinked v1 instance is not scanned or reported"
        );
    }

    #[test]
    fn legacy_instance_check_states_that_cleanup_is_manual() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let data_root = dir.path().join("data");

        let absent = legacy_instance_check(&config, Some(&data_root));
        assert!(absent.ok);
        assert!(!absent.required);
        assert!(absent.detail.contains("none found"), "{}", absent.detail);

        fs::create_dir_all(data_root.join("instances")).unwrap();
        let present = legacy_instance_check(&config, Some(&data_root));
        assert!(!present.ok);
        assert!(
            !present.required,
            "legacy state is a warning, not a failure"
        );
        for needle in [
            "ignored by v2",
            "remove them manually",
            "never starts, restores or deletes",
        ] {
            assert!(
                present.detail.contains(needle),
                "missing {needle:?}: {}",
                present.detail
            );
        }
        assert!(present.detail.contains("instances"), "{}", present.detail);
        assert!(present.line().starts_with("! "), "{}", present.line());
    }

    fn write_legacy_snapshot(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "name": name,
            "created_at": "2026-01-01T00:00:00Z",
            "vm_state": "Paused",
            "dir": dir,
        });
        fs::write(
            dir.join("snapshot.json"),
            serde_json::to_vec_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    fn write_current_snapshot(root: &Path, name: &str) {
        use crate::session::snapshot::{SNAPSHOT_VERSION, SnapshotBoot, SnapshotMeta};

        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        SnapshotStore::new(root)
            .save_meta(&SnapshotMeta {
                name: name.to_string(),
                dir,
                version: SNAPSHOT_VERSION,
                boot_fingerprint: Some("a".repeat(64)),
                boot: Some(SnapshotBoot::default()),
                ..SnapshotMeta::default()
            })
            .unwrap();
    }

    #[test]
    fn legacy_snapshot_check_reports_count_names_and_manual_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        fs::create_dir_all(&config.snapshots_dir).unwrap();

        let absent = legacy_snapshot_check(&config);
        assert!(absent.ok);
        assert!(!absent.required);
        assert!(absent.detail.contains("none found"), "{}", absent.detail);

        write_legacy_snapshot(&config.snapshots_dir, "old-a");
        write_legacy_snapshot(&config.snapshots_dir, "old-b");
        write_current_snapshot(&config.snapshots_dir, "current");

        let present = legacy_snapshot_check(&config);
        assert!(!present.ok);
        assert!(!present.required);
        for needle in [
            "2 legacy v1 snapshots",
            "old-a, old-b",
            "no boot fingerprint",
            "not restorable",
            "never restored or auto-deleted",
            "manual cleanup only",
        ] {
            assert!(
                present.detail.contains(needle),
                "missing {needle:?}: {}",
                present.detail
            );
        }
        assert!(
            !present.detail.contains("current"),
            "v2 snapshots are not legacy: {}",
            present.detail
        );
        assert!(present.line().starts_with("! "), "{}", present.line());

        let json = serde_json::to_value(&present).unwrap();
        assert_eq!(json["name"], "legacy snapshots");
        assert_eq!(json["ok"], false);
        assert_eq!(json["required"], false);
        assert!(
            json["detail"]
                .as_str()
                .unwrap()
                .contains("manual cleanup only")
        );
    }

    #[test]
    fn legacy_snapshot_check_lists_a_corrupt_metadata_as_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        fs::create_dir_all(config.snapshots_dir.join("broken")).unwrap();
        fs::write(
            config.snapshots_dir.join("broken/snapshot.json"),
            b"{not json",
        )
        .unwrap();

        let check = legacy_snapshot_check(&config);
        assert!(check.ok, "list skips unreadable entries: {}", check.detail);
        assert!(check.detail.contains("none found"), "{}", check.detail);
    }

    #[test]
    fn deprecated_project_keys_name_the_file_and_keys() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let demo = config.projects_root.join("demo");
        let other = config.projects_root.join("other");
        fs::create_dir_all(&demo).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(
            demo.join("hegn.toml"),
            "harness = \"opencode\"\npackages = []\n",
        )
        .unwrap();
        fs::write(other.join(".hegn.toml"), "template = \"rust\"\n").unwrap();

        let check = deprecated_project_keys_check(&config);
        assert!(!check.ok);
        assert!(!check.required);
        for needle in [
            "`hegn.toml`: harness, packages",
            "`.hegn.toml`: template",
            "belong in vm.toml",
            "`hegn vm create`",
            "ignored for control in v2",
        ] {
            assert!(
                check.detail.contains(needle),
                "missing {needle:?}: {}",
                check.detail
            );
        }
        assert!(check.detail.contains(&demo.display().to_string()));
        assert!(check.detail.contains(&other.display().to_string()));
        assert!(check.line().starts_with("! "), "{}", check.line());

        let absent = deprecated_project_keys_check(&Config {
            projects_root: dir.path().join("empty-projects"),
            ..Config::default()
        });
        assert!(absent.ok);
        assert!(absent.detail.contains("none found"), "{}", absent.detail);
    }

    fn write_registry(config: &Config, entries: &[(&str, &Path)]) {
        let mut raw = String::from("version = 2\n");
        for (name, path) in entries {
            raw.push_str(&format!(
                "\n[[project]]\nname = {name:?}\npath = {:?}\nvm = \"demo-vm\"\n\
                 created_at = \"2026-01-01T00:00:00Z\"\n",
                path.display().to_string()
            ));
        }
        fs::create_dir_all(&config.state_dir).unwrap();
        fs::write(config.state_dir.join(crate::project::REGISTRY_FILE), raw).unwrap();
    }

    #[test]
    fn registry_only_external_projects_are_scanned_for_deprecated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let outside = dir.path().join("outside/demo");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("hegn.toml"), "harness = \"opencode\"\n").unwrap();
        write_registry(&config, &[("demo", &outside)]);

        let check = deprecated_project_keys_check(&config);
        assert!(!check.ok);
        assert!(!check.required);
        assert!(
            check.detail.contains(&outside.display().to_string()),
            "{}",
            check.detail
        );
        assert!(
            check.detail.contains("`hegn.toml`: harness"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn picker_and_registry_project_paths_are_scanned_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let demo = config.projects_root.join("demo");
        fs::create_dir_all(&demo).unwrap();
        fs::write(demo.join("hegn.toml"), "packages = [\"git\"]\n").unwrap();
        write_registry(&config, &[("demo", &demo)]);

        let check = deprecated_project_keys_check(&config);
        let path = demo.display().to_string();
        assert_eq!(
            check.detail.matches(&path).count(),
            1,
            "the picker and registry entries are one project: {}",
            check.detail
        );
    }

    #[test]
    fn registry_read_error_is_reported_without_failing_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        fs::create_dir_all(&config.state_dir).unwrap();
        fs::write(
            config.state_dir.join(crate::project::REGISTRY_FILE),
            b"version = [not valid",
        )
        .unwrap();

        let check = deprecated_project_keys_check(&config);
        assert!(check.ok, "a registry read error is informational");
        assert!(!check.required);
        assert!(
            check.detail.contains("could not read project registry"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("none found"), "{}", check.detail);
    }

    #[test]
    fn project_keys_in_a_clean_hegn_toml_are_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let project = config.projects_root.join("demo");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("hegn.toml"),
            "launch = \"opencode\"\nauto_start = true\n",
        )
        .unwrap();

        let check = deprecated_project_keys_check(&config);
        assert!(check.ok);
        assert!(check.detail.contains("none found"), "{}", check.detail);
    }

    #[test]
    fn deprecated_global_config_keys_are_reported_with_modern_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "instances_dir = \"/data/legacy\"\nshared_tag = \"old\"\n",
        )
        .unwrap();

        let check = deprecated_config_keys_check(&path);
        assert!(!check.ok);
        assert!(!check.required);
        for needle in [
            "instances_dir -> vms_dir",
            "shared_tag -> share_tag_prefix",
            "deprecated aliases still work",
        ] {
            assert!(
                check.detail.contains(needle),
                "missing {needle:?}: {}",
                check.detail
            );
        }
        assert!(check.detail.contains(&path.display().to_string()));
        assert!(check.line().starts_with("! "), "{}", check.line());

        fs::write(
            &path,
            "vms_dir = \"/data/vms\"\nshare_tag_prefix = \"hegn\"\n",
        )
        .unwrap();
        let absent = deprecated_config_keys_check(&path);
        assert!(absent.ok);
        assert!(absent.detail.contains("none found"), "{}", absent.detail);

        let missing = deprecated_config_keys_check(&dir.path().join("missing.toml"));
        assert!(missing.ok);
        assert!(
            missing.detail.contains("does not exist"),
            "{}",
            missing.detail
        );
    }

    #[test]
    fn legacy_checks_have_stable_json_fields_and_warn_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let config_path = dir.path().join("config.toml");
        let data_root = dir.path().join("data");

        let checks = legacy_checks(&config, Some(&data_root), &config_path);
        let names = checks
            .iter()
            .map(|check| check.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "legacy instance dirs",
                "legacy snapshots",
                "legacy project keys",
                "deprecated config keys"
            ]
        );
        for check in &checks {
            assert!(check.ok, "{}: {}", check.name, check.detail);
            assert!(!check.required);
            let json = serde_json::to_value(check).unwrap();
            assert_eq!(json["name"], check.name);
            assert_eq!(json["ok"], true);
            assert_eq!(json["required"], false);
            assert_eq!(json["detail"], check.detail);
        }

        fs::create_dir_all(data_root.join("instances")).unwrap();
        fs::write(&config_path, "instances_dir = \"/old\"\n").unwrap();
        let checks = legacy_checks(&config, Some(&data_root), &config_path);
        let json = serde_json::to_value(&checks[0]).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["required"], false);
        assert_eq!(json["name"], "legacy instance dirs");
        assert!(json["detail"].as_str().unwrap().contains("ignored by v2"));
    }

    #[test]
    fn warning_lines_use_a_distinct_marker() {
        assert!(Check::new("ok", true, true, "d").line().starts_with("✓ "));
        assert!(
            Check::new("required", false, true, "d")
                .line()
                .starts_with("✗ ")
        );
        assert!(
            Check::new("legacy", false, false, "d")
                .line()
                .starts_with("! ")
        );
    }

    #[test]
    fn missing_ssh_tools_are_required_failures_with_install_hints() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = ssh_client_check(&dir.path().join("missing-ssh"));
        assert_eq!(ssh.name, "ssh");
        assert!(!ssh.ok);
        assert!(ssh.required);
        assert!(ssh.detail.contains("openssh"), "{}", ssh.detail);

        let keygen = ssh_keygen_check(&dir.path().join("missing-ssh-keygen"));
        assert_eq!(keygen.name, "ssh-keygen");
        assert!(!keygen.ok);
        assert!(keygen.required);
        assert!(keygen.detail.contains("openssh"), "{}", keygen.detail);
    }

    #[test]
    fn ssh_tool_checks_accept_a_working_binary() {
        // `/bin/true` is stable; it is only used to prove the success path.
        let ssh = ssh_client_check(Path::new("/bin/true"));
        assert!(ssh.ok, "{ssh:?}");
        assert!(ssh.required);
        let keygen = ssh_keygen_check(Path::new("/bin/true"));
        assert!(keygen.ok, "{keygen:?}");
        assert!(keygen.required);
    }

    fn network_marker(endpoints: Vec<u16>) -> crate::network::ReadyMarker {
        crate::network::ReadyMarker {
            version: 3,
            pool: crate::network::TAP_POOL,
            subnet: crate::network::SUBNET.to_string(),
            control_pool: CONTROL_TAP_POOL,
            control_subnet: crate::network::CONTROL_SUBNET.to_string(),
            dns: crate::network::DNS
                .iter()
                .map(ToString::to_string)
                .collect(),
            endpoints,
            host_firewall: crate::network::HostFirewall::None,
            applied_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn network_check_requires_marker_v3_and_control_taps_for_off() {
        let config = Config {
            network: NetworkMode::Off,
            ..Config::default()
        };
        let marker = network_marker(Vec::new());
        // `off` needs control TAPs for SSH, but no data TAPs.
        let ready = network_check(&config, Some(&marker), true, false, Some(true), true);
        assert!(ready.ok, "{}", ready.detail);
        assert!(ready.required);
        assert!(ready.detail.contains("marker v3"), "{}", ready.detail);
        assert!(ready.detail.contains("control TAP"), "{}", ready.detail);
        assert!(ready.detail.contains("network = off"), "{}", ready.detail);

        let missing_control =
            network_check(&config, Some(&marker), false, false, Some(true), false);
        assert!(!missing_control.ok);
        assert!(missing_control.required);
        assert!(
            missing_control.detail.contains("hegnc0..hegnc15"),
            "{}",
            missing_control.detail
        );
        assert!(
            missing_control.detail.contains("sudo hegn network setup"),
            "{}",
            missing_control.detail
        );

        let no_marker = network_check(&config, None, true, false, Some(true), false);
        assert!(!no_marker.ok);
        assert!(no_marker.required);
        assert!(
            no_marker.detail.contains("marker v3"),
            "{}",
            no_marker.detail
        );
        assert!(
            no_marker.detail.contains("older hegn"),
            "{}",
            no_marker.detail
        );
    }

    #[test]
    fn network_check_covers_data_taps_and_live_control_rules() {
        let marker = network_marker(Vec::new());
        let guarded = Config {
            network: NetworkMode::Guarded,
            ..Config::default()
        };
        let missing_data = network_check(&guarded, Some(&marker), true, false, Some(true), false);
        assert!(!missing_data.ok);
        assert!(missing_data.required);
        assert!(
            missing_data.detail.contains("data TAP pool hegn0..hegn15"),
            "{}",
            missing_data.detail
        );

        let off = Config {
            network: NetworkMode::Off,
            ..Config::default()
        };
        let bad_rules = network_check(&off, Some(&marker), true, false, Some(false), false);
        assert!(!bad_rules.ok);
        assert!(bad_rules.required);
        assert!(
            bad_rules.detail.contains("nftables guard or control rules"),
            "{}",
            bad_rules.detail
        );

        // The live parser verifies control sets, source drops, the established
        // accept before the drop, the forward drop and the masquerade absence.
        let verified = network_check(&off, Some(&marker), true, false, Some(true), true);
        assert!(verified.ok, "{}", verified.detail);
        assert!(
            verified.detail.contains("rules verified live"),
            "{}",
            verified.detail
        );
    }

    #[test]
    fn tap_family_presence_checks_every_slot() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..CONTROL_TAP_POOL {
            fs::write(dir.path().join(control_tap_name(index)), b"").unwrap();
        }
        assert!(tap_family_present_at(
            dir.path(),
            CONTROL_TAP_POOL,
            control_tap_name
        ));

        fs::remove_file(dir.path().join(control_tap_name(7))).unwrap();
        assert!(!tap_family_present_at(
            dir.path(),
            CONTROL_TAP_POOL,
            control_tap_name
        ));
    }

    fn test_config_with_vms(dir: &Path) -> Config {
        Config {
            vms_dir: dir.join("vms"),
            ..Config::default()
        }
    }

    #[test]
    fn running_vm_without_ssh_metadata_is_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config_with_vms(dir.path());
        let check = running_vm_check(
            &config,
            &RunningVm {
                name: "old".into(),
                guest_ip: None,
                ssh_user: None,
            },
        );
        assert!(!check.ok);
        assert!(!check.required, "old VMs must not make the doctor red");
        for needle in [
            "guest_ip and ssh_user",
            "before SSH support",
            "hegn vm apply old",
            "hegn attach old --serial",
        ] {
            assert!(
                check.detail.contains(needle),
                "missing {needle:?}: {}",
                check.detail
            );
        }
        assert!(check.line().starts_with("! "), "{}", check.line());
    }

    #[test]
    fn running_vm_with_a_current_record_and_missing_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config_with_vms(dir.path());
        let key = SshIdentity::new(&config.vms_dir, "demo")
            .key()
            .to_path_buf();
        let check = running_vm_check(
            &config,
            &RunningVm {
                name: "demo".into(),
                guest_ip: Some(Ipv4Addr::new(10, 214, 0, 2)),
                ssh_user: Some("root".into()),
            },
        );
        assert!(!check.ok);
        assert!(check.required);
        assert!(
            check.detail.contains(&key.display().to_string()),
            "{}",
            check.detail
        );
        assert!(
            check.detail.contains("hegn vm apply demo"),
            "{}",
            check.detail
        );
        assert!(
            check.detail.contains("hegn attach demo --serial"),
            "{}",
            check.detail
        );
        assert!(check.line().starts_with("✗ "), "{}", check.line());
    }

    #[test]
    fn running_vm_with_ip_user_and_key_is_green() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config_with_vms(dir.path());
        let identity = SshIdentity::new(&config.vms_dir, "demo");
        fs::create_dir_all(identity.dir()).unwrap();
        fs::write(identity.key(), b"placeholder").unwrap();
        let check = running_vm_check(
            &config,
            &RunningVm {
                name: "demo".into(),
                guest_ip: Some(Ipv4Addr::new(10, 214, 0, 2)),
                ssh_user: Some("root".into()),
            },
        );
        assert!(check.ok, "{}", check.detail);
        assert!(check.required);
        assert!(check.detail.contains("root@10.214.0.2"), "{}", check.detail);
        assert!(check.line().starts_with("✓ "), "{}", check.line());
    }

    #[test]
    fn snapshots_without_a_control_nic_are_reported_as_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        fs::create_dir_all(&config.snapshots_dir).unwrap();
        write_current_snapshot(&config.snapshots_dir, "old-ssh");
        fs::write(
            config.snapshots_dir.join("old-ssh/config.json"),
            r#"{"net":[{"tap":"hegn3"}]}"#,
        )
        .unwrap();
        write_current_snapshot(&config.snapshots_dir, "ready-ssh");
        fs::write(
            config.snapshots_dir.join("ready-ssh/config.json"),
            r#"{"net":[{"tap":"hegn3"},{"tap":"hegnc3"}]}"#,
        )
        .unwrap();

        let check = snapshot_control_check(&config);
        assert!(!check.ok);
        assert!(!check.required, "old snapshots are a hint, not a failure");
        assert!(check.detail.contains("old-ssh"), "{}", check.detail);
        assert!(!check.detail.contains("ready-ssh"), "{}", check.detail);
        assert!(check.detail.contains("cold boot"), "{}", check.detail);
        assert!(check.line().starts_with("! "), "{}", check.line());

        fs::remove_dir_all(config.snapshots_dir.join("old-ssh")).unwrap();
        let ready = snapshot_control_check(&config);
        assert!(ready.ok, "{}", ready.detail);
        assert!(!ready.required);
    }
}
