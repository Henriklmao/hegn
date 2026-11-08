//! Stale-session cleanup and runtime file removal.

use std::fs;

use crate::network;

use super::super::paths::InstancePaths;
use super::super::process::{
    hegn_binary, process_matches, read_pid, terminate, terminate_autosave, terminate_relay,
};
use super::InstanceManager;

impl InstanceManager {
    pub(super) fn cleanup_stale(&self) {
        let paths = self.paths();
        if let Some(session) = self.session() {
            let binary = self.config.vm_binary();
            if process_matches(session.vm_pid, &binary) {
                return;
            }
            terminate(session.vm_pid, &binary);
            if let Ok(binary) = hegn_binary() {
                terminate_relay(session.relay_pid, &binary);
            }
            terminate(session.virtiofs_pid, &self.config.virtiofsd_bin);
            if session.profile_virtiofs_pid != 0 {
                terminate(session.profile_virtiofs_pid, &self.config.virtiofsd_bin);
            }
        }
        // The worker may outlive a crashed session; terminate it before the
        // next VM starts so it cannot checkpoint the wrong instance.
        if let Ok(binary) = hegn_binary()
            && let Some(pid) = read_pid(&paths.autosave_pid)
        {
            terminate_autosave(pid, &binary);
        }
        cleanup_files(&paths);
        network::release(&self.config, &self.instance.name);
    }
}

/// Remove the run-time sockets, pid files and the session record.
///
/// Best effort: a missing file is fine, but this intentionally never removes
/// paths that do not belong to the instance.
pub(super) fn cleanup_files(paths: &InstancePaths) {
    for path in [
        &paths.api_socket,
        &paths.serial_socket,
        &paths.console_socket,
        &paths.virtiofs_socket,
        &paths.profile_socket,
        &paths.vm_pid,
        &paths.relay_pid,
        &paths.virtiofs_pid,
        &paths.profile_virtiofs_pid,
        &paths.autosave_pid,
        &paths.session,
        &paths.cloud_init,
    ] {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::config::Harness;
    use crate::vm::instance::test_support::{config_with_assets, manager};

    fn fake_session(vm_pid: u32) -> crate::vm::instance::SessionRecord {
        crate::vm::instance::SessionRecord {
            project: std::path::PathBuf::from("/projects/demo"),
            workspace: std::path::PathBuf::from("/projects/demo"),
            instance: "demo".into(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid,
            vm_binary: None,
            relay_pid: 0,
            virtiofs_pid: 0,
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
        }
    }

    #[test]
    fn cleanup_files_removes_only_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance_dir = config.instances_dir.join("demo");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"hello\n").unwrap();
        std::fs::create_dir_all(&instance_dir).unwrap();
        let rootfs = instance_dir.join("rootfs.qcow2");
        std::fs::write(&rootfs, b"rootfs").unwrap();

        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let listeners: Vec<_> = [
            &paths.api_socket,
            &paths.serial_socket,
            &paths.console_socket,
            &paths.virtiofs_socket,
            &paths.profile_socket,
        ]
        .into_iter()
        .map(|socket| std::os::unix::net::UnixListener::bind(socket).unwrap())
        .collect();
        for path in [
            &paths.vm_pid,
            &paths.relay_pid,
            &paths.virtiofs_pid,
            &paths.profile_virtiofs_pid,
            &paths.autosave_pid,
            &paths.session,
            &paths.cloud_init,
        ] {
            std::fs::write(path, b"runtime").unwrap();
        }

        cleanup_files(&paths);

        for path in [
            &paths.api_socket,
            &paths.serial_socket,
            &paths.console_socket,
            &paths.virtiofs_socket,
            &paths.profile_socket,
            &paths.vm_pid,
            &paths.relay_pid,
            &paths.virtiofs_pid,
            &paths.profile_virtiofs_pid,
            &paths.autosave_pid,
            &paths.session,
            &paths.cloud_init,
        ] {
            assert!(!path.exists(), "{} was not removed", path.display());
        }
        assert!(project.join("README.md").is_file());
        assert_eq!(std::fs::read(&rootfs).unwrap(), b"rootfs");
        // Diagnostics are intentionally kept for post-mortems.
        assert!(paths.event_log.parent().unwrap().is_dir());
        drop(listeners);
    }

    #[test]
    fn cleanup_files_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{}").unwrap();
        cleanup_files(&paths);
        cleanup_files(&paths);
        assert!(!paths.session.exists());
    }

    #[test]
    fn cleanup_stale_keeps_the_state_of_a_live_vm() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = fake_session(std::process::id());
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
        std::fs::write(&paths.vm_pid, std::process::id().to_string()).unwrap();

        manager.cleanup_stale();

        assert!(
            paths.session.exists(),
            "a live VMM must never lose its session record"
        );
        assert!(paths.vm_pid.exists());
    }

    #[test]
    fn cleanup_stale_removes_stale_runtime_without_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("keep.txt"), b"keep").unwrap();
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.vm_pid, b"999999999").unwrap();
        std::fs::write(&paths.cloud_init, b"seed").unwrap();

        manager.cleanup_stale();

        assert!(!paths.vm_pid.exists());
        assert!(!paths.cloud_init.exists());
        assert!(project.join("keep.txt").is_file());
    }
}
