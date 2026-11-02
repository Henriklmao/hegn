//! Instance inspection: status reports, VM info and pause/resume controls.

use anyhow::Result;

use crate::network;
use crate::session::relay;
use crate::vm::hypervisor::{CloudHypervisorClient, VmInfo, VmState};

use super::super::process::{ensure_socket, hegn_binary, process_matches};
use super::super::session::InstanceReport;
use super::super::util::{endpoint_urls, profile_bundle_dir};
use super::InstanceManager;

impl InstanceManager {
    /// Pause the guest (used before snapshot creation).
    pub async fn pause(&self) -> Result<()> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        CloudHypervisorClient::new(&paths.api_socket).pause().await
    }

    pub async fn resume(&self) -> Result<()> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        CloudHypervisorClient::new(&paths.api_socket).resume().await
    }

    pub async fn instance_info(&self) -> Result<VmInfo> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        CloudHypervisorClient::new(&paths.api_socket).info().await
    }

    pub fn status(&self) -> Result<InstanceReport> {
        let paths = self.paths();
        let session = self.session();
        let virtiofs_running = session
            .as_ref()
            .map(|s| process_matches(s.virtiofs_pid, &self.config.virtiofsd_bin))
            .unwrap_or(false);
        let profile_virtiofs_running = session
            .as_ref()
            .filter(|s| s.profile_virtiofs_pid != 0)
            .map(|s| process_matches(s.profile_virtiofs_pid, &self.config.virtiofsd_bin))
            .unwrap_or(false);
        let hegn = hegn_binary().ok();
        let relay_running = session
            .as_ref()
            .filter(|s| s.relay_pid != 0)
            .map(|s| {
                hegn.as_deref()
                    .is_some_and(|binary| relay::process_matches_relay(s.relay_pid, binary))
            })
            .unwrap_or(false);
        let api_socket_exists = paths.api_socket.exists();
        let mut state = if api_socket_exists {
            "unknown"
        } else {
            "stopped"
        }
        .to_string();
        let mut note = None;

        if api_socket_exists {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            match runtime.block_on(CloudHypervisorClient::new(&paths.api_socket).info()) {
                Ok(info) => {
                    state = match info.state {
                        VmState::Running => "running",
                        VmState::Paused | VmState::BreakPoint => "paused",
                        VmState::Created => "created",
                        VmState::Shutdown => "stopped",
                    }
                    .to_string();
                }
                Err(err) => {
                    state = "failed".into();
                    note = Some(err.to_string());
                }
            }
        } else if session.is_some() {
            state = "stopped".into();
            note = Some("session metadata exists but the API socket is gone".into());
        }
        if self.unreadable_session() {
            state = "failed".into();
            note = Some(
                "session metadata is unreadable or from an older hegn version; stop the old VM \
                 and remove the run directory"
                    .into(),
            );
        }

        Ok(InstanceReport {
            state,
            backend: "cloud-hypervisor".into(),
            instance: self.instance.name.clone(),
            project: session
                .as_ref()
                .map(|s| s.project.clone())
                .or_else(|| Some(self.instance.project.clone())),
            workspace: self.instance.workspace.clone(),
            vcpus: self.instance.vcpus,
            memory_mib: self.instance.memory_mib,
            api_socket: paths.api_socket,
            serial_socket: paths.serial_socket,
            console_socket: paths.console_socket,
            virtiofs_socket: paths.virtiofs_socket,
            virtiofs_running,
            profile_share: profile_bundle_dir(&self.config, &self.instance)
                .filter(|dir| dir.is_dir()),
            profile_virtiofs_pid: session
                .as_ref()
                .map(|s| s.profile_virtiofs_pid)
                .filter(|pid| *pid != 0),
            profile_virtiofs_running,
            relay_running,
            vm_pid: session.as_ref().map(|s| s.vm_pid),
            relay_pid: session.as_ref().map(|s| s.relay_pid),
            virtiofs_pid: session.as_ref().map(|s| s.virtiofs_pid),
            started_at: session.as_ref().map(|s| s.started_at),
            serial_log: session.as_ref().and_then(|s| s.serial_log.clone()),
            local_endpoints: self
                .instance
                .network_index
                .map(|index| endpoint_urls(&self.config, network::host_ip(index)))
                .unwrap_or_default(),
            note,
        })
    }

    /// Is the VM running or paused and answering the API?
    pub fn running(&self) -> bool {
        let paths = self.paths();
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return false,
        };
        runtime.block_on(async {
            CloudHypervisorClient::new(&paths.api_socket)
                .info()
                .await
                .map(|info| info.state.is_alive())
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::vm::instance::test_support::{config_with_assets, manager};

    #[test]
    fn status_without_session_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let report = manager.status().unwrap();
        assert_eq!(report.state, "stopped");
        assert!(!report.virtiofs_running);
        assert!(!report.relay_running);
        assert_eq!(report.project, Some(dir.path().join("project")));
    }

    #[test]
    fn report_serializes() {
        let dir = tempfile::tempdir().unwrap();
        let report = manager(config_with_assets(dir.path()), dir.path())
            .status()
            .unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["state"], "stopped");
        assert_eq!(json["backend"], "cloud-hypervisor");
    }
}
