//! Snapshot metadata model and name validation.
//!
//! A v2 snapshot is bound to exactly one VM and one exact boot membership. The
//! metadata records the exported members that made up the VMM device tree plus
//! the boot inputs that change it, and hashes both into
//! [`SnapshotMeta::boot_fingerprint`]. A restore is only allowed when the
//! current inputs produce the same fingerprint.
//!
//! Snapshot files written by hegn v1 have no fingerprint or membership. They
//! still deserialize, but [`SnapshotMeta::is_legacy`] marks them so they are
//! never auto-selected, restored or pruned automatically.

use std::path::PathBuf;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{NetworkMode, RootfsFormat};
use crate::project::ProjectKind;
use crate::shared::text::human_bytes as human_size;

/// Metadata version written by this hegn.
pub const SNAPSHOT_VERSION: u32 = 2;

/// Version assumed for metadata files that predate the `version` key.
const LEGACY_VERSION: u32 = 1;

/// Domain separator mixed into [`compute_fingerprint`].
///
/// v3 hashes the cloud-init seed version and the network mode. Bumping the
/// domain invalidates every checkpoint written before those inputs were part
/// of the fingerprint, so an old snapshot cannot be resumed into a different
/// seed or NIC layout.
const FINGERPRINT_DOMAIN: &str = "hegn-snapshot-boot-v3";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub vm_state: String,
    /// Directory passed to Cloud Hypervisor as `file://<dir>`.
    pub dir: PathBuf,
    /// VM that owns this snapshot. Writers emit `vm`; `instance` and
    /// `sandbox` are accepted so files written before the rename still load.
    #[serde(default, rename = "vm", alias = "instance", alias = "sandbox")]
    pub vm: Option<String>,
    /// Primary project directory at snapshot time, for display and scoping.
    pub project: Option<PathBuf>,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub note: Option<String>,
    /// Metadata version; v1 files have no `version` key.
    #[serde(default = "legacy_meta_version")]
    pub version: u32,
    /// sha256 (lowercase hex) over [`SnapshotBoot`] and the exported
    /// membership, in boot order. Missing for v1 snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_fingerprint: Option<String>,
    /// Exported members at snapshot time, in boot order. Worktrees are never
    /// exported separately, so they never appear here.
    #[serde(default)]
    pub membership: Vec<SnapshotMember>,
    /// Boot inputs the fingerprint was computed from, kept structured so a
    /// mismatch can name the changed fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<SnapshotBoot>,
}

fn legacy_meta_version() -> u32 {
    LEGACY_VERSION
}

impl Default for SnapshotMeta {
    fn default() -> Self {
        Self {
            name: String::new(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: PathBuf::new(),
            vm: None,
            project: None,
            size_bytes: 0,
            note: None,
            version: SNAPSHOT_VERSION,
            boot_fingerprint: None,
            membership: Vec::new(),
            boot: None,
        }
    }
}

impl SnapshotMeta {
    pub fn size_label(&self) -> String {
        human_size(self.size_bytes)
    }

    /// VM that owns this snapshot; `None` only for pre-v2 metadata.
    pub fn vm(&self) -> Option<&str> {
        self.vm.as_deref()
    }

    /// True for snapshot files written before fingerprinting existed.
    ///
    /// A v2 file always records both a fingerprint and its structured boot
    /// inputs, so a hand-edited file that drops one is treated as legacy
    /// instead of being silently restored.
    pub fn is_legacy(&self) -> bool {
        self.version < SNAPSHOT_VERSION || self.boot_fingerprint.is_none() || self.boot.is_none()
    }

    /// Human label for a legacy snapshot, used by the CLI/TUI listings.
    pub fn legacy_label(&self) -> Option<&'static str> {
        self.is_legacy()
            .then_some("legacy v1 snapshot (no boot fingerprint; not restorable)")
    }

    /// Cloud Hypervisor's required files. Snapshot directories from other VMM
    /// formats are intentionally not accepted.
    pub fn has_vm_files(&self) -> bool {
        self.dir.is_dir()
            && self.dir.join("config.json").is_file()
            && self.dir.join("state.json").is_file()
            && self.dir.join("memory-ranges").is_file()
    }

