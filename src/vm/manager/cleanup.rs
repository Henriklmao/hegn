//! Stale-session cleanup and runtime file removal.

use std::fs;

use crate::network;

use super::VmManager;
use super::paths::VmPaths;
use super::process::{
    hegn_binary, process_matches, read_pid, terminate, terminate_autosave, terminate_relay,
};

impl VmManager {
    pub(super) fn cleanup_stale(&self) {
        let paths = self.paths();
        let binary = self.config.vm_binary();
        if let Some(session) = self.session() {
            if process_matches(session.vm_pid, &binary) {
                return;
            }
            terminate(session.vm_pid, &binary);
            if let Ok(binary) = hegn_binary() {
                terminate_relay(session.relay_pid, &binary);
            }
            for pid in &session.virtiofs_pids {
                terminate(*pid, &self.config.virtiofsd_bin);
            }
            if session.profile_virtiofs_pid != 0 {
                terminate(session.profile_virtiofs_pid, &self.config.virtiofsd_bin);
            }
        }
        terminate_indexed_members(&paths, &self.config.virtiofsd_bin);
        // The worker may outlive a crashed session; terminate it before the
        // next VM starts so it cannot checkpoint the wrong VM.
        if let Ok(binary) = hegn_binary()
            && let Some(pid) = read_pid(&paths.autosave_pid)
        {
            terminate_autosave(pid, &binary);
        }
        cleanup_files(&paths);
        network::release(&self.config, &self.vm.name);
    }
}

/// Pid files written for member index `i`: `virtiofs-<i>.pid`.
pub(super) fn indexed_pids(paths: &VmPaths) -> Vec<(usize, u32)> {
    let mut pids = Vec::new();
    let Ok(entries) = fs::read_dir(&paths.run_dir) else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(index) = name
            .strip_prefix("virtiofs-")
            .and_then(|rest| rest.strip_suffix(".pid"))
            .and_then(|index| index.parse::<usize>().ok())
        else {
            continue;
        };
        // A planted symlink must not redirect the pid read; the identity check
        // would still protect termination, but skipping is cheaper and exact.
        if fs::symlink_metadata(entry.path())
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(true)
        {
            continue;
        }
        if let Some(pid) = read_pid(&entry.path()) {
            pids.push((index, pid));
        }
    }
    pids.sort_unstable_by_key(|(index, _)| *index);
    pids
}

/// Terminate every indexed member daemon recorded in the run directory.
pub(super) fn terminate_indexed_members(paths: &VmPaths, binary: &std::path::Path) {
    for (_, pid) in indexed_pids(paths) {
        terminate(pid, binary);
    }
}

/// Remove the run-time sockets, pid files and the session/membership records.
///
/// Best effort: a missing file is fine, but this intentionally never removes
/// paths that do not belong to the VM. Logs and workspaces are kept.
pub(super) fn cleanup_files(paths: &VmPaths) {
    for path in [
        &paths.api_socket,
        &paths.serial_socket,
        &paths.console_socket,
        &paths.profile_socket,
        &paths.vm_pid,
        &paths.relay_pid,
        &paths.profile_virtiofs_pid,
        &paths.autosave_pid,
        &paths.session,
        &paths.membership,
        &paths.cloud_init,
    ] {
        let _ = fs::remove_file(path);
    }
    if let Ok(entries) = fs::read_dir(&paths.run_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("virtiofs-") && (name.ends_with(".sock") || name.ends_with(".pid"))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::config::Harness;
    use crate::vm::manager::SessionRecord;
    use crate::vm::manager::test_support::{config_with_assets, manager};

    fn fake_session(vm_pid: u32) -> SessionRecord {
        SessionRecord {
            project: std::path::PathBuf::from("/projects/demo"),
            workspace: std::path::PathBuf::from("/projects/demo"),
            vm: "demo".into(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid,
            vm_binary: None,
            relay_pid: 0,
            virtiofs_pids: vec![0, 0],
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
            guest_ip: None,
            ssh_user: None,
        }
    }

    #[test]
    fn cleanup_files_removes_only_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let vm_dir = config.vms_dir.join("demo");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"hello\n").unwrap();
        std::fs::create_dir_all(&vm_dir).unwrap();
        let rootfs = vm_dir.join("rootfs.qcow2");
        std::fs::write(&rootfs, b"rootfs").unwrap();

        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let listeners: Vec<_> = [
            paths.virtiofs_socket(0),
            paths.virtiofs_socket(1),
            paths.profile_socket.clone(),
        ]
        .into_iter()
        .map(|socket| std::os::unix::net::UnixListener::bind(socket).unwrap())
        .collect();
        for path in [
            &paths.api_socket,
            &paths.serial_socket,
            &paths.console_socket,
        ] {
            std::os::unix::net::UnixListener::bind(path).unwrap();
        }
        for path in [
            &paths.vm_pid,
            &paths.relay_pid,
            &paths.profile_virtiofs_pid,
            &paths.autosave_pid,
            &paths.session,
            &paths.membership,
            &paths.cloud_init,
            &paths.virtiofs_pid(0),
            &paths.virtiofs_pid(1),
        ] {
            std::fs::write(path, b"runtime").unwrap();
        }

        cleanup_files(&paths);

        for path in [
            &paths.api_socket,
            &paths.serial_socket,
            &paths.console_socket,
            &paths.virtiofs_socket(0),
            &paths.virtiofs_socket(1),
            &paths.profile_socket,
            &paths.vm_pid,
            &paths.relay_pid,
            &paths.profile_virtiofs_pid,
            &paths.autosave_pid,
            &paths.session,
            &paths.membership,
            &paths.cloud_init,
            &paths.virtiofs_pid(0),
            &paths.virtiofs_pid(1),
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
    fn indexed_pids_reads_every_member_and_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(paths.virtiofs_pid(0), b"111").unwrap();
        std::fs::write(paths.virtiofs_pid(3), b"333").unwrap();
        std::fs::write(paths.run_dir.join("virtiofs-notes.pid"), b"999").unwrap();
        let target = dir.path().join("target.pid");
        std::fs::write(&target, b"444").unwrap();
        std::os::unix::fs::symlink(&target, paths.virtiofs_pid(1)).unwrap();

        assert_eq!(indexed_pids(&paths), vec![(0, 111), (3, 333)]);
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
        std::fs::write(paths.virtiofs_pid(0), b"999999999").unwrap();

        manager.cleanup_stale();

        assert!(!paths.vm_pid.exists());
        assert!(!paths.cloud_init.exists());
        assert!(!paths.virtiofs_pid(0).exists());
        assert!(project.join("keep.txt").is_file());
    }
}
