//! Snapshot creation and restore.
//!
//! Snapshots are staged inside the VMM's Landlock view, tightened to private
//! modes, then moved to their durable location. A restore always boots a fresh
//! Cloud Hypervisor process, so a failed restore cannot take down a running VM.
//!
//! Every snapshot belongs to one VM and records the exact exported membership
//! plus the boot inputs that shape the VMM device tree. Both are hashed into
//! `boot_fingerprint`:
//!
//! - **auto-resume** only restores a checkpoint whose fingerprint matches the
//!   current plan; a mismatch is logged and the VM cold boots,
//! - **manual restore** refuses a mismatch and names every changed field.
//!
//! A restore never consults the project registry for the device set. It
//! re-spawns one sandboxed virtiofsd per *recorded* member on the recorded
//! indexed sockets, regenerates the cloud-init seed from the recorded
//! membership and rewrites the session/membership records from it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::agent::profile::{self, PROFILE_SHARE_TAG, ProfilePlan};
use crate::agent::provision::ProvisionPlan;
use crate::config::{Config, Harness, VIRTIOFSD_FALLBACKS};
use crate::network;
use crate::project::{ProjectKind, ProjectRegistry, validate_recorded_export};
use crate::session::logs::{DEFAULT_KEEP, LogStore};
use crate::session::snapshot::{
    SNAPSHOT_VERSION, SnapshotBoot, SnapshotMember, SnapshotMeta, SnapshotStore,
    compute_fingerprint, describe_boot_changes,
};
use crate::vm::cloud_init;
use crate::vm::config::VmConfig;
use crate::vm::hypervisor::{CloudHypervisorClient, SnapshotConfig};
use crate::vm::virtiofs::VirtiofsProcess;

use super::MAX_TAG_BYTES;
use super::VmManager;
use super::args::{API_START_TIMEOUT, spawn_restore, spawn_virtiofsd_at, start_relay, wait_for_vm};
use super::cleanup::{cleanup_files, terminate_indexed_members};
use super::membership::{MEMBERSHIP_VERSION, MembershipManifest, MembershipMember};
use super::paths::VmPaths;
use super::plan::{BootPlan, SharedMember};
use super::process::{
    ensure_socket, hegn_binary, read_pid, terminate, terminate_autosave, terminate_relay,
};
use super::session::SessionRecord;
use super::start::{effective_rootfs, prepare_guest_ssh};
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