    /// A snapshot may be restored only when it has a fingerprint and the VMM
    /// files are present. Legacy snapshots are never restorable.
    pub fn is_restorable(&self) -> bool {
        !self.is_legacy() && self.has_vm_files()
    }

    /// Compare this snapshot's recorded fingerprint with a freshly computed
    /// one. Legacy snapshots never match.
    pub fn fingerprint_matches(&self, current: &str) -> bool {
        !self.is_legacy() && self.boot_fingerprint.as_deref() == Some(current)
    }

    /// Short fingerprint for list rows, or `legacy` when there is none.
    pub fn fingerprint_label(&self) -> String {
        match self.boot_fingerprint.as_deref() {
            Some(value) if value.len() >= 12 => value[..12].to_string(),
            Some(value) => value.to_string(),
            None => "legacy".to_string(),
        }
    }

    pub fn source_url(&self) -> String {
        format!("file://{}", self.dir.display())
    }
}

/// One exported project member as recorded in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMember {
    pub name: String,
    /// Canonical host directory exported at snapshot time.
    pub path: PathBuf,
    /// virtio-fs tag (also the socket index identity).
    pub tag: String,
    /// Guest mount point the member was mounted on.
    pub guest_path: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub kind: ProjectKind,
}

/// Boot inputs that change the VMM device tree but are not membership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBoot {
    pub vcpus: u8,
    pub memory_mib: u32,
    /// Effective root filesystem path passed to Cloud Hypervisor.
    pub rootfs_path: PathBuf,
    pub rootfs_format: RootfsFormat,
    /// Guarded-network TAP slice, kept across restores.
    #[serde(default)]
    pub network_index: Option<u8>,
    /// Cloud-init seed generation the guest was provisioned with. Missing in
    /// v2 files, which deserialize as `0` and therefore never match.
    #[serde(default)]
    pub seed_version: u32,
    /// Network mode of the recorded boot. Missing in v2 files, which
    /// deserialize as [`NetworkMode::Guarded`] and are invalidated by the
    /// fingerprint domain bump.
    #[serde(default)]
    pub network_mode: NetworkMode,
    /// Whether the profile/skill share was part of the device tree.
    #[serde(default)]
    pub profile_share: bool,
    /// Picker name of the project `/workspace` pointed at.
    #[serde(default)]
    pub primary: Option<String>,
}

impl Default for SnapshotBoot {
    fn default() -> Self {
        Self {
            vcpus: 0,
            memory_mib: 0,
            rootfs_path: PathBuf::new(),
            rootfs_format: RootfsFormat::default(),
            network_index: None,
            seed_version: 0,
            network_mode: NetworkMode::default(),
            profile_share: false,
            primary: None,
        }
    }
}

/// Stable, length-prefixed sha256 over the boot inputs and the ordered
/// exported membership.
///
/// Every field is hashed as `len(label) || label || len(value) || value`, so
/// no combination of values can produce the same byte stream as another.
pub fn compute_fingerprint(membership: &[SnapshotMember], boot: &SnapshotBoot) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "domain", FINGERPRINT_DOMAIN);
    hash_field(&mut hasher, "vcpus", &boot.vcpus.to_string());
    hash_field(&mut hasher, "memory_mib", &boot.memory_mib.to_string());
    hash_field(
        &mut hasher,
        "rootfs_path",
        &boot.rootfs_path.to_string_lossy(),
    );
    hash_field(&mut hasher, "rootfs_format", boot.rootfs_format.as_str());
    hash_field(
        &mut hasher,
        "network_index",
        &optional_u8(boot.network_index),
    );
    hash_field(&mut hasher, "seed_version", &boot.seed_version.to_string());
    hash_field(&mut hasher, "network_mode", boot.network_mode.label());
    hash_field(
        &mut hasher,
        "profile_share",
        if boot.profile_share {
            "present"
        } else {
            "absent"
        },
    );
    // `/workspace` is a boot-time symlink to the primary member, so a primary
    // change alters the guest layout even when the device tree is identical.
    hash_field(
        &mut hasher,
        "primary",
        boot.primary.as_deref().unwrap_or("none"),
    );
    hash_field(&mut hasher, "members", &membership.len().to_string());
    for (index, member) in membership.iter().enumerate() {
        hash_field(&mut hasher, "member_index", &index.to_string());
        hash_field(&mut hasher, "member_name", &member.name);
        hash_field(&mut hasher, "member_tag", &member.tag);
        hash_field(&mut hasher, "member_path", &member.path.to_string_lossy());
        hash_field(&mut hasher, "member_guest_path", &member.guest_path);
        hash_field(
            &mut hasher,
            "member_read_only",
            if member.read_only { "1" } else { "0" },
        );
        hash_field(&mut hasher, "member_kind", kind_label(member.kind));
    }
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, label: &str, value: &str) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn optional_u8(value: Option<u8>) -> String {
    value.map_or_else(|| "none".to_string(), |index| index.to_string())
}

