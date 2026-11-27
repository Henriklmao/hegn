//! VM shutdown: terminate the VMM, relay, every member virtiofsd, the profile
//! daemon and the autosave worker, release runtime state and the guarded
//! network reservation.
//!
//! Termination only happens after `process_matches` confirms that the recorded
//! PID still belongs to the expected executable. Workspaces are never removed.

use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::network;

use super::VmManager;
use super::cleanup::{cleanup_files, indexed_pids};
use super::process::{hegn_binary, read_pid, terminate, terminate_autosave, terminate_relay};

impl VmManager {
    /// Stop Cloud Hypervisor, the console relay, every member virtiofsd, the
    /// profile daemon and the autosave worker.
    ///
    /// Running SSH sessions end with the guest: the control TAP and its guest
    /// sshd disappear when the VMM exits. The per-VM keypair and `known_hosts`
    /// are intentionally kept so a later start can reuse the identity.
    pub fn stop(&self) -> Result<()> {
        let paths = self.paths();
        if !paths.run_dir.is_dir() {
            bail!("no VM session found in {}", paths.run_dir.display());
        }
        let _lock = self.lock(&paths)?;
        let hegn = hegn_binary()?;
        if self.unreadable_session() {
            super::repair::repair_unreadable_session_locked(&self.config, &self.vm, &paths)?;
            return Ok(());
        }
        let session = self.session();
        let vm_pid = session
            .as_ref()
            .map(|session| session.vm_pid)
            .or_else(|| read_pid(&paths.vm_pid));
        let relay_pid = session
            .as_ref()
            .map(|session| session.relay_pid)
            .filter(|pid| *pid != 0)
            .or_else(|| read_pid(&paths.relay_pid));
        let profile_virtiofs_pid = session
            .as_ref()
            .map(|session| session.profile_virtiofs_pid)
            .filter(|pid| *pid != 0)
            .or_else(|| read_pid(&paths.profile_virtiofs_pid));
        let autosave_pid = read_pid(&paths.autosave_pid);

        // Members are recorded in both the session and the run directory. The
        // union covers a crash between spawning a daemon and writing the
        // session record; deduplication prevents double signals.
        let mut member_pids = BTreeSet::new();
        if let Some(session) = &session {
            member_pids.extend(
                session
                    .virtiofs_pids
                    .iter()
                    .copied()
                    .filter(|pid| *pid != 0),
            );
        }
        member_pids.extend(indexed_pids(&paths).into_iter().map(|(_, pid)| pid));

        if vm_pid.is_none()
            && relay_pid.is_none()
            && member_pids.is_empty()
            && profile_virtiofs_pid.is_none()
            && autosave_pid.is_none()
        {
            bail!("no VM session found in {}", paths.run_dir.display());
        }
        let vm_binary = self.trusted_vm_binary(session.as_ref().and_then(|s| s.vm_binary.as_ref()));

        if let Some(pid) = vm_pid {
            terminate(pid, &vm_binary);
        }
        if let Some(pid) = relay_pid {
            terminate_relay(pid, &hegn);
        }
        for pid in member_pids {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        if let Some(pid) = profile_virtiofs_pid {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        if let Some(pid) = autosave_pid {
            terminate_autosave(pid, &hegn);
        }
        cleanup_files(&paths);
        network::release(&self.config, &self.vm.name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::vm::manager::test_support::{config_with_assets, manager};

    #[test]
    fn stop_archives_an_unreadable_session() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();

        manager.stop().unwrap();
        assert!(!paths.session.exists());
        let archive = std::fs::read_dir(&paths.run_dir)
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().starts_with("stale-"))
            .expect("archive directory");
        assert_eq!(
            std::fs::read(archive.path().join("session.json")).unwrap(),
            b"{not json"
        );
    }

    #[test]
    fn stop_without_session_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        assert!(manager.stop().is_err());
        assert!(!manager.is_running());
    }

    #[test]
    fn stop_without_a_run_directory_names_the_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        let err = manager.stop().unwrap_err().to_string();
        assert!(err.contains("no VM session found"), "{err}");
        assert!(err.contains(&paths.run_dir.display().to_string()), "{err}");
        assert!(!paths.run_dir.exists(), "stop must not create state");
    }

    #[test]
    fn stop_removes_runtime_state_but_keeps_the_workspace() {
        use chrono::Utc;

        use crate::config::Harness;
        use crate::vm::manager::SessionRecord;

        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"hello\n").unwrap();
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: project.clone(),
            workspace: project.clone(),
            vm: "demo".into(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            // Zero disables termination; this test exercises cleanup only.
            vm_pid: 0,
            vm_binary: None,
            relay_pid: 0,
            virtiofs_pids: vec![0, 0],
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
            guest_ip: None,
            ssh_user: None,
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
        std::fs::write(&paths.vm_pid, b"0").unwrap();
        std::fs::write(&paths.cloud_init, b"seed").unwrap();
        std::fs::write(&paths.membership, b"{}").unwrap();
        std::fs::write(paths.virtiofs_pid(0), b"0").unwrap();

        manager.stop().unwrap();

        assert!(!paths.session.exists());
        assert!(!paths.vm_pid.exists());
        assert!(!paths.cloud_init.exists());
        assert!(!paths.membership.exists());
        assert!(!paths.virtiofs_pid(0).exists());
        assert!(project.join("README.md").is_file());
    }

    #[test]
    fn stop_ignores_pid_files_that_are_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        // Parses as a u32 but is not a valid positive pid_t, so termination is
        // skipped and only stale runtime files are removed.
        std::fs::write(&paths.vm_pid, b"4000000000").unwrap();

        manager.stop().unwrap();
        assert!(!paths.vm_pid.exists());
    }

    #[test]
    fn stop_kills_every_indexed_member_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        // Use a real host process as the "virtiofsd" and point the trusted
        // binary at the same executable; the identity check then permits the
        // signal instead of refusing an unrelated process.
        config.virtiofsd_bin = std::path::PathBuf::from("/bin/sleep");
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();

        let mut children: Vec<std::process::Child> = (0..3)
            .map(|_| {
                std::process::Command::new("/bin/sleep")
                    .arg("30")
                    .spawn()
                    .expect("sleep must be available")
            })
            .collect();
        for (index, child) in children.iter().enumerate() {
            std::fs::write(paths.virtiofs_pid(index), child.id().to_string()).unwrap();
        }

        manager.stop().unwrap();
        for (index, child) in children.iter_mut().enumerate() {
            let status = child.wait().unwrap();
            assert!(
                !status.success(),
                "member {index} should have been terminated: {status}"
            );
            assert!(!paths.virtiofs_pid(index).exists());
        }
    }
}
