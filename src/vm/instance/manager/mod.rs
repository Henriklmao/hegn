//! [`InstanceManager`] shared state, locking and cross-cutting operations.
//!
//! The manager owns the per-instance lock (`run/hegn.lock`) that serializes
//! lifecycle operations. Boot, stop, status and snapshot work live in the
//! sibling modules; they all share the private fields declared here.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::vm::instance_config::{InstanceConfig, InstanceStore};

use super::paths::InstancePaths;
use super::process::process_matches;
use super::session::SessionRecord;

mod cleanup;
mod snapshot;
mod start;
mod status;
mod stop;

#[derive(Debug, Clone)]
pub struct InstanceManager {
    config: Config,
    instance: InstanceConfig,
}

/// Exclusive lock held for the duration of a VM lifecycle operation.
///
/// Dropping it (including on an error path) releases the lock.
#[derive(Debug)]
struct RunLock {
    _file: File,
}

impl InstanceManager {
    pub fn new(config: Config, instance: InstanceConfig) -> Self {
        Self { config, instance }
    }

    pub fn instance(&self) -> &InstanceConfig {
        &self.instance
    }

    /// Persist a newly assigned guarded-network slice so later boots and
    /// snapshot restores use the same TAP device.
    fn persist_network_index(&self, index: u8) -> Result<()> {
        if self.instance.network_index == Some(index) {
            return Ok(());
        }
        let mut instance = self.instance.clone();
        instance.network_index = Some(index);
        InstanceStore::new(self.config.clone()).save(&instance)
    }

    pub fn paths(&self) -> InstancePaths {
        InstancePaths::from_instance(&self.config, &self.instance)
    }

    pub fn session(&self) -> Option<SessionRecord> {
        let raw = fs::read_to_string(self.paths().session).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// A session file that exists but cannot be parsed is a migration hazard:
    /// an old VMM may still be running, but hegn no longer knows its pid.
    /// Refuse to overwrite that state instead of leaking an unmanaged VM.
    fn unreadable_session(&self) -> bool {
        let path = self.paths().session;
        path.exists() && self.session().is_none()
    }

    pub fn is_running(&self) -> bool {
        self.session()
            .map(|session| {
                let binary = session
                    .vm_binary
                    .clone()
                    .unwrap_or_else(|| self.config.vm_binary());
                process_matches(session.vm_pid, &binary)
            })
            .unwrap_or(false)
    }

    /// Take the exclusive VM lifecycle lock.
    ///
    /// A second `hegn open`/`instance start` must not recreate the API or serial
    /// socket underneath a VM that is already booting.
    fn lock(&self, paths: &InstancePaths) -> Result<RunLock> {
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

    /// Attach the TUI/CLI terminal to the running guest serial console.
    pub fn attach_console(&self) -> Result<crate::session::console::VmConsole> {
        crate::session::console::VmConsole::attach(&self.config, &self.instance)
    }

    /// Boot a VM if needed, then attach its console.
    pub fn ensure_console(&self) -> Result<crate::session::console::VmConsole> {
        if !self.is_running() {
            self.start()?;
        }
        self.attach_console()
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;

    use super::*;
    use crate::config::Harness;
    use crate::vm::instance::test_support::{config_with_assets, manager};

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
    fn running_state_uses_the_recorded_vm_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = PathBuf::from("/definitely/missing/cloud-hypervisor");
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: dir.path().join("project"),
            workspace: dir.path().join("project"),
            instance: "demo".into(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid: std::process::id(),
            vm_binary: Some(std::env::current_exe().unwrap()),
            relay_pid: 0,
            virtiofs_pid: 0,
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
        assert!(manager.is_running());
    }
}
