//! VM boot: preflight checks, one virtiofsd per member, and session recording.
//!
//! `start_inner` is written as a sequence of fallible steps with an explicit
//! rollback guard so a partial start never leaks a virtiofsd, a VMM or a
//! network lease.

use std::path::{Path, PathBuf};
use std::process::Child;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::agent::profile::{self, ProfilePlan};
use crate::agent::provision::ProvisionPlan;
use crate::config::{Config, Harness, VIRTIOFSD_FALLBACKS};
use crate::network;
use crate::project::ProjectRegistry;
use crate::session::logs::{DEFAULT_KEEP, LogStore};
use crate::session::snapshot::{SnapshotMeta, SnapshotStore};
use crate::vm::cloud_init;
use crate::vm::config::{VmConfig, VmStore};
use crate::vm::manager::autosave;
use crate::vm::rootfs::prepare_rootfs;
use crate::vm::virtiofs::VirtiofsProcess;

use super::BootPlan;
use super::VmManager;
use super::args::{
    API_START_TIMEOUT, boot_args, spawn_cloud_hypervisor, spawn_virtiofsd_at, start_relay,
    wait_for_vm,
};
use super::cleanup::{cleanup_files, terminate_indexed_members};
use super::membership::MembershipManifest;
use super::paths::VmPaths;
use super::process::{hegn_binary, terminate_relay};
use super::session::{SessionRecord, VmReport};
use super::util::{append_event_log, endpoint_urls, write_private};

impl VmManager {
    /// Boot every exported project, Cloud Hypervisor and the console relay.
    ///
    /// Lifecycle errors are appended to `run/hegn.log` so a failed start can
    /// be diagnosed even when the guest never reached userspace.
    pub fn start(&self) -> Result<VmReport> {
        let result = self.start_inner();
        let paths = self.paths();
        // `start_inner` may fail before it creates the run directory.
        let _ = paths.ensure();
        let message = match &result {
            Ok(report) => format!(
                "started vm={} members={} harness={} vm_pid={}",
                report.vm,
                report.members,
                self.vm.harness.label(),
                report.vm_pid.unwrap_or(0)
            ),
            Err(err) => format!(
                "start failed vm={} members={}: {err:#}",
                self.vm.name,
                self.membership()
                    .map(|manifest| manifest.members.len())
                    .unwrap_or(0)
            ),
        };
        append_event_log(&paths.event_log, &message);
        result.map_err(|err| anyhow::anyhow!("{err:#} (VM log: {})", paths.event_log.display()))
    }

    fn start_inner(&self) -> Result<VmReport> {
        let paths = self.paths();
        // A missing VM is always a hard error: there is no host-process fallback.
        self.config.require_vm_assets()?;
        // The snapshot staging directory must exist before Cloud Hypervisor
        // starts: it is a Landlock rule, and Landlock rules are validated at
        // create time.
        self.config.ensure_dirs()?;
        SnapshotStore::new(&self.config.snapshots_dir).ensure_staging()?;

        // Membership is resolved from the private registry, not `vm.toml`.
        let registry = ProjectRegistry::load(&self.config.state_dir)?;
        let plan = BootPlan::build(&self.config, &self.vm, &self.store, &registry, &paths)?;
        let primary_path = plan
            .members
            .iter()
            .find(|member| member.name == plan.primary)
            .map(|member| member.path.clone())
            .context("the boot plan lost its primary project")?;

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
        let _hegn = hegn_binary()?;
        let _lock = self.lock(&paths)?;
        if self.unreadable_session() {
            bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 stop the old VM manually and remove that directory before starting",
                paths.run_dir.display()
            );
        }
        if self.is_running() {
            bail!(
                "VM '{}' is already running (use `hegn vm stop {}` first)",
                self.vm.name,
                self.vm.name
            );
        }
        // Save the primary the first time an empty VM gets a member. `/workspace`
        // is a boot-time symlink, so this is the only place it is chosen.
        if plan.primary_changed {
            let mut vm = self.vm.clone();
            vm.primary = Some(plan.primary.clone());
            self.store.save(&vm)?;
        }
        // Clear stale sockets/pid files and any orphaned autosave worker before
        // restoring or booting. A live VM returned above via `is_running`.
        self.cleanup_stale();
        if self.config.network == crate::config::NetworkMode::Off
            && self.config.cloud_init
            && self.vm.harness != Harness::Shell
        {
            bail!(
                "harness '{}' needs the guarded network for its first-boot install; enable \
                 `network = \"guarded\"` or use a pre-provisioned rootfs with `cloud_init = false`",
                self.vm.harness.label()
            );
        }

