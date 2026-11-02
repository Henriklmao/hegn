//! Instance shutdown: terminate the VMM, relay and virtiofsd, release runtime
//! state and the guarded-network reservation.
//!
//! Termination only happens after `super::super::process::process_matches`
//! confirms that the recorded PID still belongs to the expected executable.

use anyhow::{Result, bail};

use crate::network;

use super::super::process::{hegn_binary, read_pid, terminate, terminate_relay};
use super::InstanceManager;
use super::cleanup::cleanup_files;

impl InstanceManager {
    /// Stop Cloud Hypervisor, the console relay and virtiofsd.
    pub fn stop(&self) -> Result<()> {
        let paths = self.paths();
        if !paths.run_dir.is_dir() {
            bail!("no instance session found in {}", paths.run_dir.display());
        }
        let _lock = self.lock(&paths)?;
        let hegn = hegn_binary()?;
        if self.unreadable_session() {
            bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 find and stop the recorded VMM manually, then remove that directory",
                paths.run_dir.display()
            );
        }
        let session = self.session();
        let vm_pid = session
            .as_ref()
            .map(|s| s.vm_pid)
            .or_else(|| read_pid(&paths.vm_pid));
        let relay_pid = session
            .as_ref()
            .map(|s| s.relay_pid)
            .filter(|pid| *pid != 0)
            .or_else(|| read_pid(&paths.relay_pid));
        let virtiofs_pid = session
            .as_ref()
            .map(|s| s.virtiofs_pid)
            .or_else(|| read_pid(&paths.virtiofs_pid));
        let profile_virtiofs_pid = session
            .as_ref()
            .map(|s| s.profile_virtiofs_pid)
            .filter(|pid| *pid != 0)
            .or_else(|| read_pid(&paths.profile_virtiofs_pid));

        if vm_pid.is_none()
            && relay_pid.is_none()
            && virtiofs_pid.is_none()
            && profile_virtiofs_pid.is_none()
        {
            bail!("no instance session found in {}", paths.run_dir.display());
        }
        let vm_binary = session
            .as_ref()
            .and_then(|session| session.vm_binary.clone())
            .unwrap_or_else(|| self.config.vm_binary());

        if let Some(pid) = vm_pid {
            terminate(pid, &vm_binary);
        }
        if let Some(pid) = relay_pid {
            terminate_relay(pid, &hegn);
        }
        if let Some(pid) = virtiofs_pid {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        if let Some(pid) = profile_virtiofs_pid {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        cleanup_files(&paths);
        network::release(&self.config, &self.instance.name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::vm::instance::test_support::{config_with_assets, manager};

    #[test]
    fn stop_refuses_an_unreadable_session() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();

        let err = manager.stop().unwrap_err().to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert!(
            paths.session.exists(),
            "unreadable metadata must be preserved"
        );
    }

    #[test]
    fn stop_without_session_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        assert!(manager.stop().is_err());
        assert!(!manager.is_running());
    }
}
