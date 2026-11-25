//! Snapshot creation and restore.
//!
//! Snapshots are staged inside the VMM's Landlock view, tightened to private
//! modes, then moved to their durable location. A restore always boots a fresh
//! Cloud Hypervisor process, so a failed restore cannot take down a running VM.
//!
//! Every exported member gets a fresh virtiofsd on the same indexed socket
//! before the VMM is restored. The snapshot itself owns the device tree; a
//! restore never passes `--fs` again.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::agent::profile::{self, PROFILE_SHARE_TAG};
use crate::agent::provision::ProvisionPlan;
use crate::config::{Harness, VIRTIOFSD_FALLBACKS};
use crate::network;
use crate::project::ProjectRegistry;
use crate::session::logs::{DEFAULT_KEEP, LogStore};
use crate::session::snapshot::{SnapshotMeta, SnapshotStore};
use crate::vm::cloud_init;
use crate::vm::hypervisor::{CloudHypervisorClient, SnapshotConfig};
use crate::vm::virtiofs::VirtiofsProcess;

use super::BootPlan;
use super::VmManager;
use super::args::{API_START_TIMEOUT, spawn_restore, spawn_virtiofsd_at, start_relay, wait_for_vm};
use super::cleanup::{cleanup_files, terminate_indexed_members};
use super::membership::MembershipManifest;
use super::paths::VmPaths;
use super::process::{
    ensure_socket, hegn_binary, read_pid, terminate, terminate_autosave, terminate_relay,
};
use super::session::SessionRecord;
use super::util::{append_event_log, dir_size, secure_snapshot_dir, write_private};

