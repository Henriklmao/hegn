//! [`VmManager`] shared state, locking and cross-cutting operations.
//!
//! The manager owns one VM's config, runtime lock, config store and a snapshot
//! of the private project registry. Boot, stop, status and snapshot work live
//! in the sibling modules and share these private fields.
//!
//! Boot always re-reads the registry (`VmManager::reload_registry`) so a
//! long-lived TUI cannot boot stale membership.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Config, SecurityMode};
use crate::project::ProjectRegistry;
use crate::session::logs::tail_bytes;
use crate::session::ssh::{SshError, SshIdentity, SshTarget};
use crate::vm::config::{VmConfig, VmStore};

use super::membership::MembershipManifest;
use super::paths::VmPaths;
use super::process::process_matches;
use super::session::{SessionRecord, VmReport};

#[derive(Debug, Clone)]
pub struct VmManager {
    pub(super) config: Config,
    pub(super) vm: VmConfig,
    pub(super) store: VmStore,
    pub(super) registry: ProjectRegistry,
}

/// Exclusive lock held for the duration of a VM lifecycle operation.
///
/// Dropping it (including on an error path) releases the lock.
#[derive(Debug)]
pub(super) struct RunLock {
    _file: File,
}

impl VmManager {
    pub fn new(config: Config, vm: VmConfig) -> Result<Self> {
        let registry = ProjectRegistry::load(&config.state_dir)?;
        let store = VmStore::new(&config);
        Ok(Self {
            config,
            vm,
            store,
            registry,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn vm(&self) -> &VmConfig {
        &self.vm
    }

    /// The full per-VM configuration, for callers that render VM settings.
    pub fn vm_config(&self) -> &VmConfig {
        &self.vm
    }

    /// Security mode of this VM, never the global default.
    ///
    /// The TUI reads it per VM so two VMs on the same host can use different
    /// guardrail modes.
    pub fn security_mode(&self) -> SecurityMode {
        self.vm.security
    }

    pub fn store(&self) -> &VmStore {
        &self.store
    }

    pub fn registry(&self) -> &ProjectRegistry {
        &self.registry
    }

    /// Re-read the project registry before a lifecycle operation.
    ///
    /// Membership changes are explicit, but the registry can change while a TUI
    /// is open; a start/restore must use the current file, not the snapshot
    /// captured when the manager was built.
    pub fn reload_registry(&mut self) -> Result<()> {
        self.registry = ProjectRegistry::load(&self.config.state_dir)?;
        Ok(())
    }

    /// Persist a newly assigned guarded-network slice so later boots and
    /// snapshot restores use the same TAP device.
    pub(super) fn persist_network_index(&self, index: u8) -> Result<()> {
        if self.vm.network_index == Some(index) {
            return Ok(());
        }
        let mut vm = self.vm.clone();
        vm.network_index = Some(index);
        self.store.save(&vm)
    }

    pub fn paths(&self) -> VmPaths {
        VmPaths::from_vm(&self.config, &self.vm)
    }

    pub fn session(&self) -> Option<SessionRecord> {
        let raw = fs::read_to_string(self.paths().session).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// The membership manifest of the current (or most recent) boot.
    pub fn membership(&self) -> Option<MembershipManifest> {
        MembershipManifest::load(&self.paths().membership).ok()
    }

    /// Warn when the running VM's exports differ from the current registry.
    ///
    /// Attaching still succeeds: hegn never kills a running agent to apply a
    /// membership change. Adding or removing shares requires a restart.
    pub fn membership_warning(&self) -> Option<String> {
        let manifest = self.membership()?;
        let Ok(registry) = ProjectRegistry::load(&self.config.state_dir) else {
            return Some(format!(
                "VM '{}' is running but its project membership cannot be verified because the \
                 project registry is unreadable; run `hegn vm apply {}` after fixing the registry \
                 to restart it with the current membership",
                self.vm.name, self.vm.name
            ));
        };
        let exported = registry.exported_members(&self.vm.name);
        let current: Vec<(&str, &std::path::Path, bool)> = exported
            .iter()
            .map(|entry| (entry.name.as_str(), entry.path.as_path(), entry.read_only))
            .collect();
        let recorded: Vec<(&str, &std::path::Path, bool)> = manifest
            .members
            .iter()
            .map(|member| {
                (
                    member.name.as_str(),
                    member.path.as_path(),
                    member.read_only,
                )
            })
            .collect();
        // `/workspace` is a boot-time symlink to the primary member, so a
        // primary change is as much a drift as an added or removed export.
        let current_primary = self
            .vm
            .primary
            .as_deref()
            .or_else(|| exported.first().map(|entry| entry.name.as_str()));
        if current == recorded && current_primary == Some(manifest.primary.as_str()) {
            return None;
        }
        Some(format!(
            "VM '{}' is running with different project membership or primary project; run \
             `hegn vm apply {}` to restart it with the current registry",
            self.vm.name, self.vm.name
        ))
    }

    /// A session file that exists but cannot be parsed is a migration hazard:
    /// an old VMM may still be running, but hegn no longer knows its pid.
    /// Refuse to overwrite that state instead of leaking an unmanaged VM.
    pub(super) fn unreadable_session(&self) -> bool {
        let path = self.paths().session;
        path.exists() && self.session().is_none()
    }

    /// The binary hegn may use to identify and terminate a recorded VMM.
    ///
    /// `session.json` lives in a directory a compromised guest cannot read but
    /// a local attacker can write, so the recorded path must not become an
    /// arbitrary process it can point at. Only accept the configured binary
    /// (or its resolved Cloud Hypervisor form) and fall back to the
    /// configuration otherwise.
    pub(super) fn trusted_vm_binary(&self, recorded: Option<&PathBuf>) -> PathBuf {
        let configured = self.config.vm_binary();
        match recorded {
            Some(path)
                if *path == configured
                    || self.config.resolved_cloud_hypervisor().as_deref()
                        == Some(path.as_path()) =>
            {
                path.clone()
            }
            _ => configured,
        }
    }

    /// Remove a stale socket without deleting a regular file or symlink.
    ///
    /// The virtiofs spawner shares this guard so an attacker-controlled
    /// `socket-path` cannot redirect cleanup at another file.
    pub(crate) fn remove_stale_socket(path: &Path) -> Result<()> {
        super::process::remove_socket(path)
    }

    pub fn is_running(&self) -> bool {
        self.session()
            .map(|session| {
                let binary = self.trusted_vm_binary(session.vm_binary.as_ref());
                process_matches(session.vm_pid, &binary)
            })
            .unwrap_or(false)
    }

    /// Take the exclusive VM lifecycle lock.
    ///
    /// A second `hegn open`/`hegn vm start` must not recreate the API or serial
    /// socket underneath a VM that is already booting.
    pub(super) fn lock(&self, paths: &VmPaths) -> Result<RunLock> {
        paths.ensure()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&paths.lock)
            .with_context(|| format!("failed to open {}", paths.lock.display()))?;
        // Safety: `file` is a valid, open descriptor for the lock file.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                bail!("another hegn VM operation is in progress; retry when it finishes");
            }
            return Err(err).with_context(|| format!("failed to lock {}", paths.lock.display()));
        }
        Ok(RunLock { _file: file })
    }

    /// Restart the VM so a changed config or membership takes effect.
    ///
    /// This is deliberately explicit: applying membership kills running
    /// agents. hegn never restarts a VM on its own.
    ///
    /// The guest may present a new host key after the reboot, so the cached
    /// `known_hosts` entry is dropped first. The client keypair is kept: the
    /// guest is re-provisioned with the existing public key on the next start.
    pub fn apply(&self) -> Result<super::session::VmReport> {
        super::seed_generation::bump(&self.config, &self.vm).with_context(|| {
            format!(
                "failed to advance the cloud-init seed generation for VM '{}'",
                self.vm.name
            )
        })?;
        crate::session::ssh::SshIdentity::new(&self.config.vms_dir, &self.vm.name)
            .remove_known_hosts()
            .with_context(|| {
                format!(
                    "failed to clear the SSH host key cache for VM '{}'",
                    self.vm.name
                )
            })?;
        if self.is_running() {
            self.stop()?;
        }
        let report = self.start_cold()?;
        self.wait_for_guest_ssh(&report)?;
        Ok(report)
    }

    /// Wait until cloud-init finished and SSH accepts the new host key.
    ///
    /// A boot with cloud-init rotates the guest host keys in its final stage.
    /// Returning immediately after the VMM starts would let a later attach
    /// cache the image host key and then fail once cloud-init rotates it. The
    /// host-key cache is cleared before every probe and once more at the end,
    /// so the next attach accepts the host key that is current at that time.
    fn wait_for_guest_ssh(&self, report: &VmReport) -> Result<()> {
        if !self.config.cloud_init {
            return Ok(());
        }
        let Some(index) = self.vm.network_index else {
            return Ok(());
        };
        let user = self
            .vm
            .effective_ssh_user(self.config.guest_credential_hardening)?;
        let identity = SshIdentity::new(&self.config.vms_dir, &self.vm.name);
        identity.load().with_context(|| {
            format!("failed to load the SSH identity for VM '{}'", self.vm.name)
        })?;
        let target = SshTarget::new(
            self.vm.name.clone(),
            crate::network::control_guest_ip(index),
            user.as_str(),
            identity.key().to_path_buf(),
            identity.known_hosts().to_path_buf(),
        )?;

        // Cloud-init rotates the guest host keys near the end of its run. Wait
        // for that marker before probing SSH; a missing or unreadable log is
        // not fatal, the SSH timeout below still applies.
        if let Some(log) = &report.serial_log {
            let deadline = Instant::now() + Duration::from_secs(180);
            while Instant::now() < deadline {
                if cloud_init_finished(log) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }

        identity.remove_known_hosts().with_context(|| {
            format!(
                "failed to clear the SSH host key cache for VM '{}'",
                self.vm.name
            )
        })?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut last: Option<String> = None;
        while Instant::now() < deadline {
            // `accept-new` cannot replace an existing entry. Clearing the
            // cache before each probe also covers a host-key rotation that
            // happens between two probes.
            identity.remove_known_hosts().with_context(|| {
                format!(
                    "failed to clear the SSH host key cache for VM '{}'",
                    self.vm.name
                )
            })?;
            match target.wait_ready(Duration::from_secs(5)) {
                Ok(()) => {
                    identity.remove_known_hosts().with_context(|| {
                        format!(
                            "failed to clear the SSH host key cache for VM '{}'",
                            self.vm.name
                        )
                    })?;
                    if let Some(log) = &report.serial_log {
                        self.wait_for_provision(log)?;
                    }
                    return Ok(());
                }
                Err(error) => {
                    if matches!(
                        error,
                        SshError::MissingClient
                            | SshError::InvalidUser { .. }
                            | SshError::MissingIdentity { .. }
                    ) {
                        return Err(error.into());
                    }
                    last = Some(error.to_string());
                }
            }
        }
        identity.remove_known_hosts().ok();
        bail!(
            "VM '{}' restarted, but SSH did not become ready: {}. Use 'hegn attach {} --serial' \
             if the guest did not boot.",
            self.vm.name,
            last.as_deref().unwrap_or("no probe completed"),
            self.vm.name
        );
    }

    /// Wait until the first-boot provisioner installed the harness preset.
    ///
    /// `wait_for_guest_ssh` returns as soon as sshd accepts connections, but
    /// cloud-init may still be installing the harness in the background. A
    /// user who stops the VM right after `apply` would otherwise interrupt the
    /// installation and only see it finish on the next boot.
    fn wait_for_provision(&self, log: &Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15 * 60);
        loop {
            match provision_state(log) {
                ProvisionState::Ready => return Ok(()),
                ProvisionState::Failed(reason) => bail!(
                    "VM '{}' is running, but the harness setup failed: {reason}. \
                     Attach with 'hegn attach {} --serial' to inspect the guest.",
                    self.vm.name,
                    self.vm.name
                ),
                ProvisionState::Pending => {}
            }
            if Instant::now() >= deadline {
                bail!(
                    "VM '{}' is running, but the harness setup did not finish within 15 minutes. \
                     Attach with 'hegn attach {} --serial' to follow the provisioning log.",
                    self.vm.name,
                    self.vm.name
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Attach the TUI/CLI terminal to the running guest serial console.
    pub fn attach_console(&self) -> Result<crate::session::console::VmConsole> {
        crate::session::console::VmConsole::attach(&self.config, &self.vm)
    }

    /// Boot a VM if needed, then attach its console.
    pub fn ensure_console(&self) -> Result<crate::session::console::VmConsole> {
        if !self.is_running() {
            self.start()?;
        }
        self.attach_console()
    }
}

/// True when the serial log shows cloud-init's final stage.
fn cloud_init_finished(path: &Path) -> bool {
    let Ok(bytes) = tail_bytes(path, 2000) else {
        return false;
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .any(|line| line.contains("Cloud-init") && line.contains("finished at"))
}

enum ProvisionState {
    Pending,
    Ready,
    Failed(String),
}

/// Read the harness provisioner state from the end of the serial log.
fn provision_state(path: &Path) -> ProvisionState {
    let Ok(bytes) = tail_bytes(path, 2000) else {
        return ProvisionState::Pending;
    };
    let text = String::from_utf8_lossy(&bytes);
    if text.lines().any(|line| {
        line.contains("hegn: preset ready") || line.contains("hegn: preset already provisioned")
    }) {
        return ProvisionState::Ready;
    }
    if let Some(line) = text
        .lines()
        .find(|line| line.contains("hegn-progress:") && line.contains("\"stage\":\"failed\""))
    {
        return ProvisionState::Failed(line.trim().to_string());
    }
    ProvisionState::Pending
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;

    use super::*;
    use crate::config::Harness;
    use crate::session::ssh::SshIdentity;
    use crate::vm::config::VmConfig;
    use crate::vm::manager::test_support::{config_with_assets, manager};

    #[test]
    fn lifecycle_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        let held = manager.lock(&paths).unwrap();
        let err = manager.lock(&paths).unwrap_err().to_string();
        assert!(err.contains("in progress"), "{err}");
        drop(held);
        assert!(
            manager.lock(&paths).is_ok(),
            "lock must be released on drop"
        );
    }

    #[test]
    fn running_state_ignores_a_forged_vm_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        // The configured binary is this test process; the session record tries
        // to point the identity check at an unrelated path.
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: dir.path().join("project"),
            workspace: dir.path().join("project"),
            vm: "demo".into(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid: std::process::id(),
            vm_binary: Some(PathBuf::from("/definitely/missing/cloud-hypervisor")),
            relay_pid: 0,
            virtiofs_pids: Vec::new(),
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
            guest_ip: None,
            ssh_user: None,
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
        assert!(
            manager.is_running(),
            "the configured binary must win over the recorded one"
        );
    }

    #[test]
    fn membership_warning_compares_the_manifest_with_the_registry() {
        use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
        use crate::vm::manager::MembershipManifest;
        use chrono::Utc;

        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config.clone(), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let plan = crate::vm::manager::test_support::plan_for(&manager);
        MembershipManifest::from_plan(&plan)
            .save(&paths.membership)
            .unwrap();
        assert!(manager.membership_warning().is_none());

        let second = dir.path().join("second");
        std::fs::create_dir_all(&second).unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry
            .upsert(ProjectEntry {
                name: "second".into(),
                path: second.canonicalize().unwrap(),
                vm: None,
                read_only: false,
                kind: ProjectKind::External,
                parent: None,
                managed: false,
                hidden: false,
                created_at: Utc::now(),
            })
            .unwrap();
        registry.assign("second", "demo", false).unwrap();

        let warning = manager.membership_warning().expect("membership changed");
        assert!(
            warning.contains("different project membership"),
            "{warning}"
        );
        assert!(warning.contains("hegn vm apply demo"), "{warning}");
    }

    #[test]
    fn membership_warning_includes_a_changed_primary() {
        use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
        use crate::vm::manager::MembershipManifest;
        use chrono::Utc;

        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let base = manager(config.clone(), dir.path());
        let vm = base.vm().clone();

        let second = dir.path().join("second");
        std::fs::create_dir_all(&second).unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry
            .upsert(ProjectEntry {
                name: "second".into(),
                path: second.canonicalize().unwrap(),
                vm: None,
                read_only: false,
                kind: ProjectKind::External,
                parent: None,
                managed: false,
                hidden: false,
                created_at: Utc::now(),
            })
            .unwrap();
        registry.assign("second", "demo", false).unwrap();

        let manager = VmManager::new(config.clone(), vm.clone()).unwrap();
        manager.paths().ensure().unwrap();
        let plan = crate::vm::manager::test_support::plan_for(&manager);
        MembershipManifest::from_plan(&plan)
            .save(&manager.paths().membership)
            .unwrap();
        assert!(manager.membership_warning().is_none());

        let mut changed = vm;
        changed.primary = Some("second".into());
        let changed = VmManager::new(config, changed).unwrap();
        let warning = changed
            .membership_warning()
            .expect("a changed primary must warn");
        assert!(warning.contains("primary project"), "{warning}");
        assert!(warning.contains("hegn vm apply demo"), "{warning}");
    }

    #[test]
    fn membership_warning_reports_an_unreadable_registry() {
        use crate::vm::manager::MembershipManifest;

        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config.clone(), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        MembershipManifest::from_plan(&crate::vm::manager::test_support::plan_for(&manager))
            .save(&paths.membership)
            .unwrap();
        std::fs::write(config.state_dir.join("projects.toml"), b"not = [valid").unwrap();

        let warning = manager
            .membership_warning()
            .expect("an unreadable registry must warn");
        assert!(warning.contains("cannot be verified"), "{warning}");
        assert!(
            warning.contains("project registry is unreadable"),
            "{warning}"
        );
        assert!(warning.contains("hegn vm apply demo"), "{warning}");
    }

    #[test]
    fn cloud_init_finished_detects_the_final_serial_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("serial.log");
        std::fs::write(
            &log,
            "Cloud-init v. 24.4 running 'modules:config' at Thu, 26 Nov 2026\n",
        )
        .unwrap();
        assert!(!cloud_init_finished(&log));
        std::fs::write(
            &log,
            "Cloud-init v. 24.4 finished at Thu, 26 Nov 2026 16:35:00 +0000\n",
        )
        .unwrap();
        assert!(cloud_init_finished(&log));
    }

    #[test]
    fn provision_state_detects_ready_already_and_failed() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("serial.log");
        std::fs::write(&log, "booting\n").unwrap();
        assert!(matches!(provision_state(&log), ProvisionState::Pending));
        std::fs::write(&log, "hegn: preset already provisioned\n").unwrap();
        assert!(matches!(provision_state(&log), ProvisionState::Ready));
        std::fs::write(&log, "hegn: preset ready\n").unwrap();
        assert!(matches!(provision_state(&log), ProvisionState::Ready));
        std::fs::write(&log, "hegn-progress: {\"stage\":\"failed\",\"code\":2}\n").unwrap();
        assert!(matches!(provision_state(&log), ProvisionState::Failed(_)));
    }

    #[test]
    fn apply_on_a_stopped_vm_starts_it() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        // The fixture has no VM binaries, so apply must reach start and fail
        // there rather than silently doing nothing.
        let err = manager.apply().unwrap_err().to_string();
        assert!(
            err.contains("VM log:") || err.contains("cloud-hypervisor"),
            "{err}"
        );
    }

    #[test]
    fn apply_forgets_the_guest_host_key_but_keeps_the_client_key() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let identity = SshIdentity::new(&manager.config().vms_dir, manager.vm().name.as_str());
        std::fs::create_dir_all(identity.dir()).unwrap();
        std::fs::write(identity.key(), b"client-key").unwrap();
        std::fs::write(identity.known_hosts(), b"demo ssh-ed25519 AAAA\n").unwrap();

        // The fixture has no VM binaries, so start fails after `apply` cleared
        // the host-key cache and before anything else could touch it.
        assert!(manager.apply().is_err());

        assert!(
            !identity.known_hosts().exists(),
            "a reboot may change the guest host key"
        );
        assert_eq!(
            std::fs::read(identity.key()).unwrap(),
            b"client-key",
            "the client key must survive apply"
        );
        assert_eq!(
            crate::vm::manager::seed_generation::read(manager.config(), manager.vm()).unwrap(),
            1,
            "apply must advance the generation so cloud-init rotates host keys"
        );
    }

    #[test]
    fn manager_exposes_v2_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config.clone(), dir.path());
        assert_eq!(manager.vm().name, "demo");
        assert_eq!(manager.vm_config().name, "demo");
        assert_eq!(manager.security_mode(), manager.vm().security);
        assert_eq!(manager.store().root(), config.vms_dir);
        assert_eq!(manager.registry().entries().len(), 1);
        let _ = VmConfig::default();
    }

    #[test]
    fn security_mode_comes_from_the_vm_not_the_global_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.security = SecurityMode::Off;
        let base = manager(config.clone(), dir.path());
        let mut vm = base.vm().clone();
        vm.security = SecurityMode::Enforce;
        let manager = VmManager::new(config, vm).unwrap();

        assert_eq!(manager.config().security, SecurityMode::Off);
        assert_eq!(manager.security_mode(), SecurityMode::Enforce);
    }
}