fn kind_label(kind: ProjectKind) -> &'static str {
    match kind {
        ProjectKind::External => "external",
        ProjectKind::Clone => "clone",
        ProjectKind::Worktree => "worktree",
    }
}

/// Explain every boot input that differs between a snapshot and the current
/// plan, in a stable order. An empty result means the inputs are identical.
pub fn describe_boot_changes(
    recorded: &SnapshotBoot,
    recorded_members: &[SnapshotMember],
    current: &SnapshotBoot,
    current_members: &[SnapshotMember],
) -> Vec<String> {
    let mut changes = Vec::new();
    if recorded.vcpus != current.vcpus {
        changes.push(format!("vcpus {} -> {}", recorded.vcpus, current.vcpus));
    }
    if recorded.memory_mib != current.memory_mib {
        changes.push(format!(
            "memory {} MiB -> {} MiB",
            recorded.memory_mib, current.memory_mib
        ));
    }
    if recorded.rootfs_path != current.rootfs_path {
        changes.push(format!(
            "rootfs {} -> {}",
            recorded.rootfs_path.display(),
            current.rootfs_path.display()
        ));
    }
    if recorded.rootfs_format != current.rootfs_format {
        changes.push(format!(
            "rootfs format {} -> {}",
            recorded.rootfs_format.as_str(),
            current.rootfs_format.as_str()
        ));
    }
    if recorded.network_index != current.network_index {
        changes.push(format!(
            "network index {} -> {}",
            optional_u8(recorded.network_index),
            optional_u8(current.network_index)
        ));
    }
    if recorded.seed_version != current.seed_version {
        changes.push(format!(
            "seed version {} -> {}",
            recorded.seed_version, current.seed_version
        ));
    }
    if recorded.network_mode != current.network_mode {
        changes.push(format!(
            "network mode {} -> {}",
            recorded.network_mode.label(),
            current.network_mode.label()
        ));
    }
    if recorded.profile_share != current.profile_share {
        changes.push(format!(
            "profile share {} -> {}",
            presence(recorded.profile_share),
            presence(current.profile_share)
        ));
    }
    for member in recorded_members {
        match current_members
            .iter()
            .find(|candidate| candidate.name == member.name)
        {
            None => changes.push(format!("member '{}' removed", member.name)),
            Some(now) => {
                if now.path != member.path {
                    changes.push(format!(
                        "member '{}' path {} -> {}",
                        member.name,
                        member.path.display(),
                        now.path.display()
                    ));
                }
                if now.tag != member.tag {
                    changes.push(format!(
                        "member '{}' tag {} -> {}",
                        member.name, member.tag, now.tag
                    ));
                }
                if now.guest_path != member.guest_path {
                    changes.push(format!(
                        "member '{}' guest path {} -> {}",
                        member.name, member.guest_path, now.guest_path
                    ));
                }
                if now.read_only != member.read_only {
                    changes.push(format!(
                        "member '{}' read-only {} -> {}",
                        member.name, member.read_only, now.read_only
                    ));
                }
                if now.kind != member.kind {
                    changes.push(format!(
                        "member '{}' kind {} -> {}",
                        member.name,
                        kind_label(member.kind),
                        kind_label(now.kind)
                    ));
                }
            }
        }
    }
    for member in current_members {
        if !recorded_members
            .iter()
            .any(|candidate| candidate.name == member.name)
        {
            changes.push(format!("member '{}' added", member.name));
        }
    }
    if recorded.primary != current.primary {
        changes.push(format!(
            "primary {} -> {}",
            recorded.primary.as_deref().unwrap_or("none"),
            current.primary.as_deref().unwrap_or("none")
        ));
    }
    changes
}

