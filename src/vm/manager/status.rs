//! VM inspection: status reports, VM info and pause/resume controls.

use anyhow::Result;

use crate::network;
use crate::session::relay;
use crate::vm::hypervisor::{CloudHypervisorClient, VmInfo, VmState};

use super::VmManager;
use super::process::{ensure_socket, hegn_binary, process_matches};
use super::session::VmReport;
use super::util::{endpoint_urls, profile_bundle_dir};

impl VmManager {
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

    pub async fn vm_info(&self) -> Result<VmInfo> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        CloudHypervisorClient::new(&paths.api_socket).info().await
    }

    pub fn status(&self) -> Result<VmReport> {
        let paths = self.paths();
        let session = self.session();
        let membership = self.membership();
        let virtiofs_pids: Vec<u32> = session
            .as_ref()
            .map(|session| session.virtiofs_pids.clone())
            .unwrap_or_default();
        let virtiofs_running = !virtiofs_pids.is_empty()
            && virtiofs_pids
                .iter()
                .all(|pid| process_matches(*pid, &self.config.virtiofsd_bin));
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
                "session metadata is unreadable or from an older hegn version; \
                 the stale run state is archived automatically on the next start or stop"
                    .into(),
            );
        }

        let members = membership
            .as_ref()
            .map(|manifest| manifest.members.len())
            .or_else(|| session.as_ref().map(|session| session.membership.len()))
            .unwrap_or(0);
        let primary_path = membership
            .as_ref()
            .and_then(|manifest| {
                manifest
                    .members
                    .iter()
                    .find(|member| member.name == manifest.primary)
                    .map(|member| member.path.clone())
            })
            .or_else(|| {
                crate::project::ProjectRegistry::load(&self.config.state_dir)
                    .ok()
                    .and_then(|registry| {
                        let exported = registry.exported_members(&self.vm.name);
                        exported
                            .iter()
                            .find(|entry| self.vm.primary.as_deref() == Some(entry.name.as_str()))
                            .or_else(|| exported.first())
                            .map(|entry| entry.path.clone())
                    })
            });
        Ok(VmReport {
            state,
            backend: "cloud-hypervisor".into(),
            vm: self.vm.name.clone(),
            primary: self.vm.primary.clone(),
            members,
            project: primary_path.or_else(|| {
                session
                    .as_ref()
                    .map(|session| session.project.clone())
                    .filter(|path| !path.as_os_str().is_empty())
            }),
            vcpus: self.vm.vcpus,
            memory_mib: self.vm.memory_mib,
            api_socket: paths.api_socket,
            serial_socket: paths.serial_socket,
            console_socket: paths.console_socket,
            virtiofs_running,
            virtiofs_pids,
            profile_share: profile_bundle_dir(&self.config, &self.vm).filter(|dir| dir.is_dir()),
            profile_virtiofs_pid: session
                .as_ref()
                .map(|s| s.profile_virtiofs_pid)
                .filter(|pid| *pid != 0),
            profile_virtiofs_running,
            relay_running,
            vm_pid: session.as_ref().map(|s| s.vm_pid),
            relay_pid: session.as_ref().map(|s| s.relay_pid),
            started_at: session.as_ref().map(|s| s.started_at),
            serial_log: session.as_ref().and_then(|s| s.serial_log.clone()),
            local_endpoints: if self.config.network == crate::config::NetworkMode::Off {
                // Off mode has no data TAP, so no host endpoint is reachable.
                Vec::new()
            } else {
                self.vm
                    .network_index
                    .map(|index| endpoint_urls(&self.config, network::host_ip(index)))
                    .unwrap_or_default()
            },
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
    use chrono::Utc;

    use super::*;
    use crate::config::Harness;
    use crate::vm::config::VmConfig;
    use crate::vm::manager::SessionRecord;
    use crate::vm::manager::test_support::{config_with_assets, manager, manager_for};

    fn write_session(manager: &VmManager, vm_pid: u32) {
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: manager.paths().run_dir,
            workspace: std::path::PathBuf::new(),
            vm: manager.vm().name.clone(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid,
            vm_binary: None,
            relay_pid: 0,
            virtiofs_pids: vec![0],
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
            guest_ip: None,
            ssh_user: None,
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
    }

    #[test]
    fn status_without_session_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let report = manager.status().unwrap();
        assert_eq!(report.state, "stopped");
        assert_eq!(report.vm, "demo");
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
        assert_eq!(json["vm"], "demo");
    }

    #[test]
    fn status_reports_an_unreadable_session_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();

        let report = manager.status().unwrap();
        assert_eq!(report.state, "failed");
        assert!(
            report
                .note
                .as_deref()
                .is_some_and(|note| note.contains("unreadable")),
            "{:?}",
            report.note
        );
        assert!(paths.session.exists(), "diagnostic state must be preserved");
    }

    #[test]
    fn status_notes_a_session_whose_api_socket_disappeared() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        write_session(&manager, 0);

        let report = manager.status().unwrap();
        assert_eq!(report.state, "stopped");
        assert!(
            report
                .note
                .as_deref()
                .is_some_and(|note| note.contains("API socket is gone")),
            "{:?}",
            report.note
        );
        assert_eq!(report.vm_pid, Some(0));
    }

    #[test]
    fn api_operations_require_a_socket_and_never_boot_a_vm() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for err in [
            runtime.block_on(manager.pause()).unwrap_err(),
            runtime.block_on(manager.resume()).unwrap_err(),
            runtime.block_on(manager.vm_info()).unwrap_err(),
        ] {
            let err = err.to_string();
            assert!(err.contains("no Cloud Hypervisor API socket"), "{err}");
        }
        assert!(!manager.paths().api_socket.exists());
        assert!(!manager.is_running());
    }

    #[test]
    fn running_is_false_without_an_api_socket() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        assert!(!manager.paths().api_socket.exists());
        assert!(!manager.running());
    }

    #[test]
    fn status_exposes_the_profile_bundle_only_when_populated() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let bundle = manager
            .paths()
            .run_dir
            .parent()
            .unwrap()
            .join("profile/bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        assert_eq!(manager.status().unwrap().profile_share, None);

        std::fs::write(bundle.join("manifest"), b"{}").unwrap();
        assert_eq!(manager.status().unwrap().profile_share, Some(bundle));
    }

    #[test]
    fn status_exposes_local_endpoints_only_with_a_network_slice() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.local_endpoints = vec![crate::config::LocalEndpoint {
            name: "ollama".into(),
            port: 11434,
        }];
        let manager = manager(config.clone(), dir.path());
        let mut vm = manager.vm().clone();
        vm.network_index = Some(2);
        let manager = crate::vm::manager::VmManager::new(config, vm).unwrap();

        let report = manager.status().unwrap();
        assert_eq!(report.local_endpoints, vec!["http://10.213.0.9:11434"]);

        let manager = manager_for(
            crate::vm::manager::test_support::config_with_assets(dir.path()),
            &dir.path().join("project"),
        );
        assert!(manager.status().unwrap().local_endpoints.is_empty());
        let _ = VmConfig::default();
    }

    #[test]
    fn status_hides_local_endpoints_in_off_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.network = crate::config::NetworkMode::Off;
        config.local_endpoints = vec![crate::config::LocalEndpoint {
            name: "ollama".into(),
            port: 11434,
        }];
        let manager = manager(config.clone(), dir.path());
        let mut vm = manager.vm().clone();
        vm.network_index = Some(2);
        let manager = crate::vm::manager::VmManager::new(config, vm).unwrap();

        assert!(
            manager.status().unwrap().local_endpoints.is_empty(),
            "off mode has no data TAP to expose endpoints on"
        );
    }
}