/// Check that a snapshot's recorded NICs match the current network lease.
///
/// A snapshot owns its device tree, so the expected set is exact: the control
/// TAP always, plus the data TAP only in guarded mode. Extra, missing or
/// foreign TAPs would either attach a NIC the guest never configured or boot a
/// guest with no control interface at all.
fn validate_snapshot_taps(
    snapshot_name: &str,
    vm_name: &str,
    lease: &network::NetworkLease,
    snapshot_taps: &[String],
) -> Result<()> {
    if snapshot_taps.is_empty() {
        // Old snapshots predate the always-present control NIC. The slot
        // reservation stays with the VM: a running VM may still own it and the
        // next start reuses it.
        bail!("snapshot '{snapshot_name}' has no control NIC; cold boot required");
    }
    let mut expected: Vec<String> = lease
        .taps()
        .into_iter()
        .map(|tap| tap.tap.clone())
        .collect();
    expected.sort();
    let mut found = snapshot_taps.to_vec();
    found.sort();
    if found != expected {
        // Leave the VM's reservation intact: an old VM may still be running on
        // that TAP, and the next start will reuse it.
        bail!(
            "snapshot '{}' expects TAPs [{}], but VM '{}' owns [{}]; restore it in the VM that \
             created it",
            snapshot_name,
            expected.join(", "),
            vm_name,
            found.join(", ")
        );
    }
    Ok(())
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

/// Membership of a boot plan as recorded in snapshot metadata.
pub(super) fn current_membership(plan: &BootPlan) -> Vec<SnapshotMember> {
    plan.members.iter().map(member_from_shared).collect()
}

fn member_from_shared(member: &SharedMember) -> SnapshotMember {
    SnapshotMember {
        name: member.name.clone(),
        path: member.path.clone(),
        tag: member.tag.clone(),
        guest_path: member.guest_path.clone(),
        read_only: member.read_only,
        kind: member.kind,
    }
}

fn member_from_manifest(member: &MembershipMember) -> SnapshotMember {
    SnapshotMember {
        name: member.name.clone(),
        path: member.path.clone(),
        tag: member.tag.clone(),
        guest_path: member.guest_path.clone(),
        read_only: member.read_only,
        kind: member.kind,
    }
}

/// Boot inputs of the plan that would be booted right now.
pub(super) fn current_boot(
    config: &Config,
    vm: &VmConfig,
    plan: &BootPlan,
    profile_share: bool,
) -> SnapshotBoot {
    boot_inputs(config, vm, plan.primary.clone(), profile_share)
}

fn boot_inputs(
    config: &Config,
    vm: &VmConfig,
    primary: String,
    profile_share: bool,
) -> SnapshotBoot {
    SnapshotBoot {
        vcpus: vm.vcpus,
        memory_mib: vm.memory_mib,
        rootfs_path: effective_rootfs(config, vm),
        rootfs_format: config.rootfs_format,
        network_index: vm.network_index,
        profile_share,
        primary: Some(primary),
    }
}

/// A checkpoint chosen for auto-resume, or the reasons no checkpoint matched.
pub(super) enum AutoResume {
    Resume(Box<SnapshotMeta>),
    ColdBoot(Vec<String>),
}

/// Pick the newest autosave whose recorded fingerprint matches `current`.
///
/// Mismatching and legacy candidates are skipped, never deleted, and reported
/// so the caller can log why the VM cold boots instead.
pub(super) fn select_auto_resume(
    candidates: Vec<SnapshotMeta>,
    current: &SnapshotBoot,
    current_members: &[SnapshotMember],
) -> AutoResume {
    let fingerprint = compute_fingerprint(current_members, current);
    let mut reasons = Vec::new();
    for snapshot in candidates {
        let Some(recorded) = snapshot.boot.as_ref() else {
            reasons.push(format!(
                "skipping autosave {}: snapshot has no recorded boot inputs",
                snapshot.name
            ));
            continue;
        };
        if snapshot.fingerprint_matches(&fingerprint) {
            return AutoResume::Resume(Box::new(snapshot));
        }
        let changes =
            describe_boot_changes(recorded, &snapshot.membership, current, current_members);
        reasons.push(format!(
            "skipping autosave {}: {}",
            snapshot.name,
            format_changes(&changes)
        ));
    }
    AutoResume::ColdBoot(reasons)
}

fn format_changes(changes: &[String]) -> String {
    if changes.is_empty() {
        "boot fingerprint differs".to_string()
    } else {
        changes.join(", ")
    }
}

/// Absolute ceiling for exported members per VM, independent of configuration.
const HARD_MAX_EXPORTED_MEMBERS: usize = 32;

/// Resolve a snapshot's recorded membership back into an executable plan.
///
/// This is deliberately independent of the project registry: a restore must
/// replay the device tree the checkpoint was taken with. Because the metadata
/// is a host file a corrupted or edited snapshot could point at, every recorded
/// member is re-validated with the same export-safety rules as a boot, and
/// duplicate names, tags, guest paths and canonical paths are refused.
pub(super) fn recorded_plan(manager: &VmManager, snapshot: &SnapshotMeta) -> Result<BootPlan> {
    if snapshot.is_legacy() {
        bail!(legacy_restore_error(&snapshot.name));
    }
    if snapshot.membership.is_empty() {
        bail!(
            "snapshot '{}' records no exported members; it cannot be restored into VM '{}'",
            snapshot.name,
            manager.vm().name
        );
    }
    let config = manager.config();
    let vm = manager.vm().name.as_str();
    let limit = (config.max_projects_per_vm as usize).clamp(1, HARD_MAX_EXPORTED_MEMBERS);
    if snapshot.membership.len() > limit {
        bail!(
            "snapshot '{}' records {} exported members, above max_projects_per_vm = {} (hard \
             ceiling {}); refusing to restore",
            snapshot.name,
            snapshot.membership.len(),
            config.max_projects_per_vm,
            HARD_MAX_EXPORTED_MEMBERS
        );
    }
    // Report every missing directory at once instead of the first failure the
    // per-member validator would raise. Worktrees are rejected in the main loop
    // whether or not their path still exists.
    let missing: Vec<String> = snapshot
        .membership
        .iter()
        .filter(|recorded| recorded.kind != ProjectKind::Worktree && !recorded.path.is_dir())
        .map(|recorded| format!("'{}' ({})", recorded.name, recorded.path.display()))
        .collect();
    if !missing.is_empty() {
        bail!(
            "snapshot '{}' cannot be restored because its recorded membership is no longer \
             exportable: missing member directories: {}. Restore every recorded project \
             directory and try again",
            snapshot.name,
            missing.join(", ")
        );
    }

    let paths = manager.paths();
    let mut seen_names = HashSet::new();
    let mut seen_tags: HashMap<&str, &str> = HashMap::new();
    let mut seen_guests: HashMap<&str, &str> = HashMap::new();
    let mut seen_paths: HashMap<PathBuf, &str> = HashMap::new();
    let mut members = Vec::with_capacity(snapshot.membership.len());
    for (index, recorded) in snapshot.membership.iter().enumerate() {
        if !seen_names.insert(recorded.name.as_str()) {
            bail!(
                "snapshot '{}' records member name '{}' twice; a restore would export it twice",
                snapshot.name,
                recorded.name
            );
        }
        if let Some(first) = seen_tags.insert(recorded.tag.as_str(), recorded.name.as_str()) {
            bail!(
                "snapshot '{}' records duplicate virtio-fs tag '{}' for members '{}' and '{}'",
                snapshot.name,
                recorded.tag,
                first,
                recorded.name
            );
        }
        if let Some(first) =
            seen_guests.insert(recorded.guest_path.as_str(), recorded.name.as_str())
        {
            bail!(
                "snapshot '{}' records duplicate guest path '{}' for members '{}' and '{}'",
                snapshot.name,
                recorded.guest_path,
                first,
                recorded.name
            );
        }
        if recorded.tag.len() > MAX_TAG_BYTES || !cloud_init::safe_token(&recorded.tag) {
            bail!(
                "snapshot '{}' records an unsafe virtio-fs tag '{}' for member '{}'",
                snapshot.name,
                recorded.tag,
                recorded.name
            );
        }
        if !cloud_init::safe_mount(&recorded.guest_path) {
            bail!(
                "snapshot '{}' records an unsafe guest path '{}' for member '{}'",
                snapshot.name,
                recorded.guest_path,
                recorded.name
            );
        }
        // Same rules as a boot: External through `validate_export_source`,
        // Clone exactly below this VM's managed workspaces, never a worktree.
        let canonical =
            validate_recorded_export(config, vm, &recorded.name, recorded.kind, &recorded.path)
                .with_context(|| format!("snapshot '{}' cannot be restored", snapshot.name))?;
        if canonical != recorded.path {
            bail!(
                "snapshot '{}' records member '{}' path {} which now resolves to {}; restore \
                 requires the recorded canonical path",
                snapshot.name,
                recorded.name,
                recorded.path.display(),
                canonical.display()
            );
        }
        if let Some(first) = seen_paths.insert(canonical.clone(), recorded.name.as_str()) {
            bail!(
                "snapshot '{}' records duplicate member path {} for members '{}' and '{}'",
                snapshot.name,
                canonical.display(),
                first,
                recorded.name
            );
        }
        let slug = recorded
            .guest_path
            .rsplit('/')
            .next()
            .filter(|slug| !slug.is_empty())
            .unwrap_or("project")
            .to_string();
        members.push(SharedMember {
            index,
            name: recorded.name.clone(),
            path: canonical,
            tag: recorded.tag.clone(),
            socket: paths.virtiofs_socket(index),
            pid_file: paths.virtiofs_pid(index),
            guest_path: recorded.guest_path.clone(),
            slug,
            read_only: recorded.read_only,
            kind: recorded.kind,
        });
    }
    let recorded_boot = snapshot.boot.as_ref().with_context(|| {
        format!(
            "snapshot '{}' has no recorded boot inputs and cannot be restored",
            snapshot.name
        )
    })?;
    // `/workspace` is a boot-time symlink to the recorded primary.
    let primary = recorded_boot
        .primary
        .clone()
        .filter(|primary| members.iter().any(|member| &member.name == primary))
        .with_context(|| {
            format!(
                "snapshot '{}' records primary project {:?}, which is not one of its members",
                snapshot.name, recorded_boot.primary
            )
        })?;
    let primary_slug = members
        .iter()
        .find(|member| member.name == primary)
        .map(|member| member.slug.clone())
        .context("the recorded primary project vanished")?;
    Ok(BootPlan {
        vm: manager.vm().name.clone(),
        members,
        primary,
        primary_slug,
        primary_changed: false,
    })
}

fn legacy_restore_error(name: &str) -> String {
    format!(
        "snapshot '{name}' is a legacy v1 snapshot without a boot fingerprint; it cannot be \
         restored because hegn cannot prove its device tree still matches. Take a new snapshot \
         with `hegn snapshot create`"
    )
}

/// Refuse a manual restore whose recorded fingerprint differs from the current
/// plan, naming every changed field.
fn ensure_restorable_fingerprint(
    manager: &VmManager,
    snapshot: &SnapshotMeta,
    current: &SnapshotBoot,
    current_members: &[SnapshotMember],
) -> Result<()> {
    if snapshot.is_legacy() {
        bail!(legacy_restore_error(&snapshot.name));
    }
    if let Some(owner) = snapshot.vm()
        && owner != manager.vm().name
    {
        bail!(
            "snapshot '{}' belongs to VM '{}'; restore it into that VM",
            snapshot.name,
            owner
        );
    }
    let Some(recorded) = snapshot.boot.as_ref() else {
        bail!(legacy_restore_error(&snapshot.name));
    };
    let fingerprint = compute_fingerprint(current_members, current);
    if snapshot.fingerprint_matches(&fingerprint) {
        return Ok(());
    }
    let changes = describe_boot_changes(recorded, &snapshot.membership, current, current_members);
    bail!(
        "snapshot '{}' was taken with a different boot configuration: {}. hegn will not restore \
         a checkpoint into a changed device tree; run `hegn vm apply {}` to boot the current \
         configuration, or take a new snapshot. There is no --force",
        snapshot.name,
        format_changes(&changes),
        manager.vm().name
    )
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
        // Capture membership and boot inputs from the VM that produced this
        // device tree, before the metadata is durable. A failure removes the
        // finished snapshot and resumes the VM.
        let captured = self.capture_metadata(&name, &prepared.final_dir, note);
        let meta = match captured {
            Ok(meta) => meta,
            Err(err) => {
                let _ = client.resume().await;
                let _ = fs::remove_dir_all(&prepared.final_dir);
                return Err(err);
            }
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

    /// Build v2 metadata for a snapshot whose VMM files are already in place.
    ///
    /// Membership comes from the running boot's manifest, because that is the
    /// device tree Cloud Hypervisor just wrote. The registry is only a fallback
    /// for a session that predates membership manifests.
    fn capture_metadata(&self, name: &str, dir: &Path, note: Option<&str>) -> Result<SnapshotMeta> {
        let profile_share = snapshot_fs_tags(dir)?
            .iter()
            .any(|tag| tag == PROFILE_SHARE_TAG);
        let membership_path = self.paths().membership;
        let (primary, membership) = match self.membership() {
            Some(manifest) => {
                if manifest.vm != self.vm.name {
                    bail!(
                        "cannot snapshot VM '{}': its membership manifest belongs to VM '{}'",
                        self.vm.name,
                        manifest.vm
                    );
                }
                (
                    manifest.primary.clone(),
                    manifest
                        .members
                        .iter()
                        .map(member_from_manifest)
                        .collect::<Vec<_>>(),
                )
            }
            None if !membership_path.exists() => {
                let plan = BootPlan::build(
                    &self.config,
                    &self.vm,
                    &self.store,
                    &self.registry,
                    &self.paths(),
                )?;
                (plan.primary.clone(), current_membership(&plan))
            }
            None => bail!(
                "cannot snapshot VM '{}': membership manifest {} is unreadable; restart the VM \
                 so it records its exported members",
                self.vm.name,
                membership_path.display()
            ),
        };
        let boot = boot_inputs(&self.config, &self.vm, primary, profile_share);
        let fingerprint = compute_fingerprint(&membership, &boot);
        let project = boot
            .primary
            .as_deref()
            .and_then(|primary| membership.iter().find(|member| member.name == primary))
            .map(|member| member.path.clone());
        Ok(SnapshotMeta {
            name: name.to_string(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.to_path_buf(),
            vm: Some(self.vm.name.clone()),
            project,
            size_bytes: dir_size(dir),
            note: note.map(str::to_string),
            version: SNAPSHOT_VERSION,
            boot_fingerprint: Some(fingerprint),
            membership,
            boot: Some(boot),
        })
    }

    /// Restore a snapshot into a brand new Cloud Hypervisor process.
    ///
    /// The current registry only decides whether the *current* plan still
    /// produces the recorded fingerprint; the restore itself always replays
    /// the recorded membership.
    pub async fn restore_snapshot(&self, snapshot: &SnapshotMeta) -> Result<()> {
        let paths = self.paths();
        paths.ensure()?;
        // Serialize with start/stop so another hegn process cannot recreate the
        // sockets while the snapshot boot is in flight.
        let _lock = self.lock(&paths)?;
        let registry = ProjectRegistry::load(&self.config.state_dir)?;
        let plan = BootPlan::build(&self.config, &self.vm, &self.store, &registry, &paths)
            .context("cannot compare the snapshot with the current boot plan")?;
        let profile_plan = profile::prepare_for_start(&self.config, &self.vm)?;
        for warning in &profile_plan.warnings {
            append_event_log(&paths.event_log, &format!("profile: {warning}"));
        }
        let current = current_boot(&self.config, &self.vm, &plan, profile_plan.share.is_some());
        let members = current_membership(&plan);
        ensure_restorable_fingerprint(self, snapshot, &current, &members)?;
        restore_snapshot_locked(self, snapshot, &profile_plan).await
    }
}

/// Restore while the caller already holds the lifecycle lock.
///
/// The device set comes exclusively from `snapshot.membership`; the project
/// registry is not consulted here.
pub(super) async fn restore_snapshot_locked(
    manager: &VmManager,
    snapshot: &SnapshotMeta,
    profile_plan: &ProfilePlan,
) -> Result<()> {
    let config = manager.config();
    let vm = manager.vm();
    let paths = manager.paths();
    if snapshot.is_legacy() {
        bail!(legacy_restore_error(&snapshot.name));
    }
    // A snapshot embeds the disk paths of the VM that created it. Restoring it
    // into another VM would make two VMs share one writable rootfs.
    if let Some(owner) = snapshot.vm()
        && owner != vm.name
    {
        bail!(
            "snapshot '{}' belongs to VM '{}'; restore it into that VM",
            snapshot.name,
            owner
        );
    }
    // Validate the recorded membership before stopping anything.
    let plan = recorded_plan(manager, snapshot)?;
    let primary_path = plan
        .members
        .iter()
        .find(|member| member.name == plan.primary)
        .map(|member| member.path.clone())
        .context("the recorded membership lost its primary project")?;
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
    // Prepare the keypair and resolve the guest account before the current VM
    // is stopped; an invalid ssh_user must not cost a running session.
    let (ssh_user, ssh_public_key) = prepare_guest_ssh(config, vm)?;
    if manager.unreadable_session() {
        bail!(
            "session metadata in {} is unreadable or from an older hegn version; \
             stop the old VM manually and remove that directory before restoring",
            paths.run_dir.display()
        );
    }
    // The snapshot embeds its device tree. Check the profile before stopping
    // the current VM so a bad profile cannot cost the user their running
    // session.
    let snapshot_fs = snapshot_fs_tags(&snapshot.dir)?;
    let needs_profile = snapshot_fs.iter().any(|tag| tag == PROFILE_SHARE_TAG);
    if needs_profile && profile_plan.share.is_none() {
        bail!(
            "snapshot '{}' contains the profile share, but the current profile has no files, \
             skills or environment to export",
            snapshot.name
        );
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
    // The control TAP is reserved in every mode; guarded mode adds the data
    // TAP. Off mode still gets a control link for SSH and metadata.
    let network_lease = network::acquire(config, vm)?;
    if let Err(err) = manager.persist_network_index(network_lease.slot) {
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
    validate_snapshot_taps(&snapshot.name, &vm.name, &network_lease, &snapshot_taps)?;

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
            .context("the recorded membership lost the primary guest path")?;
        let mounts = plan.mount_specs();
        let workspace = primary_path.to_string_lossy().into_owned();
        let identity = cloud_init::SeedIdentity {
            vm: &vm.name,
            project: &plan.primary,
            workspace: &workspace,
            ssh_user,
            ssh_public_key: &ssh_public_key,
        };
        let instance_id = cloud_init::instance_id(
            &mounts,
            primary_guest,
            &provision,
            profile_plan.share.as_ref(),
            Some(&network_lease),
            &config.local_endpoints,
            config.guest_credential_hardening,
            &identity,
        );
        cloud_init::write_seed(
            &paths.cloud_init,
            &mounts,
            primary_guest,
            &instance_id,
            &provision,
            profile_plan.share.as_ref(),
            Some(&network_lease),
            &config.local_endpoints,
            config.guest_credential_hardening,
            &identity,
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
    let serial_log = logs.create_session(&primary_path, started_at, std::process::id())?;

    let vm_child = spawn_restore(config, &paths, snapshot, &vm_binary, Some(&network_lease))?;
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

    // The session and membership records are rebuilt from the recorded set,
    // not from the registry, so `stop`/`attach` see the device tree that is
    // actually running.
    let membership = MembershipManifest {
        version: MEMBERSHIP_VERSION,
        vm: vm.name.clone(),
        primary: plan.primary.clone(),
        members: plan
            .members
            .iter()
            .map(|member| MembershipMember {
                name: member.name.clone(),
                path: member.path.clone(),
                tag: member.tag.clone(),
                guest_path: member.guest_path.clone(),
                read_only: member.read_only,
                kind: member.kind,
            })
            .collect(),
    };
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
        guest_ip: Some(network_lease.control_guest_ip()),
        ssh_user: None,
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
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::session::snapshot::AUTOSAVE_NOTE;
    use crate::vm::manager::SharedMember;
    use crate::vm::manager::test_support::{config_with_assets, manager, manager_for, plan_for};

    fn network_lease(slot: u8, with_data: bool) -> crate::network::NetworkLease {
        let control = crate::network::Lease {
            index: slot,
            vm: "demo".into(),
            tap: crate::network::control_tap_name(slot),
            mac: crate::network::control_mac(slot),
            host_ip: crate::network::control_host_ip(slot),
            guest_ip: crate::network::control_guest_ip(slot),
        };
        let data = with_data.then(|| crate::network::Lease {
            index: slot,
            vm: "demo".into(),
            tap: crate::network::tap_name(slot),
            mac: crate::network::mac(slot),
            host_ip: crate::network::host_ip(slot),
            guest_ip: crate::network::guest_ip(slot),
        });
        crate::network::NetworkLease {
            slot,
            vm: "demo".into(),
            data,
            control,
        }
    }

    fn taps(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn snapshot_taps_must_match_the_guarded_lease_exactly() {
        let lease = network_lease(3, true);
        assert!(
            validate_snapshot_taps("snap", "demo", &lease, &taps(&["hegn3", "hegnc3"])).is_ok()
        );
        // A snapshot from the data-only area is refused: it has no control NIC.
        let err = validate_snapshot_taps("snap", "demo", &lease, &taps(&["hegn3"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("expects TAPs [hegn3, hegnc3]"), "{err}");
        assert!(err.contains("owns [hegn3]"), "{err}");
        // A foreign or extra TAP is refused too.
        let err =
            validate_snapshot_taps("snap", "demo", &lease, &taps(&["hegn3", "hegnc3", "hegn9"]))
                .unwrap_err()
                .to_string();
        assert!(err.contains("hegn9"), "{err}");
    }

    #[test]
    fn snapshot_taps_accept_off_mode_with_only_the_control_nic() {
        let lease = network_lease(3, false);
        assert!(validate_snapshot_taps("snap", "demo", &lease, &taps(&["hegnc3"])).is_ok());
        // Off mode never has a data TAP; a snapshot that contains one is
        // contradictory and must be refused.
        let err = validate_snapshot_taps("snap", "demo", &lease, &taps(&["hegn3", "hegnc3"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("expects TAPs [hegnc3]"), "{err}");
        assert!(err.contains("owns [hegn3, hegnc3]"), "{err}");
    }

    #[test]
    fn snapshots_without_any_nic_require_a_cold_boot() {
        let err = validate_snapshot_taps("snap", "demo", &network_lease(3, true), &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("has no control NIC; cold boot required"),
            "{err}"
        );
    }

    fn snapshot(dir: &Path, owner: &str) -> SnapshotMeta {
        SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.to_path_buf(),
            vm: Some(owner.into()),
            project: None,
            size_bytes: 0,
            note: None,
            ..SnapshotMeta::default()
        }
    }

    /// A v2 snapshot that matches the manager's current plan.
    fn matching_snapshot(manager: &VmManager, dir: &Path) -> SnapshotMeta {
        let plan = plan_for(manager);
        let members = current_membership(&plan);
        let boot = current_boot(manager.config(), manager.vm(), &plan, false);
        SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.to_path_buf(),
            vm: Some(manager.vm().name.clone()),
            project: Some(plan.members[0].path.clone()),
            size_bytes: 0,
            note: None,
            version: SNAPSHOT_VERSION,
            boot_fingerprint: Some(compute_fingerprint(&members, &boot)),
            membership: members,
            boot: Some(boot),
        }
    }

    fn write_snapshot_files(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("config.json"), b"{}").unwrap();
        for name in ["state.json", "memory-ranges"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
    }

    /// A second exported member below the projects root, which the export
    /// safety rules allow (ordinary picker entries live there).
    fn extra_member(manager: &VmManager, name: &str, tag: &str, guest: &str) -> SnapshotMember {
        let path = manager.config().projects_root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        SnapshotMember {
            name: name.into(),
            path: path.canonicalize().unwrap(),
            tag: tag.into(),
            guest_path: guest.into(),
            read_only: false,
            kind: ProjectKind::External,
        }
    }

    fn plan_error(manager: &VmManager, snapshot: &SnapshotMeta) -> String {
        match recorded_plan(manager, snapshot) {
            Ok(_) => panic!("expected recorded_plan to refuse the snapshot"),
            Err(err) => format!("{err:#}"),
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn restore_rejects_missing_snapshot_files() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snapshot = matching_snapshot(&manager, &dir.path().join("missing"));
        let err = runtime()
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
        let snap_dir = dir.path().join("snap");
        write_snapshot_files(&snap_dir);
        let snapshot = matching_snapshot(&manager, &snap_dir);
        std::fs::remove_dir_all(&workspace).unwrap();
        let err = runtime()
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn restore_refuses_a_snapshot_from_another_vm() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let mut snapshot = matching_snapshot(&manager, &dir.path().join("snap"));
        snapshot.vm = Some("someone-else".into());
        let err = runtime()
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("belongs to VM 'someone-else'"), "{err}");
    }

    #[test]
    fn restore_refuses_a_legacy_snapshot_with_a_clear_message() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snap_dir = dir.path().join("snap");
        write_snapshot_files(&snap_dir);
        let legacy = snapshot(&snap_dir, "demo");
        let err = runtime()
            .block_on(manager.restore_snapshot(&legacy))
            .unwrap_err()
            .to_string();
        assert!(err.contains("legacy v1 snapshot"), "{err}");
        assert!(err.contains("cannot be restored"), "{err}");
        assert!(snap_dir.is_dir(), "a refused restore must not delete it");
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
        write_snapshot_files(&snap_dir);
        let snapshot = matching_snapshot(&manager, &snap_dir);
        let err = runtime()
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
        write_snapshot_files(&snap_dir);
        let snapshot = matching_snapshot(&manager, &snap_dir);
        let err = runtime()
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cloud-hypervisor binary"), "{err}");
    }

    #[test]
    fn manual_restore_refusal_names_every_changed_field() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snap_dir = dir.path().join("snap");
        write_snapshot_files(&snap_dir);
        let mut snapshot = matching_snapshot(&manager, &snap_dir);
        snapshot.boot = Some(SnapshotBoot {
            vcpus: 8,
            memory_mib: 8192,
            rootfs_path: std::path::PathBuf::from("/vms/rust/old-rootfs.qcow2"),
            rootfs_format: crate::config::RootfsFormat::Raw,
            network_index: Some(9),
            profile_share: true,
            primary: Some("demo".into()),
        });
        snapshot.membership[0].read_only = true;
        snapshot.boot_fingerprint = Some("0".repeat(64));

        let err = runtime()
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        for expected in [
            "vcpus 8 -> 1",
            "memory 8192 MiB -> 128 MiB",
            "rootfs /vms/rust/old-rootfs.qcow2 ->",
            "rootfs format raw -> qcow2",
            "network index 9 -> none",
            "profile share present -> absent",
            "member 'demo' read-only true -> false",
            "There is no --force",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in {err}");
        }
        assert!(snap_dir.is_dir(), "a refused restore must not delete it");
    }

    #[test]
    fn recorded_membership_wins_over_a_changed_registry() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config.clone(), dir.path());
        let plan = plan_for(&manager);

        // Record a second member the registry does not know about anymore. Its
        // path sorts before the existing member, proving the recorded boot
        // order (and therefore its tagged socket index) is preserved.
        let extra_path = dir.path().join("aaa-first");
        std::fs::create_dir_all(&extra_path).unwrap();
        let extra_path = extra_path.canonicalize().unwrap();
        let mut snapshot = matching_snapshot(&manager, &dir.path().join("snap"));
        snapshot.membership.push(SnapshotMember {
            name: "second".into(),
            path: extra_path.clone(),
            tag: "hegn-prj-1".into(),
            guest_path: "/projects/second".into(),
            read_only: true,
            kind: ProjectKind::External,
        });

        let recorded = recorded_plan(&manager, &snapshot).unwrap();
        assert_eq!(recorded.members.len(), 2);
        assert_eq!(recorded.members[1].name, "second");
        assert_eq!(recorded.members[1].path, extra_path);
        assert_eq!(recorded.members[1].tag, "hegn-prj-1");
        assert_eq!(recorded.members[1].guest_path, "/projects/second");
        assert!(recorded.members[1].read_only);
        assert_eq!(
            recorded.members[1].socket,
            manager.paths().virtiofs_socket(1)
        );
        assert_eq!(
            recorded.members[1].pid_file,
            manager.paths().virtiofs_pid(1)
        );
        assert_eq!(recorded.primary, plan.primary);
        assert_eq!(recorded.members[0].path, plan.members[0].path);
    }

    #[test]
    fn missing_recorded_members_are_a_hard_error_listing_all() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let mut snapshot = matching_snapshot(&manager, &dir.path().join("snap"));
        snapshot.membership.push(SnapshotMember {
            name: "gone".into(),
            path: dir.path().join("gone"),
            tag: "hegn-prj-1".into(),
            guest_path: "/projects/gone".into(),
            read_only: false,
            kind: ProjectKind::External,
        });
        snapshot.membership.push(SnapshotMember {
            name: "also-gone".into(),
            path: dir.path().join("also-gone"),
            tag: "hegn-prj-2".into(),
            guest_path: "/projects/also-gone".into(),
            read_only: false,
            kind: ProjectKind::External,
        });

        let err = recorded_plan(&manager, &snapshot).unwrap_err().to_string();
        assert!(err.contains("missing member directories"), "{err}");
        assert!(err.contains("'gone'"), "{err}");
        assert!(err.contains("'also-gone'"), "{err}");
    }

    #[test]
    fn a_recorded_member_that_resolves_elsewhere_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut snapshot = matching_snapshot(&manager, &dir.path().join("snap"));
        snapshot.membership[0].path = link;
        let err = recorded_plan(&manager, &snapshot).unwrap_err().to_string();
        assert!(err.contains("now resolves to"), "{err}");
    }

    #[test]
    fn a_recorded_primary_must_be_a_member() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let mut snapshot = matching_snapshot(&manager, &dir.path().join("snap"));
        snapshot.boot.as_mut().unwrap().primary = Some("missing".into());
        let err = recorded_plan(&manager, &snapshot).unwrap_err().to_string();
        assert!(err.contains("not one of its members"), "{err}");
    }

    #[test]
    fn recorded_plan_accepts_valid_external_and_clone_exports() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config.clone(), dir.path());
        let mut snapshot = matching_snapshot(&manager, &dir.path().join("snap"));

        let clone = config
            .vms_dir
            .join(&manager.vm().name)
            .join("workspaces")
            .join("clone-1");
        std::fs::create_dir_all(&clone).unwrap();
        let clone = clone.canonicalize().unwrap();
        snapshot.membership.push(SnapshotMember {
            name: "clone-1".into(),
            path: clone.clone(),
            tag: "hegn-prj-1".into(),
            guest_path: "/projects/clone-1".into(),
            read_only: false,
            kind: ProjectKind::Clone,
        });

        let recorded = recorded_plan(&manager, &snapshot).unwrap();
        assert_eq!(recorded.members.len(), 2);
        assert_eq!(recorded.members[0].kind, ProjectKind::External);
        assert_eq!(recorded.members[1].kind, ProjectKind::Clone);
        assert_eq!(recorded.members[1].path, clone);
    }

    #[test]
    fn recorded_plan_refuses_unsafe_external_paths() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config.clone(), dir.path());
        let base = matching_snapshot(&manager, &dir.path().join("snap"));

        let mut unsafe_paths: Vec<(PathBuf, &str)> = vec![(PathBuf::from("/"), "filesystem root")];
        if let Some(home) = dirs::home_dir()
            && let Ok(home) = home.canonicalize()
        {
            unsafe_paths.push((home, "home directory"));
        }
        for (label, path) in [
            ("state", config.state_dir.clone()),
            ("snapshots", config.snapshots_dir.clone()),
            ("vms", config.vms_dir.clone()),
        ] {
            std::fs::create_dir_all(&path).unwrap();
            unsafe_paths.push((path.canonicalize().unwrap(), label));
        }

        for (path, expected) in unsafe_paths {
            let mut snapshot = base.clone();
            snapshot.membership[0].path = path.clone();
            let err = plan_error(&manager, &snapshot);
            assert!(
                err.contains(expected),
                "{} should be refused for {expected:?}: {err}",
                path.display()
            );
            assert!(err.contains("cannot be restored"), "{err}");
        }
    }

    #[test]
    fn recorded_plan_refuses_a_worktree_member() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let mut snapshot = matching_snapshot(&manager, &dir.path().join("snap"));
        let worktree = manager.config().projects_root.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        snapshot.membership.push(SnapshotMember {
            name: "wt".into(),
            path: worktree.canonicalize().unwrap(),
            tag: "hegn-prj-1".into(),
            guest_path: "/projects/wt".into(),
            read_only: false,
            kind: ProjectKind::Worktree,
        });
        let err = plan_error(&manager, &snapshot);
        assert!(err.contains("is a worktree"), "{err}");
        assert!(
            err.contains("never exported separately"),
            "worktrees ride their parent's share: {err}"
        );
    }

    #[test]
    fn recorded_clones_must_live_in_this_vms_managed_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config.clone(), dir.path());
        let elsewhere = config
            .vms_dir
            .join("other")
            .join("workspaces")
            .join("stolen");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let mut snapshot = matching_snapshot(&manager, &dir.path().join("snap"));
        snapshot.membership.push(SnapshotMember {
            name: "stolen".into(),
            path: elsewhere.canonicalize().unwrap(),
            tag: "hegn-prj-1".into(),
            guest_path: "/projects/stolen".into(),
            read_only: false,
            kind: ProjectKind::Clone,
        });
        let err = plan_error(&manager, &snapshot);
        assert!(err.contains("must live at"), "{err}");
        assert!(err.contains("inside VM 'demo'"), "{err}");
    }

    #[test]
    fn recorded_plan_refuses_duplicate_membership_fields() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let base = matching_snapshot(&manager, &dir.path().join("snap"));

        // Duplicate canonical path under a different name.
        let mut duplicate_path = base.clone();
        let mut duplicate = extra_member(&manager, "copy", "hegn-prj-1", "/projects/copy");
        duplicate.path = duplicate_path.membership[0].path.clone();
        duplicate_path.membership.push(duplicate);
        let err = plan_error(&manager, &duplicate_path);
        assert!(err.contains("duplicate member path"), "{err}");
        assert!(err.contains("'demo' and 'copy'"), "{err}");

        // Duplicate tag.
        let mut duplicate_tag = base.clone();
        let tag = duplicate_tag.membership[0].tag.clone();
        let mut member = extra_member(&manager, "copy-tag", "hegn-prj-1", "/projects/copy-tag");
        member.tag = tag;
        duplicate_tag.membership.push(member);
        let err = plan_error(&manager, &duplicate_tag);
        assert!(err.contains("duplicate virtio-fs tag"), "{err}");
        assert!(err.contains("'demo' and 'copy-tag'"), "{err}");

        // Duplicate guest path.
        let mut duplicate_guest = base.clone();
        let guest = duplicate_guest.membership[0].guest_path.clone();
        let mut member = extra_member(&manager, "copy-guest", "hegn-prj-1", "/projects/copy-guest");
        member.guest_path = guest;
        duplicate_guest.membership.push(member);
        let err = plan_error(&manager, &duplicate_guest);
        assert!(err.contains("duplicate guest path"), "{err}");
        assert!(err.contains("'demo' and 'copy-guest'"), "{err}");

        // Duplicate name.
        let mut duplicate_name = base.clone();
        let name = duplicate_name.membership[0].name.clone();
        let mut member = extra_member(&manager, "copy-name", "hegn-prj-1", "/projects/copy-name");
        member.name = name;
        duplicate_name.membership.push(member);
        let err = plan_error(&manager, &duplicate_name);
        assert!(err.contains("records member name 'demo' twice"), "{err}");
    }

    #[test]
    fn recorded_plan_enforces_the_exported_member_quota() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.max_projects_per_vm = 1;
        let limited = manager(config.clone(), dir.path());
        let mut snapshot = matching_snapshot(&limited, &dir.path().join("snap"));
        snapshot.membership.push(extra_member(
            &limited,
            "second",
            "hegn-prj-1",
            "/projects/second",
        ));

        let err = plan_error(&limited, &snapshot);
        assert!(err.contains("above max_projects_per_vm = 1"), "{err}");
        assert!(err.contains("hard ceiling 32"), "{err}");

        // The hard ceiling applies even if a hand-edited config claims more.
        config.max_projects_per_vm = 100;
        let over_ceiling = manager(config, dir.path());
        let mut snapshot = matching_snapshot(&over_ceiling, &dir.path().join("snap"));
        for index in 0..HARD_MAX_EXPORTED_MEMBERS {
            snapshot.membership.push(SnapshotMember {
                name: format!("extra-{index}"),
                path: dir.path().join(format!("extra-{index}")),
                tag: format!("hegn-prj-{index}"),
                guest_path: format!("/projects/extra-{index}"),
                read_only: false,
                kind: ProjectKind::External,
            });
        }
        assert_eq!(snapshot.membership.len(), HARD_MAX_EXPORTED_MEMBERS + 1);
        let err = plan_error(&over_ceiling, &snapshot);
        assert!(err.contains("above max_projects_per_vm = 100"), "{err}");
        assert!(err.contains("hard ceiling 32"), "{err}");
    }

    #[test]
    fn auto_resume_skips_a_fingerprint_mismatch_and_explains_why() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let plan = plan_for(&manager);
        let members = current_membership(&plan);
        let current = current_boot(manager.config(), manager.vm(), &plan, false);

        let mut old = matching_snapshot(&manager, &dir.path().join("snap"));
        old.name = "autosave-old".into();
        old.note = Some(AUTOSAVE_NOTE.into());
        old.boot = Some(SnapshotBoot {
            vcpus: 4,
            ..current.clone()
        });
        old.boot_fingerprint = Some(compute_fingerprint(
            &old.membership,
            old.boot.as_ref().unwrap(),
        ));

        match select_auto_resume(vec![old], &current, &members) {
            AutoResume::ColdBoot(reasons) => {
                assert_eq!(reasons.len(), 1);
                assert!(
                    reasons[0].contains("skipping autosave autosave-old"),
                    "{reasons:?}"
                );
                assert!(reasons[0].contains("vcpus 4 -> 1"), "{reasons:?}");
            }
            AutoResume::Resume(_) => panic!("a mismatched checkpoint must not be resumed"),
        }
    }

    #[test]
    fn auto_resume_picks_the_matching_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let plan = plan_for(&manager);
        let members = current_membership(&plan);
        let current = current_boot(manager.config(), manager.vm(), &plan, false);

        let mut matching = matching_snapshot(&manager, &dir.path().join("snap"));
        matching.name = "autosave-good".into();
        matching.note = Some(AUTOSAVE_NOTE.into());
        match select_auto_resume(vec![matching], &current, &members) {
            AutoResume::Resume(snapshot) => assert_eq!(snapshot.name, "autosave-good"),
            AutoResume::ColdBoot(reasons) => panic!("expected a resume, got {reasons:?}"),
        }
    }

    #[test]
    fn auto_resume_never_selects_a_legacy_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let plan = plan_for(&manager);
        let members = current_membership(&plan);
        let current = current_boot(manager.config(), manager.vm(), &plan, false);

        let mut legacy = snapshot(&dir.path().join("snap"), "demo");
        legacy.name = "autosave-legacy".into();
        legacy.note = Some(AUTOSAVE_NOTE.into());
        match select_auto_resume(vec![legacy], &current, &members) {
            AutoResume::ColdBoot(reasons) => {
                assert!(
                    reasons[0].contains("no recorded boot inputs"),
                    "{reasons:?}"
                );
            }
            AutoResume::Resume(_) => panic!("a legacy checkpoint must never be resumed"),
        }
    }

    #[test]
    fn snapshot_metadata_records_the_manifest_membership_in_boot_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config.clone(), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let plan = plan_for(&manager);
        let mut manifest = MembershipManifest::from_plan(&plan);
        let second = dir.path().join("second");
        std::fs::create_dir_all(&second).unwrap();
        let second = second.canonicalize().unwrap();
        manifest.members.push(MembershipMember {
            name: "second".into(),
            path: second.clone(),
            tag: "hegn-prj-1".into(),
            guest_path: "/projects/second".into(),
            read_only: true,
            kind: ProjectKind::Clone,
        });
        manifest.save(&paths.membership).unwrap();

        let snap_dir = dir.path().join("snap");
        write_snapshot_files(&snap_dir);
        let meta = manager
            .capture_metadata("snap", &snap_dir, Some(AUTOSAVE_NOTE))
            .unwrap();
        assert_eq!(meta.version, SNAPSHOT_VERSION);
        assert_eq!(meta.vm(), Some("demo"));
        assert_eq!(meta.membership.len(), 2);
        assert_eq!(meta.membership[1].name, "second");
        assert!(meta.membership[1].read_only);
        assert_eq!(meta.membership[1].kind, ProjectKind::Clone);
        assert!(!meta.boot.as_ref().unwrap().profile_share);
        assert_eq!(
            meta.boot_fingerprint.as_deref().unwrap(),
            compute_fingerprint(&meta.membership, meta.boot.as_ref().unwrap())
        );
        assert_eq!(meta.note.as_deref(), Some(AUTOSAVE_NOTE));
    }

    #[test]
    fn capture_without_a_membership_manifest_falls_back_to_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snap_dir = dir.path().join("snap");
        write_snapshot_files(&snap_dir);
        let meta = manager.capture_metadata("snap", &snap_dir, None).unwrap();
        assert_eq!(meta.membership.len(), 1);
        assert_eq!(meta.membership[0].name, "demo");
        assert_eq!(meta.boot.as_ref().unwrap().primary.as_deref(), Some("demo"));
    }

    #[test]
    fn capture_refuses_an_unreadable_membership_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.membership, b"{not json").unwrap();
        let snap_dir = dir.path().join("snap");
        write_snapshot_files(&snap_dir);
        let err = manager
            .capture_metadata("snap", &snap_dir, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unreadable"), "{err}");
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

    #[test]
    fn member_conversion_keeps_every_device_field() {
        let member = SharedMember {
            index: 3,
            name: "parser".into(),
            path: std::path::PathBuf::from("/projects/parser"),
            tag: "hegn-prj-3".into(),
            socket: std::path::PathBuf::from("/run/virtiofs-3.sock"),
            pid_file: std::path::PathBuf::from("/run/virtiofs-3.pid"),
            guest_path: "/projects/parser".into(),
            slug: "parser".into(),
            read_only: true,
            kind: ProjectKind::Clone,
        };
        let recorded = member_from_shared(&member);
        assert_eq!(recorded.name, member.name);
        assert_eq!(recorded.path, member.path);
        assert_eq!(recorded.tag, member.tag);
        assert_eq!(recorded.guest_path, member.guest_path);
        assert!(recorded.read_only);
        assert_eq!(recorded.kind, ProjectKind::Clone);
    }
}
