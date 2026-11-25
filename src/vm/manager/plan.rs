//! Deterministic boot plan for a VM's exported projects.
//!
//! Members come from the private project registry, never from `vm.toml`. They
//! are sorted by canonical path so the device tree is stable across boots. The
//! index in this list becomes the share tag suffix, the socket name and the
//! pid file name; the sanitized project name becomes the guest mount slug.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::project::{ProjectKind, ProjectRegistry};
use crate::vm::cloud_init::{self, MountSpec};
use crate::vm::config::{VmConfig, VmStore};

use super::paths::VmPaths;

/// Longest guest slug; paths must stay short and readable.
pub const MAX_SLUG_BYTES: usize = 24;
/// Cloud Hypervisor rejects tags above 36 bytes.
pub const MAX_TAG_BYTES: usize = 36;

/// One exported project member in boot order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedMember {
    /// Position in the boot plan; drives the tag, socket and pid file.
    pub index: usize,
    pub name: String,
    pub path: PathBuf,
    pub tag: String,
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub guest_path: String,
    /// Sanitized picker name used below `guest_projects_base`.
    pub slug: String,
    pub read_only: bool,
    pub kind: ProjectKind,
}

impl SharedMember {
    /// Cloud-init mount description for this member.
    pub fn mount_spec(&self) -> MountSpec {
        MountSpec {
            tag: self.tag.clone(),
            guest_path: self.guest_path.clone(),
            read_only: self.read_only,
        }
    }
}

/// Resolved membership for one boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    pub vm: String,
    pub members: Vec<SharedMember>,
    /// Picker name of the primary project that drives `/workspace`.
    pub primary: String,
    pub primary_slug: String,
    /// The VM config gained `primary` and must be saved.
    pub primary_changed: bool,
}

impl BootPlan {
    /// Build and validate the plan.
    ///
    /// Registry validation runs first, so duplicate paths, nesting, unknown
    /// VMs and `max_projects_per_vm` fail before any process is spawned.
    pub fn build(
        config: &Config,
        vm: &VmConfig,
        store: &VmStore,
        registry: &ProjectRegistry,
        paths: &VmPaths,
    ) -> Result<Self> {
        registry.validate(config, store)?;
        let mut members = registry.exported_members(&vm.name);
        if members.is_empty() {
            bail!(
                "VM '{}' has no projects; run `hegn project assign <project> --vm {}` first",
                vm.name,
                vm.name
            );
        }
        // The registry sorts by canonical path already; sorting again keeps the
        // plan independent of that implementation detail.
        members.sort_by(|a, b| a.path.cmp(&b.path));

        let missing: Vec<String> = members
            .iter()
            .filter(|entry| !entry.path.is_dir())
            .map(|entry| format!("'{}' ({})", entry.name, entry.path.display()))
            .collect();
        if !missing.is_empty() {
            bail!(
                "VM '{}' cannot boot: member director{} missing: {}; run `hegn project unassign` \
                 for the missing project",
                vm.name,
                if missing.len() == 1 {
                    "y is"
                } else {
                    "ies are"
                },
                missing.join(", ")
            );
        }

        let primary = vm
            .primary
            .clone()
            .unwrap_or_else(|| members[0].name.clone());
        if !members.iter().any(|entry| entry.name == primary) {
            bail!(
                "VM '{}' primary project '{}' is not an exported member; choose one of: {}",
                vm.name,
                primary,
                members
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        let slugs = unique_slugs(members.iter().map(|entry| entry.name.as_str()));
        let mut shared = Vec::with_capacity(members.len());
        let mut primary_slug = None;
        for (index, (entry, slug)) in members.iter().zip(slugs).enumerate() {
            let tag = format!("{}-{index}", config.share_tag_prefix);
            validate_tag(&tag)?;
            let guest_path = format!("{}/{slug}", config.guest_projects_base);
            if !cloud_init::safe_mount(&guest_path) {
                bail!(
                    "unsafe guest mount path for project '{}': {guest_path}",
                    entry.name
                );
            }
            if entry.name == primary {
                primary_slug = Some(slug.clone());
            }
            shared.push(SharedMember {
                index,
                name: entry.name.clone(),
                path: entry.path.clone(),
                tag,
                socket: paths.virtiofs_socket(index),
                pid_file: paths.virtiofs_pid(index),
                guest_path,
                slug,
                read_only: entry.read_only,
                kind: entry.kind,
            });
        }
        let primary_slug = primary_slug
            .with_context(|| format!("VM '{}' primary project '{primary}' vanished", vm.name))?;

        Ok(Self {
            vm: vm.name.clone(),
            members: shared,
            primary,
            primary_slug,
            primary_changed: vm.primary.is_none(),
        })
    }

    /// Guest path of the primary project and of the `/workspace` symlink.
    pub fn primary_guest_path(&self) -> Option<&str> {
        self.members
            .iter()
            .find(|member| member.name == self.primary)
            .map(|member| member.guest_path.as_str())
    }

    pub fn mount_specs(&self) -> Vec<MountSpec> {
        self.members.iter().map(SharedMember::mount_spec).collect()
    }

    /// Canonical paths exported by this plan.
    pub fn exported_paths(&self) -> Vec<&std::path::Path> {
        self.members
            .iter()
            .map(|member| member.path.as_path())
            .collect()
    }
}

/// Sanitize a picker name into a guest slug.
///
/// Only ASCII letters, digits, `-`, `_` and `.` survive; everything else
/// becomes `-`. The result is lowercased, trimmed and at most
/// [`MAX_SLUG_BYTES`] bytes.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || matches!(lower, '-' | '_' | '.') {
            slug.push(lower);
            last_dash = lower == '-';
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches(['-', '.']);
    let mut slug: String = slug.chars().take(MAX_SLUG_BYTES).collect();
    while slug.ends_with('-') || slug.ends_with('.') {
        slug.pop();
    }
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    }
}

