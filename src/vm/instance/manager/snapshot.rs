//! Snapshot creation and restore.
//!
//! Snapshots are staged inside the VMM's Landlock view, tightened to private
//! modes, then moved to their durable location. A restore always boots a fresh
//! Cloud Hypervisor process, so a failed restore cannot take down a running VM.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::agent::profile::{self, PROFILE_SHARE_TAG};
use crate::agent::provision::ProvisionPlan;
use crate::config::{Harness, VIRTIOFSD_FALLBACKS};
use crate::network;
use crate::session::logs::{DEFAULT_KEEP, LogStore};
use crate::session::snapshot::{SnapshotMeta, SnapshotStore};
use crate::vm::cloud_init;
use crate::vm::hypervisor::{CloudHypervisorClient, SnapshotConfig};
use crate::vm::virtiofs::VirtiofsProcess;

use super::super::args::{
    API_START_TIMEOUT, boot_args, spawn_cloud_hypervisor, spawn_virtiofsd, spawn_virtiofsd_at,
    start_relay, wait_for_vm,
};
use super::super::process::{ensure_socket, hegn_binary, read_pid, terminate, terminate_relay};
use super::super::registry::same_project;
use super::super::session::SessionRecord;
use super::super::util::{append_event_log, dir_size, secure_snapshot_dir, write_private};
use super::InstanceManager;
use super::cleanup::cleanup_files;

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