/// TAP names embedded in a Cloud Hypervisor snapshot config.
fn snapshot_network_taps(dir: &Path) -> Result<Vec<String>> {
    let path = dir.join("config.json");
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(value
        .get("net")
        .and_then(serde_json::Value::as_array)
        .map(|devices| {
            devices
                .iter()
                .filter_map(|device| device.get("tap").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// virtio-fs tags embedded in a Cloud Hypervisor snapshot config.
fn snapshot_fs_tags(dir: &Path) -> Result<Vec<String>> {
    let path = dir.join("config.json");
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(value
        .get("fs")
        .and_then(serde_json::Value::as_array)
        .map(|devices| {
            devices
                .iter()
                .filter_map(|device| device.get("tag").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

impl VmManager {
    /// Pause, snapshot RAM+CPU into `snapshots_dir`, resume.
    pub async fn create_snapshot(&self, name: &str) -> Result<SnapshotMeta> {
        self.create_snapshot_tagged(name, None).await
    }

    /// Like [`Self::create_snapshot`], but records a note in the metadata.
    ///
    /// Automatic checkpoints use [`crate::session::snapshot::AUTOSAVE_NOTE`]
    /// so they can be pruned and auto-resumed without ever touching a manual
    /// checkpoint.
    pub async fn create_snapshot_tagged(
        &self,
        name: &str,
        note: Option<&str>,
    ) -> Result<SnapshotMeta> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        // Serialize with start/stop/restore so the VM cannot be torn down
        // between pause and resume.
        let _lock = self.lock(&paths)?;
        let name = crate::session::snapshot::sanitize_name(name)?;
        let store = SnapshotStore::new(&self.config.snapshots_dir);
        let prepared = store.prepare_staged(&name)?;
        let client = CloudHypervisorClient::new(&paths.api_socket);

        if let Err(err) = client.pause().await {
            let _ = fs::remove_dir_all(&prepared.dir);
            return Err(err);
        }
        if let Err(err) = client
            .snapshot(&SnapshotConfig {
                destination_url: format!("file://{}", prepared.dir.display()),
            })
            .await
        {
            // Always try to resume, even if snapshot creation failed.
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&prepared.dir);
            return Err(err);
        }

        if let Err(err) = secure_snapshot_dir(&prepared.dir) {
            // Snapshot memory can contain credentials; do not keep a snapshot
            // whose permissions we could not tighten.
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&prepared.dir);
            return Err(err);
        }
        // Move the finished snapshot out of the VMM's Landlock view before it
        // becomes a durable checkpoint.
        if let Err(err) = store.finalize(&prepared) {
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&prepared.dir);
            return Err(err);
        }
        let size = dir_size(&prepared.final_dir);
        let membership = self.membership();
        let project = membership.as_ref().and_then(|manifest| {
            manifest
                .members
                .iter()
                .find(|member| member.name == manifest.primary)
                .map(|member| member.path.clone())
        });
        let meta = SnapshotMeta {
            name: name.to_string(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: prepared.final_dir.clone(),
            instance: Some(self.vm.name.clone()),
            project,
            size_bytes: size,
            note: note.map(str::to_string),
        };
        if let Err(err) = store.save_meta(&meta) {
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&prepared.final_dir);
            return Err(err);
        }
        client
            .resume()
            .await
            .context("snapshot saved, but the VM could not be resumed")?;
        Ok(meta)
    }

    /// Restore a snapshot into a brand new Cloud Hypervisor process.
    pub async fn restore_snapshot(&self, snapshot: &SnapshotMeta) -> Result<()> {
        let paths = self.paths();
        paths.ensure()?;
        // Serialize with start/stop so another hegn process cannot recreate the
        // sockets while the snapshot boot is in flight.
        let _lock = self.lock(&paths)?;
        let registry = ProjectRegistry::load(&self.config.state_dir)?;
        let plan = BootPlan::build(&self.config, &self.vm, &self.store, &registry, &paths)?;
        let rootfs = self.vm.rootfs_path(&self.config);
        restore_snapshot_locked(self, snapshot, &plan, &rootfs).await
    }
}

/// Restore while the caller already holds the lifecycle lock.
pub(super) async fn restore_snapshot_locked(
    manager: &VmManager,
    snapshot: &SnapshotMeta,
    plan: &BootPlan,
    rootfs: &Path,
) -> Result<()> {
    let config = manager.config();
    let vm = manager.vm();
    let paths = manager.paths();
    // A snapshot embeds the disk paths of the VM that created it. Restoring it
    // into another VM would make two VMs share one writable rootfs.
    if let Some(owner) = &snapshot.instance
        && owner != &vm.name
    {
        bail!(
            "snapshot '{}' belongs to VM '{}'; restore it into that VM",
            snapshot.name,
            owner
        );
    }
    if let Some(project) = &snapshot.project
        && !plan.exported_paths().contains(&project.as_path())
    {
        bail!(
            "snapshot '{}' was taken for {}; no exported member of VM '{}' matches it",
            snapshot.name,
            project.display(),
            vm.name
        );
    }
    paths.ensure()?;
    config.ensure_dirs()?;
    SnapshotStore::new(&config.snapshots_dir).ensure_staging()?;
    if !snapshot.dir.is_dir() {
        bail!("snapshot directory {} is missing", snapshot.dir.display());
    }
    for required in ["config.json", "state.json", "memory-ranges"] {
        if !snapshot.dir.join(required).is_file() {
            bail!(
                "snapshot {} is not restorable: missing {}",
                snapshot.name,
                snapshot.dir.join(required).display()
            );
        }
    }
    let existing = manager.session();
    // Preflight the binaries before stopping a running VM, so a typo in the
    // configuration does not cost the user their current VM.
    let vm_binary = config.resolved_cloud_hypervisor().with_context(|| {
        format!(
            "cloud-hypervisor binary {} was not found; hegn also accepts `{}`",
            config.cloud_hypervisor_bin.display(),
            crate::config::CLOUD_HYPERVISOR_STATIC
        )
    })?;
    if config.resolved_virtiofsd().is_none() {
        bail!(
            "virtiofsd binary {} was not found on PATH or in {}; install virtiofsd or set \
             `virtiofsd_bin`",
            config.virtiofsd_bin.display(),
            VIRTIOFSD_FALLBACKS.join(", ")
        );
    }
    let hegn = hegn_binary()?;
    if manager.unreadable_session() {
        bail!(
            "session metadata in {} is unreadable or from an older hegn version; \
             stop the old VM manually and remove that directory before restoring",
            paths.run_dir.display()
        );
    }
    // The snapshot embeds its device tree. Prepare the profile before stopping
    // the current VM so a bad profile cannot cost the user their running
    // session.
    let profile_plan = profile::prepare_for_start(config, vm)?;
    let snapshot_fs = snapshot_fs_tags(&snapshot.dir)?;
    let needs_profile = snapshot_fs.iter().any(|tag| tag == PROFILE_SHARE_TAG);
    if needs_profile && profile_plan.share.is_none() {
        bail!(
            "snapshot '{}' contains the profile share, but the current profile has no files, \
             skills or environment to export",
            snapshot.name
        );
    }
    for warning in &profile_plan.warnings {
        append_event_log(&paths.event_log, &format!("profile: {warning}"));
    }
    let mut packages = vm.extra_packages.clone();
    packages.extend(profile_plan.packages.iter().cloned());
    let provision = ProvisionPlan::new(vm.harness, &packages)?;
    if config.network == crate::config::NetworkMode::Off
        && config.cloud_init
        && vm.harness != Harness::Shell
    {
        bail!(
            "harness '{}' needs the guarded network for its first-boot install; enable \
             `network = \"guarded\"` or use a pre-provisioned rootfs with `cloud_init = false`",
            vm.harness.label()
        );
    }
    let network_lease = match config.network {
        crate::config::NetworkMode::Guarded => Some(network::acquire(config, vm)?),
        crate::config::NetworkMode::Off => None,
    };
    if let Some(lease) = &network_lease
        && let Err(err) = manager.persist_network_index(lease.index)
    {
        network::release(config, &vm.name);
        return Err(err);
    }
    // The snapshot embeds its device tree. Never silently restore a NIC that
    // contradicts the current policy, and never attach a snapshot to a
    // different VM TAP than the one it was created with.
    let snapshot_taps = match snapshot_network_taps(&snapshot.dir) {
        Ok(taps) => taps,
        Err(err) => {
            network::release(config, &vm.name);
            return Err(err);
        }
    };
    match (&network_lease, snapshot_taps.as_slice()) {
        (None, []) => {}
        (None, [_, ..]) => {
            bail!(
                "snapshot '{}' contains a network device but `network = \"off\"`; enable \
                 guarded networking or restore an air-gapped snapshot",
                snapshot.name
            );
        }
        (Some(lease), []) => append_event_log(
            &paths.event_log,
            &format!(
                "warning: snapshot {} has no network device; restoring it without the reserved {}",
                snapshot.name, lease.tap
            ),
        ),
        (Some(lease), taps) if taps.iter().any(|tap| tap != &lease.tap) => {
            // Leave the VM's reservation intact: an old VM may still be running
            // on that TAP, and the next start will reuse it.
            bail!(
                "snapshot '{}' expects TAP {}, but VM '{}' owns {}; restore it in the VM that \
                 created it",
                snapshot.name,
                taps.join(", "),
                vm.name,
                lease.tap
            );
        }
        (Some(_), [_, ..]) => {}
    }

    // Stop the old VM, relay and every member filesystem daemon. The sockets
    // are freed and recreated below with the same paths the snapshot expects.
    let existing_vm_binary = manager.trusted_vm_binary(
        existing
            .as_ref()
            .and_then(|session| session.vm_binary.as_ref()),
    );
    if let Some(session) = &existing {
        terminate(session.vm_pid, &existing_vm_binary);
        terminate_relay(session.relay_pid, &hegn);
        for pid in &session.virtiofs_pids {
            terminate(*pid, &config.virtiofsd_bin);
        }
        if session.profile_virtiofs_pid != 0 {
            terminate(session.profile_virtiofs_pid, &config.virtiofsd_bin);
        }
    }
    if let Some(pid) = read_pid(&paths.vm_pid) {
        terminate(pid, &existing_vm_binary);
    }
    if let Some(pid) = read_pid(&paths.relay_pid) {
        terminate_relay(pid, &hegn);
    }
    terminate_indexed_members(&paths, &config.virtiofsd_bin);
    if let Some(pid) = read_pid(&paths.profile_virtiofs_pid) {
        terminate(pid, &config.virtiofsd_bin);
    }
    if let Some(pid) = read_pid(&paths.autosave_pid) {
        terminate_autosave(pid, &hegn);
    }
    cleanup_files(&paths);

    let mut rollback = RestoreRollback {
        manager,
        paths: &paths,
        members: Vec::new(),
        profile: None,
        vm: None,
        relay_pid: 0,
        armed: false,
    };

    // One sandboxed daemon per recorded member, on the same indexed sockets.
    for member in &plan.members {
        let process = spawn_virtiofsd_at(config, &member.path, &member.socket, member.read_only)?;
        if let Err(err) = write_private(&member.pid_file, process.pid().to_string()) {
            let mut process = process;
            process.kill();
            return Err(err).context("failed to record the virtiofsd pid");
        }
        rollback.members.push(process);
    }
    let mut profile_virtiofs = None;
    if needs_profile && let Some(share) = &profile_plan.share {
        let process = spawn_virtiofsd_at(config, &share.bundle_dir, &paths.profile_socket, false)?;
        if let Err(err) = write_private(&paths.profile_virtiofs_pid, process.pid().to_string()) {
            let mut process = process;
            process.kill();
            return Err(err).context("failed to record the profile virtiofsd pid");
        }
        profile_virtiofs = Some(process);
    }
    rollback.profile = profile_virtiofs;
    let profile_virtiofs_pid = rollback
        .profile
        .as_ref()
        .map(VirtiofsProcess::pid)
        .unwrap_or(0);

    if config.cloud_init {
        let primary_guest = plan
            .primary_guest_path()
            .context("the boot plan lost the primary guest path")?;
        let mounts = plan.mount_specs();
        let instance_id = cloud_init::instance_id(
            &vm.name,
            &mounts,
            primary_guest,
            &provision,
            profile_plan.share.as_ref(),
            network_lease.as_ref(),
            &config.local_endpoints,
            config.guest_credential_hardening,
        );
        cloud_init::write_seed(
            &paths.cloud_init,
            &mounts,
            primary_guest,
            &instance_id,
            &provision,
            profile_plan.share.as_ref(),
            network_lease.as_ref(),
            &config.local_endpoints,
            config.guest_credential_hardening,
        )?;
    }

    let started_at = Utc::now();
    let logs = LogStore::new(config.logs_dir());
    if let Err(err) = logs.prune(DEFAULT_KEEP) {
        append_event_log(
            &paths.event_log,
            &format!("warning: could not prune old serial logs: {err:#}"),
        );
    }
    let primary_path = plan
        .members
        .iter()
        .find(|member| member.name == plan.primary)
        .map(|member| member.path.clone())
        .context("the boot plan lost its primary project")?;
    let serial_log = logs.create_session(&primary_path, started_at, std::process::id())?;

    let vm_child = spawn_restore(config, &paths, snapshot, &vm_binary, network_lease.as_ref())?;
    let vm_pid = vm_child.id();
    rollback.vm = Some(vm_child);
    let relay_pid = {
        let child = rollback.vm.as_mut().expect("the VMM was just spawned");
        start_relay(&paths, &serial_log, child)?
    };
    rollback.relay_pid = relay_pid;
    {
        let child = rollback.vm.as_mut().expect("the VMM was just spawned");
        wait_for_vm(&paths.api_socket, child, API_START_TIMEOUT)
            .await
            .context("failed to restore the Cloud Hypervisor snapshot")?;
    }

    let membership = MembershipManifest::from_plan(plan);
    let record = SessionRecord {
        project: primary_path.clone(),
        workspace: primary_path,
        vm: vm.name.clone(),
        harness: existing
            .as_ref()
            .map(|session| session.harness)
            .unwrap_or(vm.harness),
        started_at,
        vm_pid,
        vm_binary: Some(vm_binary.clone()),
        relay_pid,
        virtiofs_pids: rollback.members.iter().map(VirtiofsProcess::pid).collect(),
        profile_virtiofs_pid,
        boot_args: super::args::boot_args(&config.root_device),
        serial_log: Some(serial_log),
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
        return Err(err).context("failed to record the restored VM session");
    }

    rollback.commit();
    // Checkpoints keep flowing after a resume, so the next restart can use
    // the newest one. A worker failure must never fail the restore itself.
    if let Err(err) = super::autosave::spawn(config, vm, &paths) {
        append_event_log(
            &paths.event_log,
            &format!("autosnapshot: could not start worker: {err:#}"),
        );
    }
    let _ = rootfs;
    Ok(())
}

struct RestoreRollback<'a> {
    manager: &'a VmManager,
    paths: &'a VmPaths,
    members: Vec<VirtiofsProcess>,
    profile: Option<VirtiofsProcess>,
    vm: Option<std::process::Child>,
    relay_pid: u32,
    armed: bool,
}

impl RestoreRollback<'_> {
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

impl Drop for RestoreRollback<'_> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::manager::test_support::{config_with_assets, manager, manager_for};

    fn snapshot(dir: &Path, owner: &str) -> SnapshotMeta {
        SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.to_path_buf(),
            instance: Some(owner.into()),
            project: None,
            size_bytes: 0,
            note: None,
        }
    }

    #[test]
    fn restore_rejects_missing_snapshot_files() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snapshot = snapshot(&dir.path().join("missing"), "demo");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn restore_requires_an_existing_member() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("project");
        let manager = manager_for(config_with_assets(dir.path()), &workspace);
        std::fs::remove_dir_all(&workspace).unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        for name in ["config.json", "state.json", "memory-ranges"] {
            std::fs::write(snap_dir.join(name), b"x").unwrap();
        }
        let snapshot = snapshot(&snap_dir, "demo");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn restore_refuses_a_snapshot_from_another_vm() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snapshot = snapshot(&dir.path().join("snap"), "someone-else");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("belongs to VM 'someone-else'"), "{err}");
    }

    #[test]
    fn restore_refuses_an_unreadable_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        config.virtiofsd_bin = std::env::current_exe().unwrap();
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        for name in ["config.json", "state.json", "memory-ranges"] {
            std::fs::write(snap_dir.join(name), b"x").unwrap();
        }
        let snapshot = snapshot(&snap_dir, "demo");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert!(
            paths.session.exists(),
            "unreadable metadata must be preserved"
        );
    }

    #[test]
    fn restore_preflights_binaries_before_stopping_a_vm() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin =
            std::path::PathBuf::from("/definitely/missing/cloud-hypervisor");
        config.virtiofsd_bin = std::path::PathBuf::from("/definitely/missing/virtiofsd");
        let manager = manager(config, dir.path());
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        for name in ["config.json", "state.json", "memory-ranges"] {
            std::fs::write(snap_dir.join(name), b"x").unwrap();
        }
        let snapshot = snapshot(&snap_dir, "demo");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cloud-hypervisor binary"), "{err}");
    }

    #[test]
    fn snapshot_tap_names_are_read_from_the_device_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"net":[{"tap":"hegn3","mac":"52:54:00:00:00:04"}]}"#,
        )
        .unwrap();
        assert_eq!(snapshot_network_taps(dir.path()).unwrap(), vec!["hegn3"]);
        std::fs::write(dir.path().join("config.json"), r#"{"net":[]}"#).unwrap();
        assert!(snapshot_network_taps(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn snapshot_fs_tags_are_read_for_profile_restores() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"fs":[{"tag":"hegn-prj-0"},{"tag":"hegn-profile"}]}"#,
        )
        .unwrap();
        assert_eq!(
            snapshot_fs_tags(dir.path()).unwrap(),
            vec!["hegn-prj-0", "hegn-profile"]
        );
        std::fs::write(dir.path().join("config.json"), r#"{"fs":[]}"#).unwrap();
        assert!(snapshot_fs_tags(dir.path()).unwrap().is_empty());
    }
}