/// Deduplicate slugs with `-2`, `-3`, … while keeping them within the limit.
fn unique_slugs<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut slugs = Vec::new();
    for name in names {
        let base = slugify(name);
        let count = counts.entry(base.clone()).or_insert(0);
        *count += 1;
        let mut suffix = *count;
        let mut candidate = if suffix == 1 {
            base.clone()
        } else {
            with_suffix(&base, suffix)
        };
        while used.contains(&candidate) {
            suffix += 1;
            candidate = with_suffix(&base, suffix);
        }
        used.insert(candidate.clone());
        slugs.push(candidate);
    }
    slugs
}

fn with_suffix(base: &str, suffix: usize) -> String {
    let suffix = format!("-{suffix}");
    let keep = MAX_SLUG_BYTES.saturating_sub(suffix.len());
    let mut stem: String = base.chars().take(keep).collect();
    while stem.ends_with('-') || stem.ends_with('.') {
        stem.pop();
    }
    format!("{stem}{suffix}")
}

fn validate_tag(tag: &str) -> Result<()> {
    if tag.len() > MAX_TAG_BYTES {
        bail!(
            "virtio-fs tag '{tag}' is {} bytes, above the {MAX_TAG_BYTES}-byte limit; shorten \
             share_tag_prefix",
            tag.len()
        );
    }
    if !cloud_init::safe_token(tag) {
        bail!("virtio-fs tag '{tag}' contains unsafe characters");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
    use crate::vm::config::VmConfig;
    use chrono::Utc;
    use std::fs;
    use std::path::Path;

    fn test_config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.projects_root = dir.join("projects");
        config.state_dir = dir.join("state");
        config.snapshots_dir = dir.join("snapshots");
        config.vms_dir = dir.join("vms");
        config.templates_dir = dir.join("templates");
        config.profiles_dir = dir.join("profiles");
        config
    }

    fn vm_store(config: &Config, name: &str) -> VmStore {
        let store = VmStore::new(config);
        store
            .save(&VmConfig::from_config(config, name, None))
            .unwrap();
        store
    }

    fn directory(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn entry(name: &str, path: &Path) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            path: path.to_path_buf(),
            vm: None,
            read_only: false,
            kind: ProjectKind::External,
            parent: None,
            managed: false,
            hidden: false,
            created_at: Utc::now(),
        }
    }

    fn registry_with(config: &Config, projects: &[(&str, &str); 3]) -> ProjectRegistry {
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        for (name, relative) in projects {
            let path = directory(&config.projects_root, relative);
            registry.upsert(entry(name, &path)).unwrap();
            registry.assign(name, "rust", false).unwrap();
        }
        registry
    }

    #[test]
    fn members_are_indexed_in_canonical_path_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let store = vm_store(&config, "rust");
        // Names deliberately disagree with path order.
        let registry = registry_with(
            &config,
            &[
                ("zeta", "projects/a"),
                ("alpha", "projects/z"),
                ("middle", "projects/m"),
            ],
        );
        let vm = store.load("rust").unwrap();
        let paths = VmPaths::from_vm(&config, &vm);
        let plan = BootPlan::build(&config, &vm, &store, &registry, &paths).unwrap();

        let names: Vec<&str> = plan.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["zeta", "middle", "alpha"]);
        for (index, member) in plan.members.iter().enumerate() {
            assert_eq!(member.index, index);
            assert_eq!(member.tag, format!("hegn-prj-{index}"));
            assert_eq!(member.socket, paths.virtiofs_socket(index));
            assert_eq!(member.pid_file, paths.virtiofs_pid(index));
            assert_eq!(member.guest_path, format!("/projects/{}", member.slug));
        }
    }

    #[test]
    fn primary_falls_back_to_the_first_member_and_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let store = vm_store(&config, "rust");
        let registry = registry_with(
            &config,
            &[
                ("alpha", "projects/a"),
                ("beta", "projects/b"),
                ("gamma", "projects/c"),
            ],
        );
        let vm = store.load("rust").unwrap();
        let paths = VmPaths::from_vm(&config, &vm);
        let plan = BootPlan::build(&config, &vm, &store, &registry, &paths).unwrap();
        assert_eq!(plan.primary, "alpha");
        assert_eq!(plan.primary_slug, "alpha");
        assert!(plan.primary_changed);

        let mut persisted = vm.clone();
        persisted.primary = Some(plan.primary.clone());
        store.save(&persisted).unwrap();
        let plan = BootPlan::build(&config, &persisted, &store, &registry, &paths).unwrap();
        assert!(!plan.primary_changed);
        assert_eq!(plan.primary, "alpha");
    }

    #[test]
    fn primary_must_name_an_exported_member() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let store = vm_store(&config, "rust");
        let registry = registry_with(
            &config,
            &[
                ("alpha", "projects/a"),
                ("beta", "projects/b"),
                ("gamma", "projects/c"),
            ],
        );
        let mut vm = store.load("rust").unwrap();
        vm.primary = Some("missing".into());
        let paths = VmPaths::from_vm(&config, &vm);
        let err = BootPlan::build(&config, &vm, &store, &registry, &paths)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not an exported member"), "{err}");
        assert!(err.contains("alpha"), "{err}");
    }

    #[test]
    fn an_empty_vm_fails_with_the_assign_hint() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let store = vm_store(&config, "rust");
        let registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let vm = store.load("rust").unwrap();
        let paths = VmPaths::from_vm(&config, &vm);
        let err = BootPlan::build(&config, &vm, &store, &registry, &paths)
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no projects"), "{err}");
        assert!(err.contains("hegn project assign"), "{err}");
    }

    #[test]
    fn a_missing_member_directory_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let store = vm_store(&config, "rust");
        let path = directory(&config.projects_root, "projects/gone");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("gone", &path)).unwrap();
        registry.assign("gone", "rust", false).unwrap();
        fs::remove_dir_all(&path).unwrap();

        let vm = store.load("rust").unwrap();
        let paths = VmPaths::from_vm(&config, &vm);
        let err = format!(
            "{:#}",
            BootPlan::build(&config, &vm, &store, &registry, &paths).unwrap_err()
        );
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn slugs_are_lowercased_limited_and_deduplicated() {
        let slugs =
            unique_slugs(["Parser", "parser", "A Project!", "a-project", "parser"].into_iter());
        assert_eq!(slugs[0], "parser");
        assert_eq!(slugs[1], "parser-2");
        assert_eq!(slugs[2], "a-project");
        assert_eq!(slugs[3], "a-project-2");
        assert_eq!(slugs[4], "parser-3");
        assert!(slugs.iter().all(|slug| slug.len() <= MAX_SLUG_BYTES));
    }

    #[test]
    fn long_names_stay_within_the_slug_limit_after_deduplication() {
        let long = "a".repeat(40);
        let another = format!("{long}b");
        let slugs = unique_slugs([long.as_str(), another.as_str()].into_iter());
        assert_eq!(slugs[0], "a".repeat(MAX_SLUG_BYTES));
        assert!(slugs[1].len() <= MAX_SLUG_BYTES, "{}", slugs[1]);
        assert!(slugs[1].ends_with("-2"), "{}", slugs[1]);
        assert_ne!(slugs[0], slugs[1]);
    }

    #[test]
    fn slugify_handles_unsafe_and_empty_names() {
        assert_eq!(slugify("My Project!"), "my-project");
        assert_eq!(slugify("A..B"), "a..b");
        assert_eq!(slugify("---"), "project");
        assert_eq!(slugify(""), "project");
        assert_eq!(slugify("CAFÉ"), "caf");
        assert_eq!(slugify(&"x".repeat(40)).len(), MAX_SLUG_BYTES);
    }

    #[test]
    fn tags_must_fit_the_cloud_hypervisor_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.share_tag_prefix = "p".repeat(35);
        let store = vm_store(&config, "rust");
        let registry = registry_with(
            &config,
            &[
                ("alpha", "projects/a"),
                ("beta", "projects/b"),
                ("gamma", "projects/c"),
            ],
        );
        let vm = store.load("rust").unwrap();
        let paths = VmPaths::from_vm(&config, &vm);
        let err = BootPlan::build(&config, &vm, &store, &registry, &paths)
            .unwrap_err()
            .to_string();
        assert!(err.contains("above the 36-byte limit"), "{err}");
    }

    #[test]
    fn read_only_members_reach_the_mount_spec() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let store = vm_store(&config, "rust");
        let path = directory(&config.projects_root, "projects/ro");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("ro", &path)).unwrap();
        registry.assign("ro", "rust", true).unwrap();

        let vm = store.load("rust").unwrap();
        let paths = VmPaths::from_vm(&config, &vm);
        let plan = BootPlan::build(&config, &vm, &store, &registry, &paths).unwrap();
        let spec = plan.mount_specs().remove(0);
        assert!(spec.read_only);
        assert_eq!(spec.tag, "hegn-prj-0");
        assert_eq!(spec.guest_path, "/projects/ro");
    }
}
