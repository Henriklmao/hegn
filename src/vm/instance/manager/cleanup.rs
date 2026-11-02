//! Stale-session cleanup and runtime file removal.

use std::fs;

use crate::network;

use super::super::paths::InstancePaths;
use super::super::process::{hegn_binary, process_matches, terminate, terminate_relay};
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
        &paths.session,
        &paths.cloud_init,
    ] {
        let _ = fs::remove_file(path);
    }
}