        ensure_rootfs_unique(&self.config, &self.store, &self.vm)?;
        let rootfs = prepare_rootfs(&self.config, &self.vm)?;
        ensure_no_running_export(&self.config, &self.store, &plan)?;
        // Materialize the harness profile before the auto-resume decision: its
        // presence is part of the boot fingerprint, and a cold boot would use
        // the same plan. A bad profile must fail before any process spawns.
        let profile_plan = profile::prepare_for_start(&self.config, &self.vm)?;
        for warning in &profile_plan.warnings {
            append_event_log(&paths.event_log, &format!("profile: {warning}"));
        }

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
        // Try a matching automatic checkpoint first: this is what turns a
        // restart into a resume instead of a full cold boot. A checkpoint whose
        // device tree no longer matches is skipped with a logged reason, never
        // deleted, and the VM cold boots.
        if let Some(candidates) = auto_resume_candidates(&self.config, &self.vm) {
            match candidates {
                Ok(candidates) if !candidates.is_empty() => {
                    let current = super::snapshot::current_boot(
                        &self.config,
                        &self.vm,
                        &plan,
                        profile_plan.share.is_some(),
                    );
                    let members = super::snapshot::current_membership(&plan);
                    match super::snapshot::select_auto_resume(candidates, &current, &members) {
                        super::snapshot::AutoResume::Resume(snapshot) => {
                            append_event_log(
                                &paths.event_log,
                                &format!("auto-resuming from snapshot {}", snapshot.name),
                            );
                            match runtime
                                .block_on(self.restore_snapshot_locked(&snapshot, &profile_plan))
                            {
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
                        super::snapshot::AutoResume::ColdBoot(reasons) => {
                            for reason in reasons {
                                append_event_log(
                                    &paths.event_log,
                                    &format!("auto-resume {reason}"),
                                );
                            }
                            append_event_log(
                                &paths.event_log,
                                "auto-resume found no matching checkpoint; cold booting",
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => append_event_log(
                    &paths.event_log,
                    &format!("warning: could not inspect automatic snapshots: {err:#}"),
                ),
            }
        }
        let mut packages = self.vm.extra_packages.clone();
        packages.extend(profile_plan.packages.iter().cloned());
        let provision = ProvisionPlan::new(self.vm.harness, &packages)?;
        // Reserve the network slot before any process is spawned. `acquire`
        // also verifies that the host firewall marker is present in guarded
        // mode, so an unconfigured guard fails before virtiofsd or the VMM can
        // start. The control TAP is reserved in every mode.
        let network_lease = network::acquire(&self.config, &self.vm)?;
        if let Err(err) = self.persist_network_index(network_lease.slot) {
            network::release(&self.config, &self.vm.name);
            return Err(err);
        }
        if let Err(err) = paths.ensure() {
            network::release(&self.config, &self.vm.name);
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
        let serial_log = match logs.create_session(&primary_path, started_at, std::process::id()) {
            Ok(path) => path,
            Err(err) => {
                network::release(&self.config, &self.vm.name);
                return Err(err);
            }
        };

        let mut rollback = BootRollback {
            manager: self,
            paths: &paths,
            members: Vec::new(),
            profile: None,
            vm: None,
            relay_pid: 0,
            armed: false,
        };

        // 1. One sandboxed virtiofsd per exported project, in plan order.
        for member in &plan.members {
            let mut process = match spawn_virtiofsd_at(
                &self.config,
                &member.path,
                &member.socket,
                member.read_only,
            ) {
                Ok(process) => process,
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to export project '{}'", member.name));
                }
            };
            if let Err(err) = write_private(&member.pid_file, process.pid().to_string()) {
                process.kill();
                return Err(err).context("failed to record the virtiofsd pid");
            }
            rollback.members.push(process);
        }

        // 1b. Optional second virtiofsd for the copied profile and skills.
        if let Some(share) = &profile_plan.share {
            let process = spawn_virtiofsd_at(
                &self.config,
                &share.bundle_dir,
                &paths.profile_socket,
                false,
            )?;
            if let Err(err) = write_private(&paths.profile_virtiofs_pid, process.pid().to_string())
            {
                let mut process = process;
                process.kill();
                return Err(err).context("failed to record the profile virtiofsd pid");
            }
            rollback.profile = Some(process);
        }
        let profile_virtiofs_pid = rollback
            .profile
            .as_ref()
            .map(VirtiofsProcess::pid)
            .unwrap_or(0);

        // 2. Optional NoCloud seed for the official cloud images. It contains
        // no secrets and only enables a serial login (root unless credential
        // hardening is enabled).
        if self.config.cloud_init {
            let primary_guest = plan
                .primary_guest_path()
                .context("the boot plan lost the primary guest path")?;
            let mounts = plan.mount_specs();
            let instance_id = cloud_init::instance_id(
                &self.vm.name,
                &mounts,
                primary_guest,
                &provision,
                profile_plan.share.as_ref(),
                network_lease.data_lease(),
                &self.config.local_endpoints,
                self.config.guest_credential_hardening,
            );
            cloud_init::write_seed(
                &paths.cloud_init,
                &mounts,
                primary_guest,
                &instance_id,
                &provision,
                profile_plan.share.as_ref(),
                network_lease.data_lease(),
                &self.config.local_endpoints,
                self.config.guest_credential_hardening,
            )?;
        }

        // 3. Cloud Hypervisor. Children outlive this CLI call.
        let vm = spawn_cloud_hypervisor(
            &self.config,
            &self.vm,
            &plan,
            &paths,
            &rootfs,
            &vm_binary,
            network_lease.data_lease(),
            profile_plan
                .share
                .as_ref()
                .map(|_| paths.profile_socket.as_path()),
        )?;
        let vm_pid = vm.id();
        rollback.vm = Some(vm);

        // 4. The relay is Cloud Hypervisor's only serial client. Start it
        // before waiting for the API so the earliest boot output is captured.
        let relay_pid = {
            let Some(vm) = rollback.vm.as_mut() else {
                bail!("internal error: the VMM child disappeared immediately after spawn");
            };
            start_relay(&paths, &serial_log, vm)?
        };
        rollback.relay_pid = relay_pid;

        {
            let Some(vm) = rollback.vm.as_mut() else {
                bail!("internal error: the VMM child disappeared immediately after spawn");
            };
            runtime
                .block_on(wait_for_vm(&paths.api_socket, vm, API_START_TIMEOUT))
                .context("failed to boot Cloud Hypervisor")?;
        }

        let membership = MembershipManifest::from_plan(&plan);
        let record = SessionRecord {
            project: primary_path.clone(),
            workspace: primary_path,
            vm: self.vm.name.clone(),
            harness: self.vm.harness,
            started_at,
            vm_pid,
            vm_binary: Some(vm_binary.clone()),
            relay_pid,
            virtiofs_pids: rollback.members.iter().map(VirtiofsProcess::pid).collect(),
            profile_virtiofs_pid,
            boot_args: boot_args(&self.config.root_device),
            serial_log: Some(serial_log.clone()),
            membership: membership.members.clone(),
        };
        let recorded = (|| -> Result<()> {
            membership.save(&paths.membership)?;
            write_private(&paths.vm_pid, vm_pid.to_string())?;
            write_private(&paths.relay_pid, relay_pid.to_string())?;
            write_private(&paths.session, serde_json::to_string_pretty(&record)?)?;
            Ok(())
        })();
        if let Err(err) = recorded {
            return Err(err).context("failed to record the VM session");
        }

        let report = VmReport {
            state: "running".into(),
            backend: "cloud-hypervisor".into(),
            vm: self.vm.name.clone(),
            primary: Some(plan.primary.clone()),
            members: plan.members.len(),
            project: Some(record.project.clone()),
            vcpus: self.vm.vcpus,
            memory_mib: self.vm.memory_mib,
            api_socket: paths.api_socket.clone(),
            serial_socket: paths.serial_socket.clone(),
            console_socket: paths.console_socket.clone(),
            virtiofs_running: true,
            virtiofs_pids: record.virtiofs_pids.clone(),
            profile_share: profile_plan
                .share
                .as_ref()
                .map(|share| share.bundle_dir.clone()),
            profile_virtiofs_pid: (profile_virtiofs_pid != 0).then_some(profile_virtiofs_pid),
            profile_virtiofs_running: profile_virtiofs_pid != 0,
            relay_running: true,
            vm_pid: Some(vm_pid),
            relay_pid: Some(relay_pid),
            started_at: Some(record.started_at),
            serial_log: Some(serial_log),
            local_endpoints: network_lease
                .data_lease()
                .map(|lease| endpoint_urls(&self.config, lease.host_ip))
                .unwrap_or_default(),
            note: Some(format!(
                "{} project(s) mounted below {}; /workspace points at {}",
                plan.members.len(),
                self.config.guest_projects_base,
                plan.primary_guest_path().unwrap_or("/workspace")
            )),
        };

        // Do not kill the children when the rollback guard drops.
        rollback.commit();
        // Keep rolling checkpoints so the next start is a resume. This is a
        // helper for recovery, never a reason to fail a successful boot.
        if let Err(err) = autosave::spawn(&self.config, &self.vm, &paths) {
            append_event_log(
                &paths.event_log,
                &format!("autosnapshot: could not start worker: {err:#}"),
            );
        }
        Ok(report)
    }

    async fn restore_snapshot_locked(
        &self,
        snapshot: &SnapshotMeta,
        profile_plan: &ProfilePlan,
    ) -> Result<()> {
        super::snapshot::restore_snapshot_locked(self, snapshot, profile_plan).await
    }
}

/// Rollback guard for a partial boot: kills every spawned child and cleans
/// runtime files unless the boot is committed.
struct BootRollback<'a> {
    manager: &'a VmManager,
    paths: &'a VmPaths,
    members: Vec<VirtiofsProcess>,
    profile: Option<VirtiofsProcess>,
    vm: Option<Child>,
    relay_pid: u32,
    armed: bool,
}

impl BootRollback<'_> {
    /// Keep the children alive; the caller owns the running VM now.
    fn commit(&mut self) {
        self.armed = true;
        for process in self.members.drain(..) {
            std::mem::forget(process);
        }
        if let Some(process) = self.profile.take() {
            std::mem::forget(process);
        }
        if let Some(child) = self.vm.take() {
            std::mem::forget(child);
        }
        self.relay_pid = 0;
    }
}

impl Drop for BootRollback<'_> {
    fn drop(&mut self) {
        if self.armed {
            return;
        }
        if let Some(mut child) = self.vm.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Ok(hegn) = hegn_binary()
            && self.relay_pid != 0
        {
            terminate_relay(self.relay_pid, &hegn);
        }
        for mut process in self.members.drain(..) {
            process.kill();
        }
        if let Some(mut process) = self.profile.take() {
            process.kill();
        }
        terminate_indexed_members(self.paths, &self.manager.config.virtiofsd_bin);
        cleanup_files(self.paths);
        network::release(&self.manager.config, &self.manager.vm.name);
    }
}

/// Reject two VMs that resolve to the same writable disk.
///
/// This compares effective paths, so an explicit `rootfs_image` that collides
/// with another VM's private disk is caught too.
fn ensure_rootfs_unique(config: &Config, store: &VmStore, vm: &VmConfig) -> Result<()> {
    let mine = effective_rootfs(config, vm);
    for other in store.list()? {
        if other.name == vm.name {
            continue;
        }
        if effective_rootfs(config, &other) == mine {
            bail!(
                "VM '{}' and VM '{}' share rootfs {}; every VM needs its own writable disk",
                vm.name,
                other.name,
                mine.display()
            );
        }
    }
    Ok(())
}

pub(super) fn effective_rootfs(config: &Config, vm: &VmConfig) -> PathBuf {
    let path = vm.rootfs_path(config);
    path.canonicalize().unwrap_or(path)
}

/// Automatic checkpoints eligible for this start.
///
/// `None` means the VM disabled auto-resume. The global `Config::auto_resume`
/// only seeds a new [`VmConfig`]; an existing VM's `vm.toml` is authoritative.
fn auto_resume_candidates(config: &Config, vm: &VmConfig) -> Option<Result<Vec<SnapshotMeta>>> {
    if !vm.auto_resume {
        return None;
    }
    Some(SnapshotStore::new(&config.snapshots_dir).autosaves_for(&vm.name))
}

/// Reject a canonical member path already exported by a running VM.
fn ensure_no_running_export(config: &Config, store: &VmStore, plan: &BootPlan) -> Result<()> {
    for other in store.list()? {
        if other.name == plan.vm {
            continue;
        }
        let manager = VmManager::new(config.clone(), other.clone())?;
        if !manager.is_running() {
            continue;
        }
        let Some(manifest) = manager.membership() else {
            bail!(
                "cannot verify exports of running VM '{}' because its membership manifest is \
                 missing or unreadable; stop that VM first (or restore the manifest)",
                other.name
            );
        };
        for member in &manifest.members {
            if plan
                .exported_paths()
                .iter()
                .any(|path| paths_equal(path, &member.path))
            {
                bail!(
                    "path {} is already exported by running VM '{}'; a host directory may only be \
                     exported by one running VM",
                    member.path.display(),
                    other.name
                );
            }
        }
    }
    Ok(())
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::fs;

    use super::*;
    use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
    use crate::vm::manager::test_support::{config_with_assets, manager, manager_for, plan_for};
    use chrono::Utc;

    fn entry(name: &str, path: &Path) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            path: path.canonicalize().unwrap(),
            vm: None,
            read_only: false,
            kind: ProjectKind::External,
            parent: None,
            managed: false,
            hidden: false,
            created_at: Utc::now(),
        }
    }

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
    fn auto_resume_is_decided_from_the_vm_not_the_global_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.auto_resume = true;
        let mut vm = VmConfig::from_config(&config, "demo", Some(Harness::Shell));

