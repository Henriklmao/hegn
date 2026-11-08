//! Instance boot: preflight checks, process spawning and session recording.
//!
//! `start_inner` is written as a sequence of fallible steps with explicit
//! rollback on every error path so a partial start never leaks a virtiofsd, a
//! VMM or a network lease.

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::agent::profile;
use crate::agent::provision::ProvisionPlan;
use crate::config::{Harness, VIRTIOFSD_FALLBACKS};
use crate::network;
use crate::session::logs::{DEFAULT_KEEP, LogStore};
use crate::session::snapshot::SnapshotStore;
use crate::vm::cloud_init;
use crate::vm::instance::autosave;
use crate::vm::virtiofs::VirtiofsProcess;

use super::super::args::{
    API_START_TIMEOUT, boot_args, spawn_cloud_hypervisor, spawn_virtiofsd, spawn_virtiofsd_at,
    start_relay, wait_for_vm,
};
use super::super::process::{hegn_binary, terminate_relay};
use super::super::session::{InstanceReport, SessionRecord};
use super::super::util::{append_event_log, endpoint_urls, write_private};
use super::InstanceManager;
use super::cleanup::cleanup_files;

impl InstanceManager {
    /// Boot virtiofsd, Cloud Hypervisor and the console relay.
    ///
    /// Lifecycle errors are appended to `run/hegn.log` so a failed start can
    /// be diagnosed even when the guest never reached userspace.
    pub fn start(&self) -> Result<InstanceReport> {
        let result = self.start_inner();
        let paths = self.paths();
        // `start_inner` may fail before it creates the run directory.
        let _ = paths.ensure();
        let message = match &result {
            Ok(report) => format!(
                "started instance={} project={} harness={} vm_pid={}",
                report.instance,
                report.workspace.display(),
                self.instance.harness.label(),
                report.vm_pid.unwrap_or(0)
            ),
            Err(err) => format!(
                "start failed instance={} project={}: {err:#}",
                self.instance.name,
                self.instance.workspace.display()
            ),
        };
        append_event_log(&paths.event_log, &message);
        result.map_err(|err| anyhow::anyhow!("{err:#} (VM log: {})", paths.event_log.display()))
    }