fn presence(value: bool) -> &'static str {
    if value { "present" } else { "absent" }
}

/// Note written into metadata of automatic rolling snapshots. Only these are
/// eligible for auto-resume and retention pruning; manual checkpoints are
/// never deleted or loaded implicitly.
pub const AUTOSAVE_NOTE: &str = "autosave";

/// Paths reserved for a new snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPaths {
    /// Directory the VMM writes into (a staging directory in production).
    pub dir: PathBuf,
    /// Final private snapshot directory once `finalize` has moved it.
    pub final_dir: PathBuf,
    pub meta_path: PathBuf,
}

/// Validate a snapshot name. Names become directory names, so traversal and
/// hidden entries are rejected.
pub fn sanitize_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("snapshot name cannot be empty");
    }
    if name.len() > 100 {
        bail!("snapshot name is too long");
    }
    if name.starts_with('.') || name.contains('/') || name.contains('\\') {
        bail!("snapshot name must not contain path separators or start with '.'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("snapshot name may only contain ASCII letters, digits, '-', '_' and '.'");
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, path: &str, tag: &str, guest: &str) -> SnapshotMember {
        SnapshotMember {
            name: name.into(),
            path: PathBuf::from(path),
            tag: tag.into(),
            guest_path: guest.into(),
            read_only: false,
            kind: ProjectKind::External,
        }
    }

    fn membership() -> Vec<SnapshotMember> {
        vec![
            member(
                "parser",
                "/projects/parser",
                "hegn-prj-0",
                "/projects/parser",
            ),
            member("cli", "/projects/cli", "hegn-prj-1", "/projects/cli"),
        ]
    }

    fn boot() -> SnapshotBoot {
        SnapshotBoot {
            vcpus: 2,
            memory_mib: 2048,
            rootfs_path: PathBuf::from("/vms/rust/rootfs.qcow2"),
            rootfs_format: RootfsFormat::Qcow2,
            network_index: Some(3),
            seed_version: 13,
            network_mode: NetworkMode::Guarded,
            profile_share: true,
            primary: Some("parser".into()),
        }
    }

    fn meta() -> SnapshotMeta {
        SnapshotMeta {
            name: "demo".into(),
            vm: Some("rust".into()),
            boot_fingerprint: Some(compute_fingerprint(&membership(), &boot())),
            membership: membership(),
            boot: Some(boot()),
            ..SnapshotMeta::default()
        }
    }

    #[test]
    fn source_url_is_a_file_url() {
        let meta = SnapshotMeta {
            name: "demo".into(),
            dir: PathBuf::from("/snapshots/demo"),
            ..SnapshotMeta::default()
        };
        assert_eq!(meta.source_url(), "file:///snapshots/demo");
    }

    #[test]
    fn a_missing_snapshot_is_not_restorable() {
        let dir = tempfile::tempdir().unwrap();
        let meta = SnapshotMeta {
            name: "missing".into(),
            dir: dir.path().join("missing"),
            vm: Some("rust".into()),
            boot_fingerprint: Some("0".repeat(64)),
            boot: Some(boot()),
            ..SnapshotMeta::default()
        };
        assert!(!meta.is_restorable());
        assert!(!meta.is_legacy());
    }

    #[test]
    fn human_sizes_are_readable() {
        assert_eq!(human_size(12), "12 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn fingerprint_is_deterministic_and_length_is_sha256_hex() {
        let first = compute_fingerprint(&membership(), &boot());
        assert_eq!(first, compute_fingerprint(&membership(), &boot()));
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn every_boot_input_changes_the_fingerprint() {
        let base = compute_fingerprint(&membership(), &boot());
        let variants = [
            SnapshotBoot { vcpus: 4, ..boot() },
            SnapshotBoot {
                memory_mib: 4096,
                ..boot()
            },
            SnapshotBoot {
                rootfs_path: PathBuf::from("/vms/rust/rootfs.raw"),
                ..boot()
            },
            SnapshotBoot {
                rootfs_format: RootfsFormat::Raw,
                ..boot()
            },
            SnapshotBoot {
                network_index: Some(4),
                ..boot()
            },
            SnapshotBoot {
                network_index: None,
                ..boot()
            },
            SnapshotBoot {
                seed_version: 12,
                ..boot()
            },
            SnapshotBoot {
                network_mode: NetworkMode::Off,
                ..boot()
            },
            SnapshotBoot {
                profile_share: false,
                ..boot()
            },
            SnapshotBoot {
                primary: Some("cli".into()),
                ..boot()
            },
        ];
        for variant in variants {
            let other = compute_fingerprint(&membership(), &variant);
            assert_ne!(base, other, "{variant:?}");
        }
    }

    #[test]
    fn membership_order_tags_read_only_and_guest_paths_change_the_fingerprint() {
        let base = compute_fingerprint(&membership(), &boot());

        let mut reordered = membership();
        reordered.reverse();
        assert_ne!(base, compute_fingerprint(&reordered, &boot()));

        let mut read_only = membership();
        read_only[1].read_only = true;
        assert_ne!(base, compute_fingerprint(&read_only, &boot()));

        let mut retagged = membership();
        retagged[0].tag = "other-tag".into();
        assert_ne!(base, compute_fingerprint(&retagged, &boot()));

        let mut reguested = membership();
        reguested[0].guest_path = "/projects/parser-2".into();
        assert_ne!(base, compute_fingerprint(&reguested, &boot()));

        let mut kind = membership();
        kind[0].kind = ProjectKind::Clone;
        assert_ne!(base, compute_fingerprint(&kind, &boot()));

        let mut added = membership();
        added.push(member(
            "extra",
            "/projects/extra",
            "hegn-prj-2",
            "/projects/extra",
        ));
        assert_ne!(base, compute_fingerprint(&added, &boot()));
    }

    #[test]
    fn a_primary_change_is_named() {
        let recorded = boot();
        let current = SnapshotBoot {
            primary: Some("cli".into()),
            ..boot()
        };
        let changes = describe_boot_changes(&recorded, &membership(), &current, &membership());
        assert_eq!(changes, vec!["primary parser -> cli"]);
    }

    #[test]
    fn every_changed_field_is_named() {
        let recorded = boot();
        let mut current = SnapshotBoot {
            vcpus: 4,
            memory_mib: 4096,
            rootfs_path: PathBuf::from("/vms/rust/other.qcow2"),
            rootfs_format: RootfsFormat::Raw,
            network_index: Some(5),
            seed_version: 12,
            network_mode: NetworkMode::Off,
            profile_share: false,
            primary: Some("cli".into()),
        };
        let mut current_members = membership();
        current_members[0].tag = "hegn-prj-9".into();
        current_members[1].read_only = true;
        current_members.push(member(
            "extra",
            "/projects/extra",
            "hegn-prj-2",
            "/projects/extra",
        ));
        current.primary = Some("cli".into());

        let changes = describe_boot_changes(&recorded, &membership(), &current, &current_members);
        let joined = changes.join("; ");
        for expected in [
            "vcpus 2 -> 4",
            "memory 2048 MiB -> 4096 MiB",
            "rootfs /vms/rust/rootfs.qcow2 -> /vms/rust/other.qcow2",
            "rootfs format qcow2 -> raw",
            "network index 3 -> 5",
            "seed version 13 -> 12",
            "network mode guarded -> off",
            "profile share present -> absent",
            "member 'parser' tag hegn-prj-0 -> hegn-prj-9",
            "member 'cli' read-only false -> true",
            "member 'extra' added",
            "primary parser -> cli",
        ] {
            assert!(
                joined.contains(expected),
                "missing {expected:?} in {joined}"
            );
        }
        assert_eq!(
            describe_boot_changes(&recorded, &membership(), &recorded, &membership()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_removed_member_is_named() {
        let changes = describe_boot_changes(&boot(), &membership(), &boot(), &membership()[..1]);
        assert_eq!(changes, vec!["member 'cli' removed"]);
    }

    #[test]
    fn v1_metadata_is_legacy_and_never_restorable() {
        let raw = serde_json::json!({
            "name": "old",
            "created_at": "2026-01-01T00:00:00Z",
            "vm_state": "Paused",
            "dir": "/snapshots/old",
            "instance": "rust",
            "project": "/projects/parser",
            "size_bytes": 1,
        });
        let meta: SnapshotMeta = serde_json::from_value(raw).unwrap();
        assert_eq!(meta.version, 1);
        assert!(meta.is_legacy());
        assert!(!meta.is_restorable());
        assert!(meta.legacy_label().unwrap().contains("legacy"));
        assert_eq!(meta.vm(), Some("rust"));
        assert_eq!(meta.fingerprint_label(), "legacy");
        assert!(!meta.fingerprint_matches("whatever"));
    }

    #[test]
    fn v2_round_trips_and_writes_vm_while_accepting_the_instance_alias() {
        let meta = meta();
        let raw = serde_json::to_string_pretty(&meta).unwrap();
        assert!(raw.contains("\"vm\": \"rust\""), "{raw}");
        assert!(!raw.contains("\"instance\""), "{raw}");
        assert!(raw.contains("\"version\": 2"), "{raw}");
        assert!(raw.contains("\"boot_fingerprint\""), "{raw}");
        assert!(raw.contains("\"membership\""), "{raw}");
        assert!(raw.contains("\"seed_version\": 13"), "{raw}");
        assert!(raw.contains("\"network_mode\": \"guarded\""), "{raw}");
        let loaded: SnapshotMeta = serde_json::from_str(&raw).unwrap();
        assert_eq!(loaded, meta);

        let legacy_named = raw.replace("\"vm\": \"rust\"", "\"instance\": \"rust\"");
        let loaded: SnapshotMeta = serde_json::from_str(&legacy_named).unwrap();
        assert_eq!(loaded.vm(), Some("rust"));
        assert!(!loaded.is_legacy());
    }

    #[test]
    fn v2_metadata_without_the_new_boot_fields_is_incompatible() {
        let mut raw = serde_json::to_value(meta()).unwrap();
        let boot_fields = raw["boot"].as_object_mut().unwrap();
        boot_fields.remove("seed_version");
        boot_fields.remove("network_mode");

        let loaded: SnapshotMeta = serde_json::from_value(raw).unwrap();
        let recorded = loaded.boot.as_ref().unwrap();
        // Old v2 files still load: serde falls back to zero/guarded, but the
        // v3 fingerprint domain and the seed version make them incompatible.
        assert_eq!(recorded.seed_version, 0);
        assert_eq!(recorded.network_mode, NetworkMode::Guarded);
        assert!(!loaded.is_legacy(), "the v2 structure is still intact");
        assert!(!loaded.fingerprint_matches(&compute_fingerprint(&loaded.membership, recorded)));

        let changes = describe_boot_changes(recorded, &loaded.membership, &boot(), &membership());
        assert!(
            changes
                .iter()
                .any(|change| change == "seed version 0 -> 13"),
            "{changes:?}"
        );
    }

    #[test]
    fn a_v2_file_missing_its_inputs_is_treated_as_legacy() {
        let mut meta = meta();
        meta.boot = None;
        assert!(meta.is_legacy());
        meta.boot = Some(boot());
        meta.boot_fingerprint = None;
        assert!(meta.is_legacy());
    }

    #[test]
    fn fingerprint_labels_are_short_and_stable() {
        let mut meta = meta();
        assert_eq!(meta.fingerprint_label().len(), 12);
        meta.boot_fingerprint = Some("abc".into());
        assert_eq!(meta.fingerprint_label(), "abc");
    }

    #[test]
    fn snapshot_names_reject_traversal_absolute_and_hidden_entries() {
        for bad in [
            "",
            "   ",
            ".",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            ".hidden",
            "/absolute",
            "with space",
            "ümlaut",
        ] {
            assert!(sanitize_name(bad).is_err(), "{bad:?}");
        }
        assert!(sanitize_name(&"a".repeat(101)).is_err());
        assert!(sanitize_name(&"a".repeat(100)).is_ok());
    }

    #[test]
    fn snapshot_names_are_trimmed_and_keep_inner_dots() {
        assert_eq!(sanitize_name("  demo.v1 ").unwrap(), "demo.v1");
        assert_eq!(
            sanitize_name("2026-01-02_03.04").unwrap(),
            "2026-01-02_03.04"
        );
    }
}