        vm.auto_resume = false;
        assert!(
            auto_resume_candidates(&config, &vm).is_none(),
            "vm.toml must win over the global default"
        );

        vm.auto_resume = true;
        let candidates = auto_resume_candidates(&config, &vm).expect("auto-resume enabled");
        assert!(candidates.unwrap().is_empty(), "fixture has no snapshots");
    }

    #[test]
    fn a_vm_seeded_from_the_global_config_inherits_auto_resume() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.auto_resume = false;
        let vm = VmConfig::from_config(&config, "demo", Some(Harness::Shell));

        assert!(!vm.auto_resume);
        assert!(auto_resume_candidates(&config, &vm).is_none());
    }

    #[test]
    fn start_preflights_binaries_before_spawning_anything() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());

        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("cloud-hypervisor binary"), "{err}");
        assert!(!manager.paths().virtiofs_pid(0).exists());
        assert!(!manager.paths().api_socket.exists());
    }

    #[test]
    fn start_requires_existing_member_directories() {
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
        let project = dir.path().join("project");
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
        let project = dir.path().join("project");
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
        use crate::config::Harness;
        use crate::vm::manager::SessionRecord;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        config.virtiofsd_bin = std::env::current_exe().unwrap();
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: manager.vm().dir(&manager.config().vms_dir),
            workspace: std::path::PathBuf::new(),
            vm: "demo".into(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid: std::process::id(),
            vm_binary: Some(std::env::current_exe().unwrap()),
            relay_pid: 0,
            virtiofs_pids: Vec::new(),
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
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
        assert!(!paths.virtiofs_pid(0).exists());
    }

    #[test]
    fn start_refuses_a_harness_install_without_the_guarded_network() {
        use crate::config::Harness;
        use crate::vm::config::VmConfig;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        config.virtiofsd_bin = std::env::current_exe().unwrap();
        config.network = crate::config::NetworkMode::Off;
        config.cloud_init = true;
        let manager = manager(config, dir.path());
        let mut vm: VmConfig = manager.vm().clone();
        vm.harness = Harness::Codex;
        let manager = VmManager::new(manager.config().clone(), vm).unwrap();

        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("needs the guarded network"), "{err}");
        assert!(!manager.paths().virtiofs_pid(0).exists());
        assert!(!manager.paths().api_socket.exists());
    }

    #[test]
    fn start_refuses_an_empty_vm() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let store = VmStore::new(&config);
        let vm = VmConfig::from_config(&config, "empty", None);
        store.save(&vm).unwrap();
        let manager = VmManager::new(config, vm).unwrap();
        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("has no projects"), "{err}");
        assert!(err.contains("hegn project assign"), "{err}");
    }

    #[test]
    fn a_running_vm_without_a_membership_manifest_blocks_a_new_export() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        let store = VmStore::new(&config);

        let one_project = dir.path().join("projects/shared");
        fs::create_dir_all(&one_project).unwrap();
        let other_project = dir.path().join("projects/other");
        fs::create_dir_all(&other_project).unwrap();

        let mut one = VmConfig::from_config(&config, "one", None);
        one.primary = Some("shared".into());
        store.save(&one).unwrap();
        let mut two = VmConfig::from_config(&config, "two", None);
        two.primary = Some("other".into());
        store.save(&two).unwrap();

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("shared", &one_project)).unwrap();
        registry.assign("shared", "one", false).unwrap();
        registry.upsert(entry("other", &other_project)).unwrap();
        registry.assign("other", "two", false).unwrap();

        // Fake a running VM without its membership manifest.
        let one_manager = VmManager::new(config.clone(), one).unwrap();
        let one_paths = one_manager.paths();
        one_paths.ensure().unwrap();
        let record = SessionRecord {
            project: one_project.canonicalize().unwrap(),
            workspace: one_project.canonicalize().unwrap(),
            vm: "one".into(),
            harness: crate::config::Harness::Shell,
            started_at: Utc::now(),
            vm_pid: std::process::id(),
            vm_binary: Some(std::env::current_exe().unwrap()),
            relay_pid: 0,
            virtiofs_pids: Vec::new(),
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
        };
        fs::write(&one_paths.session, serde_json::to_string(&record).unwrap()).unwrap();

        let two_manager = VmManager::new(config.clone(), two).unwrap();
        let plan = plan_for(&two_manager);
        let err = ensure_no_running_export(&config, &store, &plan)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cannot verify exports of running VM 'one'"),
            "{err}"
        );
        assert!(
            err.contains("membership manifest is missing or unreadable"),
            "{err}"
        );
    }

    #[test]
    fn explicit_rootfs_images_must_be_unique() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let store = VmStore::new(&config);
        let shared = dir.path().join("shared.qcow2");
        fs::write(&shared, b"disk").unwrap();
        let mut one = VmConfig::from_config(&config, "one", None);
        one.rootfs_image = Some(shared.clone());
        let mut two = VmConfig::from_config(&config, "two", None);
        two.rootfs_image = Some(shared.clone());
        store.save(&one).unwrap();
        store.save(&two).unwrap();

        let err = ensure_rootfs_unique(&config, &store, &two)
            .unwrap_err()
            .to_string();
        assert!(err.contains("share rootfs"), "{err}");
        assert!(err.contains("VM 'one'"), "{err}");
    }

    #[test]
    fn a_running_vm_cannot_export_the_same_path_twice() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        let store = VmStore::new(&config);

        let project = dir.path().join("projects/shared");
        fs::create_dir_all(&project).unwrap();
        let path = project.canonicalize().unwrap();

        let mut one = VmConfig::from_config(&config, "one", None);
        one.primary = Some("shared".into());
        store.save(&one).unwrap();
        let mut two = VmConfig::from_config(&config, "two", None);
        two.primary = Some("shared".into());
        store.save(&two).unwrap();

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("shared", &path)).unwrap();
        registry.assign("shared", "one", false).unwrap();

        // Fake a running "one" VM with a membership manifest.
        let one_manager = VmManager::new(config.clone(), one.clone()).unwrap();
        let one_paths = one_manager.paths();
        one_paths.ensure().unwrap();
        let record = SessionRecord {
            project: path.clone(),
            workspace: path.clone(),
            vm: "one".into(),
            harness: crate::config::Harness::Shell,
            started_at: Utc::now(),
            vm_pid: std::process::id(),
            vm_binary: Some(std::env::current_exe().unwrap()),
            relay_pid: 0,
            virtiofs_pids: Vec::new(),
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
        };
        fs::write(&one_paths.session, serde_json::to_string(&record).unwrap()).unwrap();
        MembershipManifest {
            version: 1,
            vm: "one".into(),
            primary: "shared".into(),
            members: vec![crate::vm::manager::MembershipMember {
                name: "shared".into(),
                path: path.clone(),
                tag: "hegn-prj-0".into(),
                guest_path: "/projects/shared".into(),
                read_only: false,
                kind: ProjectKind::External,
            }],
        }
        .save(&one_paths.membership)
        .unwrap();

        // The project is reassigned while the first VM is still running. The
        // registry now says "two", but "one" still has the path in its
        // membership manifest.
        registry.assign("shared", "two", false).unwrap();
        let two_manager = VmManager::new(config.clone(), two.clone()).unwrap();
        let plan = plan_for(&two_manager);
        let err = ensure_no_running_export(&config, &store, &plan)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already exported by running VM 'one'"),
            "{err}"
        );
    }
}