    fn start_inner(&self) -> Result<InstanceReport> {
        let paths = self.paths();
        // A missing VM is always a hard error: there is no host-process fallback.
        self.config.require_vm_assets()?;
        // The snapshot staging directory must exist before Cloud Hypervisor
        // starts: it is a Landlock rule, and Landlock rules are validated at
        // create time.
        self.config.ensure_dirs()?;
        SnapshotStore::new(&self.config.snapshots_dir).ensure_staging()?;
        let project = &self.instance.workspace;
        if !project.is_dir() {
            bail!("instance workspace {} does not exist", project.display());
        }
        // Preflight every binary before spawning anything, so a typo in the
        // configuration cannot leave a stray virtiofsd (or a VM without a
        // console) behind.
        let vm_binary = self.config.resolved_cloud_hypervisor().with_context(|| {
            format!(
                "cloud-hypervisor binary {} was not found; hegn also accepts `{}`; run `hegn doctor`",
                self.config.cloud_hypervisor_bin.display(),
                crate::config::CLOUD_HYPERVISOR_STATIC
            )
        })?;
        if self.config.resolved_virtiofsd().is_none() {
            bail!(
                "virtiofsd binary {} was not found on PATH or in {}; install virtiofsd or set \
                 `virtiofsd_bin` (run `hegn doctor`)",
                self.config.virtiofsd_bin.display(),
                VIRTIOFSD_FALLBACKS.join(", ")
            );
        }
        let hegn = hegn_binary()?;
        let _lock = self.lock(&paths)?;
        if self.unreadable_session() {
            bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 stop the old VM manually and remove that directory before starting",
                paths.run_dir.display()
            );
        }
        if self.is_running() {
            bail!("an instance is already running (use `hegn instance stop` first)");
        }
        // Clear stale sockets/pid files and any orphaned autosave worker before
        // restoring or booting. A live VM returned above via `is_running`.
        self.cleanup_stale();
        // Build the runtime before any spawn so the auto-resume attempt and the
        // cold boot share one async runtime.
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                return Err(err).context("failed to build async runtime");
            }
        };
        // Try the newest automatic checkpoint first: this is what turns a
        // restart into a resume instead of a full cold boot. A broken or
        // foreign checkpoint must not brick the instance, so failures fall
        // through to a normal boot and are recorded in the lifecycle log.
        if self.config.auto_resume {
            let store = SnapshotStore::new(&self.config.snapshots_dir);
            match store.latest_autosave(&self.instance.name) {
                Ok(Some(snapshot)) => {
                    append_event_log(
                        &paths.event_log,
                        &format!("auto-resuming from snapshot {}", snapshot.name),
                    );
                    match runtime.block_on(self.restore_snapshot_locked(&snapshot)) {
                        Ok(()) => {
                            append_event_log(
                                &paths.event_log,
                                &format!("auto-resume from {} succeeded", snapshot.name),
                            );
                            return self.status();
                        }
                        Err(err) => append_event_log(
                            &paths.event_log,
                            &format!(
                                "auto-resume from {} failed, cold booting: {err:#}",
                                snapshot.name
                            ),
                        ),
                    }
                }
                Ok(None) => {}
                Err(err) => append_event_log(
                    &paths.event_log,
                    &format!("warning: could not inspect automatic snapshots: {err:#}"),
                ),
            }
        }
        if self.config.network == crate::config::NetworkMode::Off
            && self.config.cloud_init
            && self.instance.harness != Harness::Shell
        {
            bail!(
                "harness '{}' needs the guarded network for its first-boot install; enable \
                 `network = \"guarded\"` or use a pre-provisioned rootfs with `cloud_init = false`",
                self.instance.harness.label()
            );
        }
        // Materialize the harness profile before reserving the network so a
        // bad profile cannot leave a TAP reservation behind.
        let profile_plan = profile::prepare_for_start(&self.config, &self.instance)?;
        for warning in &profile_plan.warnings {
            append_event_log(&paths.event_log, &format!("profile: {warning}"));
        }
        let mut packages = self.instance.extra_packages.clone();
        packages.extend(profile_plan.packages.iter().cloned());
        let provision = ProvisionPlan::new(self.instance.harness, &packages)?;
        // Reserve the guarded-network slice before any process is spawned.
        // `acquire` also verifies that the host firewall marker is present, so
        // an unconfigured guard fails before virtiofsd or the VMM can start.
        let network_lease = match self.config.network {
            crate::config::NetworkMode::Guarded => {
                Some(network::acquire(&self.config, &self.instance)?)
            }
            crate::config::NetworkMode::Off => None,
        };
        if let Some(lease) = &network_lease
            && let Err(err) = self.persist_network_index(lease.index)
        {
            network::release(&self.config, &self.instance.name);
            return Err(err);
        }
        if let Err(err) = paths.ensure() {
            network::release(&self.config, &self.instance.name);
            return Err(err);
        }

        let started_at = Utc::now();
        let logs = LogStore::new(self.config.logs_dir());
        if let Err(err) = logs.prune(DEFAULT_KEEP) {
            append_event_log(
                &paths.event_log,
                &format!("warning: could not prune old serial logs: {err:#}"),
            );
        }
        let serial_log = match logs.create_session(project, started_at, std::process::id()) {
            Ok(path) => path,
            Err(err) => {
                network::release(&self.config, &self.instance.name);
                return Err(err);
            }
        };

        // 1. virtiofsd, sandboxed to exactly this project.
        let mut virtiofs = match spawn_virtiofsd(
            &self.config,
            &paths,
            project,
            self.instance.read_only_workspace,
        ) {
            Ok(process) => process,
            Err(err) => {
                network::release(&self.config, &self.instance.name);
                return Err(err);
            }
        };
        let virtiofs_pid = virtiofs.pid();
        if let Err(err) = write_private(&paths.virtiofs_pid, virtiofs_pid.to_string()) {
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.instance.name);
            return Err(err).context("failed to record the virtiofsd pid");
        }

        // 1b. Optional second virtiofsd for the copied profile and skills.
        let mut profile_virtiofs = None;
        if let Some(share) = &profile_plan.share {
            match spawn_virtiofsd_at(
                &self.config,
                &share.bundle_dir,
                &paths.profile_socket,
                false,
            ) {
                Ok(process) => profile_virtiofs = Some(process),
                Err(err) => {
                    virtiofs.kill();
                    cleanup_files(&paths);
                    network::release(&self.config, &self.instance.name);
                    return Err(err);
                }
            }
            if let Some(process) = &profile_virtiofs
                && let Err(err) =
                    write_private(&paths.profile_virtiofs_pid, process.pid().to_string())
            {
                if let Some(mut process) = profile_virtiofs.take() {
                    process.kill();
                }
                virtiofs.kill();
                cleanup_files(&paths);
                network::release(&self.config, &self.instance.name);
                return Err(err).context("failed to record the profile virtiofsd pid");
            }
        }
        let profile_virtiofs_pid = profile_virtiofs
            .as_ref()
            .map(VirtiofsProcess::pid)
            .unwrap_or(0);

        // 2. Optional NoCloud seed for the official cloud images. It contains
        // no secrets and only enables a serial login (root unless credential
        // hardening is enabled).
        if self.config.cloud_init
            && let Err(err) = cloud_init::write_seed(
                &paths.cloud_init,
                &self.instance.shared_tag,
                &self.instance.guest_mount,
                &cloud_init::instance_id(
                    &self.instance.name,
                    &provision,
                    profile_plan.share.as_ref(),
                    network_lease.as_ref(),
                    &self.config.local_endpoints,
                    self.config.guest_credential_hardening,
                    self.instance.read_only_workspace,
                ),
                &provision,
                profile_plan.share.as_ref(),
                network_lease.as_ref(),
                &self.config.local_endpoints,
                self.config.guest_credential_hardening,
                self.instance.read_only_workspace,
            )
        {
            virtiofs.kill();
            if let Some(mut process) = profile_virtiofs.take() {
                process.kill();
            }
            cleanup_files(&paths);
            network::release(&self.config, &self.instance.name);
            return Err(err);
        }

        // 3. Cloud Hypervisor. Children outlive this CLI call.
        let mut vm = match spawn_cloud_hypervisor(
            &self.config,
            &self.instance,
            &paths,
            &vm_binary,
            None,
            network_lease.as_ref(),
            profile_plan
                .share
                .as_ref()
                .map(|_| paths.profile_socket.as_path()),
        ) {
            Ok(child) => child,
            Err(err) => {
                virtiofs.kill();
                if let Some(mut process) = profile_virtiofs.take() {
                    process.kill();
                }
                cleanup_files(&paths);
                network::release(&self.config, &self.instance.name);
                return Err(err);
            }
        };
        let vm_pid = vm.id();

        // 4. The relay is Cloud Hypervisor's only serial client. Start it
        // before waiting for the API so the earliest boot output is captured.
        let relay_pid = match start_relay(&paths, &serial_log, &mut vm) {
            Ok(pid) => pid,
            Err(err) => {
                let _ = vm.kill();
                let _ = vm.wait();
                virtiofs.kill();
                if let Some(mut process) = profile_virtiofs.take() {
                    process.kill();
                }
                cleanup_files(&paths);
                network::release(&self.config, &self.instance.name);
                return Err(err);
            }
        };

        if let Err(err) =
            runtime.block_on(wait_for_vm(&paths.api_socket, &mut vm, API_START_TIMEOUT))
        {
            let _ = vm.kill();
            let _ = vm.wait();
            terminate_relay(relay_pid, &hegn);
            virtiofs.kill();
            if let Some(mut process) = profile_virtiofs.take() {
                process.kill();
            }
            cleanup_files(&paths);
            network::release(&self.config, &self.instance.name);
            return Err(err).context("failed to boot Cloud Hypervisor");
        }

        let record = SessionRecord {
            project: self.instance.project.clone(),
            workspace: self.instance.workspace.clone(),
            instance: self.instance.name.clone(),
            harness: self.instance.harness,
            started_at,
            vm_pid,
            vm_binary: Some(vm_binary.clone()),
            relay_pid,
            virtiofs_pid,
            profile_virtiofs_pid,
            boot_args: boot_args(&self.config.root_device),
            serial_log: Some(serial_log.clone()),
        };
        let recorded = (|| -> Result<()> {
            write_private(&paths.vm_pid, vm_pid.to_string())?;
            write_private(&paths.relay_pid, relay_pid.to_string())?;
            write_private(&paths.session, serde_json::to_string_pretty(&record)?)?;
            Ok(())
        })();
        if let Err(err) = recorded {
            // Without the session record a later `stop` cannot find the VM, so
            // tear everything down instead of leaking a running VMM.
            let _ = vm.kill();
            let _ = vm.wait();
            terminate_relay(relay_pid, &hegn);
            virtiofs.kill();
            if let Some(mut process) = profile_virtiofs.take() {
                process.kill();
            }
            cleanup_files(&paths);
            network::release(&self.config, &self.instance.name);
            return Err(err).context("failed to record the instance session");
        }

        // Do not kill the children on drop.
        std::mem::forget(vm);
        std::mem::forget(virtiofs);
        if let Some(process) = profile_virtiofs.take() {
            std::mem::forget(process);
        }
        // Keep rolling checkpoints so the next start is a resume. This is a
        // helper for recovery, never a reason to fail a successful boot.
        if let Err(err) = autosave::spawn(&self.config, &self.instance, &paths) {
            append_event_log(
                &paths.event_log,
                &format!("autosnapshot: could not start worker: {err:#}"),
            );
        }

        Ok(InstanceReport {
            state: "running".into(),
            backend: "cloud-hypervisor".into(),
            instance: self.instance.name.clone(),
            project: Some(self.instance.project.clone()),
            workspace: self.instance.workspace.clone(),
            vcpus: self.instance.vcpus,
            memory_mib: self.instance.memory_mib,
            api_socket: paths.api_socket,
            serial_socket: paths.serial_socket,
            console_socket: paths.console_socket,
            virtiofs_socket: paths.virtiofs_socket,
            virtiofs_running: true,
            profile_share: profile_plan
                .share
                .as_ref()
                .map(|share| share.bundle_dir.clone()),
            profile_virtiofs_pid: (profile_virtiofs_pid != 0).then_some(profile_virtiofs_pid),
            profile_virtiofs_running: profile_virtiofs_pid != 0,
            relay_running: true,
            vm_pid: Some(vm_pid),
            relay_pid: Some(relay_pid),
            virtiofs_pid: Some(virtiofs_pid),
            started_at: Some(record.started_at),
            serial_log: Some(serial_log),
            local_endpoints: network_lease
                .as_ref()
                .map(|lease| endpoint_urls(&self.config, lease.host_ip))
                .unwrap_or_default(),
            note: Some(format!(
                "guest workspace mounted at {}",
                self.instance.guest_mount
            )),
        })
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::fs;

    use crate::vm::instance::test_support::{config_with_assets, manager, manager_for};

    #[test]
    fn start_requires_assets() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.kernel_image = dir.path().join("missing");
        let manager = manager(config, dir.path());
        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("missing VM assets"), "{err}");
    }

    #[test]
    fn start_preflights_binaries_before_spawning_anything() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();

        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("cloud-hypervisor binary"), "{err}");
        assert!(!manager.paths().virtiofs_pid.exists());
        assert!(!manager.paths().api_socket.exists());
    }

    #[test]
    fn start_requires_existing_project() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-project");
        let manager = manager_for(config_with_assets(dir.path()), &missing);
        std::fs::remove_dir_all(&missing).unwrap();
        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn start_reports_missing_virtiofsd() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        // Make the Cloud Hypervisor preflight pass so this test reaches virtiofsd.
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        let manager = manager(config, dir.path());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let err = manager.start().unwrap_err().to_string();
        assert!(
            err.contains("virtiofsd") || err.contains("failed to start"),
            "{err}"
        );
    }

    #[test]
    fn start_refuses_an_unreadable_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        let fake_virtiofsd = dir.path().join("virtiofsd");
        std::fs::write(&fake_virtiofsd, b"#!/bin/sh\n").unwrap();
        config.virtiofsd_bin = fake_virtiofsd;
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();

        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert_eq!(std::fs::read(&paths.session).unwrap(), b"{not json");
    }

    #[test]
    fn failed_start_leaves_a_private_lifecycle_log() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.kernel_image = dir.path().join("missing-kernel");
        let manager = manager(config, dir.path());
        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("VM log:"), "{err}");
        let log = manager.paths().event_log;
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("start failed"), "{text}");
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn start_refuses_a_second_boot_while_the_recorded_vm_is_alive() {
        use chrono::Utc;

        use crate::config::Harness;
        use crate::vm::instance::test_support::instance_for;
        use crate::vm::instance::{InstanceManager, SessionRecord};

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        config.virtiofsd_bin = std::env::current_exe().unwrap();
        let instance = instance_for(&dir.path().join("project"));
        let manager = InstanceManager::new(config, instance);
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: manager.instance().project.clone(),
            workspace: manager.instance().workspace.clone(),
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
        let raw = serde_json::to_string(&record).unwrap();
        fs::write(&paths.session, &raw).unwrap();

        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("already running"), "{err}");
        assert_eq!(
            fs::read_to_string(&paths.session).unwrap(),
            raw,
            "a refused boot must not touch the live session"
        );
        assert!(!paths.virtiofs_pid.exists());
    }

    #[test]
    fn start_refuses_a_harness_install_without_the_guarded_network() {
        use crate::config::Harness;
        use crate::vm::instance::InstanceManager;
        use crate::vm::instance::test_support::instance_for;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        config.virtiofsd_bin = std::env::current_exe().unwrap();
        config.network = crate::config::NetworkMode::Off;
        config.cloud_init = true;
        let mut instance = instance_for(&dir.path().join("project"));
        instance.harness = Harness::Codex;
        let manager = InstanceManager::new(config, instance);

        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("needs the guarded network"), "{err}");
        assert!(!manager.paths().virtiofs_pid.exists());
        assert!(!manager.paths().api_socket.exists());
    }
}