impl InstanceManager {
    /// Pause, snapshot RAM+CPU into `snapshots_dir`, resume.
    pub async fn create_snapshot(&self, name: &str) -> Result<SnapshotMeta> {
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
        let meta = SnapshotMeta {
            name: name.to_string(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: prepared.final_dir.clone(),
            instance: Some(self.instance.name.clone()),
            project: Some(self.instance.project.clone()),
            size_bytes: size,
            note: None,
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
    ///
    /// The running VMM and relay are replaced first. A fresh process keeps a
    /// failed restore (Cloud Hypervisor exits on some restore errors) from
    /// taking down an existing VM. virtiofsd is restarted for the snapshot's
    /// project so the restored guest finds a healthy backend on the same
    /// socket path.
    pub async fn restore_snapshot(&self, snapshot: &SnapshotMeta) -> Result<()> {
        // The snapshot config embeds the disk paths from the instance that
        // created it. Restoring it elsewhere would make two instances share one
        // writable rootfs, so only the owning instance may load it.
        if let Some(owner) = &snapshot.instance
            && owner != &self.instance.name
        {
            bail!(
                "snapshot '{}' belongs to instance '{}'; restore it into that instance",
                snapshot.name,
                owner
            );
        }
        // The name alone is not enough: a deleted instance can be recreated for
        // a different project, and its old snapshot must not be loaded there.
        if let Some(project) = &snapshot.project
            && !same_project(project, &self.instance.project)
        {
            bail!(
                "snapshot '{}' belongs to {}; restore it into an instance for that project",
                snapshot.name,
                project.display()
            );
        }
        let paths = self.paths();
        paths.ensure()?;
        self.config.ensure_dirs()?;
        SnapshotStore::new(&self.config.snapshots_dir).ensure_staging()?;
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
        let existing = self.session();
        let project = self.instance.workspace.clone();
        if !project.is_dir() {
            bail!("instance workspace {} no longer exists", project.display());
        }
        // Preflight the binaries before stopping a running VM, so a typo in
        // the configuration does not cost the user their current instance.
        let vm_binary = self.config.resolved_cloud_hypervisor().with_context(|| {
            format!(
                "cloud-hypervisor binary {} was not found; hegn also accepts `{}`",
                self.config.cloud_hypervisor_bin.display(),
                crate::config::CLOUD_HYPERVISOR_STATIC
            )
        })?;
        if self.config.resolved_virtiofsd().is_none() {
            bail!(
                "virtiofsd binary {} was not found on PATH or in {}; install virtiofsd or set \
                 `virtiofsd_bin`",
                self.config.virtiofsd_bin.display(),
                VIRTIOFSD_FALLBACKS.join(", ")
            );
        }
        let hegn = hegn_binary()?;
        // Serialize with start/stop so another hegn process cannot recreate the
        // sockets while the snapshot boot is in flight.
        let _lock = self.lock(&paths)?;
        if self.unreadable_session() {
            bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 stop the old VM manually and remove that directory before restoring",
                paths.run_dir.display()
            );
        }
        // The snapshot embeds its device tree. Prepare the profile before
        // stopping the current VM so a bad profile cannot cost the user their
        // running session.
        let profile_plan = profile::prepare_for_start(&self.config, &self.instance)?;
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
        let mut packages = self.instance.extra_packages.clone();
        packages.extend(profile_plan.packages.iter().cloned());
        let provision = ProvisionPlan::new(self.instance.harness, &packages)?;
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
        // The snapshot embeds its device tree. Never silently restore a NIC
        // that contradicts the current policy, and never attach a snapshot to
        // a different instance TAP than the one it was created with.
        let snapshot_taps = match snapshot_network_taps(&snapshot.dir) {
            Ok(taps) => taps,
            Err(err) => {
                network::release(&self.config, &self.instance.name);
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
                // Leave the instance's reservation intact: an old VM may still be
                // running on that TAP, and the next start will reuse it.
                bail!(
                    "snapshot '{}' expects TAP {}, but instance '{}' owns {}; restore it in the \
                     instance that created it",
                    snapshot.name,
                    taps.join(", "),
                    self.instance.name,
                    lease.tap
                );
            }
            (Some(_), [_, ..]) => {}
        }

        // Stop the old VM, relay and filesystem daemon. The sockets are freed
        // and recreated below with the same paths the snapshot expects.
        let existing_vm_binary = existing
            .as_ref()
            .and_then(|session| session.vm_binary.clone())
            .unwrap_or_else(|| self.config.vm_binary());
        if let Some(session) = &existing {
            terminate(session.vm_pid, &existing_vm_binary);
            terminate_relay(session.relay_pid, &hegn);
            terminate(session.virtiofs_pid, &self.config.virtiofsd_bin);
            if session.profile_virtiofs_pid != 0 {
                terminate(session.profile_virtiofs_pid, &self.config.virtiofsd_bin);
            }
        }
        if let Some(pid) = read_pid(&paths.vm_pid) {
            terminate(pid, &existing_vm_binary);
        }
        if let Some(pid) = read_pid(&paths.relay_pid) {
            terminate_relay(pid, &hegn);
        }
        if let Some(pid) = read_pid(&paths.virtiofs_pid) {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        if let Some(pid) = read_pid(&paths.profile_virtiofs_pid) {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        cleanup_files(&paths);

        let mut virtiofs = match spawn_virtiofsd(&self.config, &paths, &project) {
            Ok(process) => process,
            Err(err) => {
                cleanup_files(&paths);
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

        let mut profile_virtiofs = None;
        if needs_profile && let Some(share) = &profile_plan.share {
            match spawn_virtiofsd_at(&self.config, &share.bundle_dir, &paths.profile_socket) {
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
        let profile_share = if needs_profile {
            profile_plan.share.as_ref()
        } else {
            None
        };

        if self.config.cloud_init
            && let Err(err) = cloud_init::write_seed(
                &paths.cloud_init,
                &self.instance.shared_tag,
                &self.instance.guest_mount,
                &cloud_init::instance_id(
                    &self.instance.name,
                    &provision,
                    profile_share,
                    network_lease.as_ref(),
                    &self.config.local_endpoints,
                ),
                &provision,
                profile_share,
                network_lease.as_ref(),
                &self.config.local_endpoints,
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

        let started_at = Utc::now();
        let logs = LogStore::new(self.config.logs_dir());
        if let Err(err) = logs.prune(DEFAULT_KEEP) {
            append_event_log(
                &paths.event_log,
                &format!("warning: could not prune old serial logs: {err:#}"),
            );
        }
        let serial_log = match logs.create_session(&project, started_at, std::process::id()) {
            Ok(path) => path,
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

        let mut vm = match spawn_cloud_hypervisor(
            &self.config,
            &self.instance,
            &paths,
            &vm_binary,
            Some(snapshot),
            None,
            profile_share.map(|_| paths.profile_socket.as_path()),
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

        if let Err(err) = wait_for_vm(&paths.api_socket, &mut vm, API_START_TIMEOUT).await {
            let _ = vm.kill();
            let _ = vm.wait();
            terminate_relay(relay_pid, &hegn);
            virtiofs.kill();
            if let Some(mut process) = profile_virtiofs.take() {
                process.kill();
            }
            cleanup_files(&paths);
            network::release(&self.config, &self.instance.name);
            return Err(err).context("failed to restore the Cloud Hypervisor snapshot");
        }

        let record = SessionRecord {
            project: self.instance.project.clone(),
            workspace: self.instance.workspace.clone(),
            instance: self.instance.name.clone(),
            harness: existing
                .as_ref()
                .map(|session| session.harness)
                .unwrap_or(self.instance.harness),
            started_at,
            vm_pid,
            vm_binary: Some(vm_binary.clone()),
            relay_pid,
            virtiofs_pid,
            profile_virtiofs_pid,
            boot_args: boot_args(&self.config.root_device),
            serial_log: Some(serial_log),
        };
        let recorded = (|| -> Result<()> {
            write_private(&paths.vm_pid, vm_pid.to_string())?;
            write_private(&paths.relay_pid, relay_pid.to_string())?;
            write_private(&paths.session, serde_json::to_string_pretty(&record)?)?;
            Ok(())
        })();
        if let Err(err) = recorded {
            let _ = vm.kill();
            let _ = vm.wait();
            terminate_relay(relay_pid, &hegn);
            virtiofs.kill();
            if let Some(mut process) = profile_virtiofs.take() {
                process.kill();
            }
            cleanup_files(&paths);
            network::release(&self.config, &self.instance.name);
            return Err(err).context("failed to record the restored instance session");
        }

        // The restored VM runs for the rest of the hegn session.
        std::mem::forget(vm);
        std::mem::forget(virtiofs);
        if let Some(process) = profile_virtiofs.take() {
            std::mem::forget(process);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::instance::test_support::{config_with_assets, manager, manager_for};

    #[test]
    fn restore_rejects_missing_snapshot_files() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snapshot = SnapshotMeta {
            name: "missing".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.path().join("missing"),
            instance: Some("demo".into()),
            project: Some(dir.path().join("project")),
            size_bytes: 0,
            note: None,
        };
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
    fn restore_requires_an_existing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("project");
        let manager = manager_for(config_with_assets(dir.path()), &workspace);
        std::fs::remove_dir_all(&workspace).unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        for name in ["config.json", "state.json", "memory-ranges"] {
            std::fs::write(snap_dir.join(name), b"x").unwrap();
        }
        let snapshot = SnapshotMeta {
            name: "orphan".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: snap_dir,
            instance: Some("demo".into()),
            project: None,
            size_bytes: 0,
            note: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("workspace") && err.contains("no longer exists"),
            "{err}"
        );
    }

    #[test]
    fn restore_refuses_a_snapshot_from_another_instance() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snapshot = SnapshotMeta {
            name: "shared".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.path().join("snap"),
            instance: Some("someone-else".into()),
            project: Some(dir.path().join("project")),
            size_bytes: 0,
            note: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("belongs to instance 'someone-else'"), "{err}");
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
        let snapshot = SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: snap_dir,
            instance: Some("demo".into()),
            project: Some(dir.path().join("project")),
            size_bytes: 0,
            note: None,
        };
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
        let snapshot = SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: snap_dir,
            instance: Some("demo".into()),
            project: Some(dir.path().join("project")),
            size_bytes: 0,
            note: None,
        };
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
            r#"{"fs":[{"tag":"workspace"},{"tag":"hegn-profile"}]}"#,
        )
        .unwrap();
        assert_eq!(
            snapshot_fs_tags(dir.path()).unwrap(),
            vec!["workspace", "hegn-profile"]
        );
        std::fs::write(dir.path().join("config.json"), r#"{"fs":[]}"#).unwrap();
        assert!(snapshot_fs_tags(dir.path()).unwrap().is_empty());
    }
}
