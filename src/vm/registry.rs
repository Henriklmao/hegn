//! Multi-VM registry and project-selector resolution.
//!
//! Each VM owns its `vm.toml`, runtime directory and workspaces; membership
//! lives in the private [`ProjectRegistry`]. This module is the small
//! convenience layer the CLI and TUI use to list, resolve, create (transitional
//! solo VMs) and delete VMs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::config::{Config, Harness};
use crate::network;
use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
use crate::vm::config::{VmConfig, VmStore};
use crate::vm::manager::VmManager;

use super::manager::process_matches;

/// A VM entry shown in the CLI and TUI.
///
/// `project` and `workspace` are transitional single-project views of the VM's
/// primary member; Phase 6 replaces them with the full membership list.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VmSummary {
    pub name: String,
    pub harness: Harness,
    /// Picker name of the primary project, if one is assigned.
    pub primary: Option<String>,
    /// Number of separately exported projects.
    pub members: usize,
    /// Primary (or first) exported project path; empty when the VM is empty.
    pub project: PathBuf,
    /// Transitional alias of `project`.
    pub workspace: PathBuf,
    pub state: String,
    pub vm_pid: Option<u32>,
    pub serial_log: Option<PathBuf>,
    pub config_path: PathBuf,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub created_at: DateTime<Utc>,
    pub error: Option<String>,
}

/// Result of deleting a VM directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub removed: bool,
    /// Workspace root kept on disk when files were not deleted.
    pub kept_workspace: Option<PathBuf>,
}

/// Delete a VM's private rootfs so the next start copies a fresh base image.
///
/// Only the per-VM copy below `vms_dir/<name>` is eligible. An explicit
/// `rootfs_image` is shared state and is always refused. The file must be a
/// regular, non-symlinked `rootfs.<format>` directly inside the VM directory;
/// workspaces and every other VM file are left untouched.
pub fn remove_private_rootfs(config: &Config, vm: &VmConfig) -> Result<Option<PathBuf>> {
    if let Some(image) = &vm.rootfs_image {
        bail!(
            "VM '{}' uses the explicit rootfs_image {}; rebuild only manages a private copy",
            vm.name,
            image.display()
        );
    }
    let dir = vm.dir(&config.vms_dir);
    match fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing symlinked VM directory {}", dir.display());
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("VM path {} is not a directory", dir.display());
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", dir.display()));
        }
    }

    let dest = vm.rootfs_path(config);
    if dest.parent() != Some(dir.as_path()) {
        bail!(
            "refusing to delete rootfs {}: expected it directly inside {}",
            dest.display(),
            dir.display()
        );
    }
    let expected = format!("rootfs.{}", config.rootfs_format.as_str());
    if dest.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
        bail!(
            "refusing to delete rootfs {}: expected the private file name {expected}",
            dest.display()
        );
    }

    match fs::symlink_metadata(&dest) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to delete symlinked rootfs {}", dest.display());
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("rootfs {} is not a regular file", dest.display());
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", dest.display()));
        }
    }
    fs::remove_file(&dest)
        .with_context(|| format!("failed to delete rootfs {}", dest.display()))?;
    Ok(Some(dest))
}

/// Multi-VM registry; the global [`Config`] supplies machine-wide settings.
#[derive(Debug, Clone)]
pub struct VmRegistry {
    config: Config,
    store: VmStore,
}

impl VmRegistry {
    pub fn new(config: Config) -> Self {
        let store = VmStore::new(&config);
        Self { config, store }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn store(&self) -> &VmStore {
        &self.store
    }

    pub fn list(&self) -> Result<Vec<VmConfig>> {
        self.store.list()
    }

    pub fn manager(&self, vm: VmConfig) -> Result<VmManager> {
        VmManager::new(self.config.clone(), vm)
    }

    pub fn manager_by_name(&self, name: &str) -> Result<VmManager> {
        self.manager(self.store.load(name)?)
    }

    /// Resolve a VM by its name, or a project by picker name or path.
    pub fn resolve(&self, selector: &str) -> Result<VmConfig> {
        if let Ok(vm) = self.store.load(selector) {
            return Ok(vm);
        }
        let registry = ProjectRegistry::load(&self.config.state_dir)?;
        if let Some(entry) = registry.get(selector) {
            return self.vm_for_entry(entry);
        }
        let direct = PathBuf::from(selector);
        let candidate = if direct.is_dir() {
            direct.canonicalize().unwrap_or(direct)
        } else {
            self.config.projects_root.join(selector)
        };
        if let Some(entry) = registry.find_by_path(&candidate) {
            return self.vm_for_entry(entry);
        }
        bail!(
            "no VM named '{selector}' and no project at {} or {}",
            PathBuf::from(selector).display(),
            self.config.projects_root.join(selector).display()
        )
    }

    fn vm_for_entry(&self, entry: &ProjectEntry) -> Result<VmConfig> {
        let Some(vm) = entry.vm.as_deref() else {
            bail!(
                "project '{}' is not assigned to a VM; run `hegn project assign {} --vm <name>`",
                entry.name,
                entry.name
            );
        };
        self.store
            .load(vm)
            .with_context(|| format!("project '{}' names unknown VM '{vm}'", entry.name))
    }

    /// Return the VM for a project, creating a solo VM named after the project
    /// when the project is unassigned.
    ///
    /// This is the transitional bridge for the current `create`/`import`/`open`
    /// flows. Phase 5/6 make the VM choice explicit.
    pub fn ensure_for_project(&self, project: &Path, harness: Harness) -> Result<VmConfig> {
        let project_path = project
            .canonicalize()
            .with_context(|| format!("project {} does not exist", project.display()))?;
        if !project_path.is_dir() {
            bail!("{} is not a directory", project_path.display());
        }
        let mut registry = ProjectRegistry::load(&self.config.state_dir)?;
        if let Some(entry) = registry.find_by_path(&project_path) {
            if entry.kind == ProjectKind::Worktree {
                bail!(
                    "project '{}' is a worktree and inherits its parent's VM; open the parent \
                     instead",
                    entry.name
                );
            }
            if let Some(vm) = entry.vm.clone() {
                return self.store.load(&vm);
            }
            let name = entry.name.clone();
            let vm = self.create_solo_vm(&name, harness)?;
            registry.assign(&name, &vm.name, false)?;
            return Ok(vm);
        }

        let name = self.unique_project_name(&registry, &project_path);
        registry.upsert(ProjectEntry {
            name: name.clone(),
            path: project_path,
            vm: None,
            read_only: false,
            kind: ProjectKind::External,
            parent: None,
            managed: false,
            hidden: false,
            created_at: Utc::now(),
        })?;
        let vm = self.create_solo_vm(&name, harness)?;
        registry.assign(&name, &vm.name, false)?;
        Ok(vm)
    }

    fn create_solo_vm(&self, project_name: &str, harness: Harness) -> Result<VmConfig> {
        let vm_name = self.unique_name(project_name)?;
        let mut vm = VmConfig::from_config(&self.config, &vm_name, Some(harness));
        vm.primary = Some(project_name.to_string());
        self.store.save(&vm)?;
        Ok(vm)
    }

    fn unique_project_name(&self, registry: &ProjectRegistry, project: &Path) -> String {
        let raw = project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "project".to_string());
        let base = match crate::project::validate_project_name(&raw) {
            Ok(name) => name.trim().to_string(),
            Err(_) => {
                let slug = crate::vm::manager::slugify(&raw);
                if slug.is_empty() {
                    "project".into()
                } else {
                    slug
                }
            }
        };
        let mut candidate = base.clone();
        let mut suffix = 2;
        while registry
            .get(&candidate)
            .is_some_and(|entry| entry.path != project)
        {
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }
        candidate
    }

    /// Find a free VM name starting at `base`.
    pub fn unique_name(&self, base: &str) -> Result<String> {
        let base = crate::session::logs::sanitize_segment(&crate::vm::manager::slugify(base), 48);
        let mut candidate = base.clone();
        for suffix in 1..1000 {
            if !self.store.dir(&candidate)?.exists() {
                return Ok(candidate);
            }
            candidate = format!("{base}-{suffix}");
        }
        bail!("could not allocate a free VM name for '{base}'")
    }

    pub fn summaries(&self) -> Result<Vec<VmSummary>> {
        let registry = ProjectRegistry::load(&self.config.state_dir)?;
        let summaries = self
            .store
            .list()?
            .into_iter()
            .map(|vm| self.summary_with(&registry, vm))
            .collect();
        Ok(summaries)
    }

    pub fn summary(&self, vm: VmConfig) -> VmSummary {
        match ProjectRegistry::load(&self.config.state_dir) {
            Ok(registry) => self.summary_with(&registry, vm),
            Err(_) => self.summary_with_empty_registry(vm),
        }
    }

    fn summary_with_empty_registry(&self, vm: VmConfig) -> VmSummary {
        self.summary_fields(&vm, None, None, 0, None)
    }

    fn summary_with(&self, registry: &ProjectRegistry, vm: VmConfig) -> VmSummary {
        let paths = super::manager::VmPaths::from_vm(&self.config, &vm);
        let session: Option<super::manager::SessionRecord> =
            std::fs::read_to_string(&paths.session)
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok());
        let membership = super::manager::MembershipManifest::load(&paths.membership).ok();
        let exported = registry.exported_members(&vm.name);
        let members = membership
            .as_ref()
            .map(|manifest| manifest.members.len())
            .unwrap_or(exported.len());
        let project = membership
            .as_ref()
            .and_then(|manifest| {
                manifest
                    .members
                    .iter()
                    .find(|member| member.name == manifest.primary)
                    .or_else(|| manifest.members.first())
                    .map(|member| member.path.clone())
            })
            .or_else(|| {
                exported
                    .iter()
                    .find(|entry| vm.primary.as_deref() == Some(entry.name.as_str()))
                    .or_else(|| exported.first())
                    .map(|entry| entry.path.clone())
            })
            .unwrap_or_default();
        self.summary_fields(&vm, session, Some(project), members, None)
    }

    fn summary_fields(
        &self,
        vm: &VmConfig,
        session: Option<super::manager::SessionRecord>,
        project: Option<PathBuf>,
        members: usize,
        _error: Option<String>,
    ) -> VmSummary {
        let vm_pid = session.as_ref().map(|session| session.vm_pid);
        let binary = session
            .as_ref()
            .and_then(|session| session.vm_binary.clone())
            .unwrap_or_else(|| self.config.vm_binary());
        let alive = vm_pid.is_some_and(|pid| process_matches(pid, &binary));
        let project = project.unwrap_or_default();
        VmSummary {
            name: vm.name.clone(),
            harness: vm.harness,
            primary: vm.primary.clone(),
            members,
            project: project.clone(),
            workspace: project,
            state: if alive { "running" } else { "stopped" }.to_string(),
            vm_pid: vm_pid.filter(|pid| process_matches(*pid, &binary)),
            serial_log: session.and_then(|session| session.serial_log),
            config_path: self
                .store
                .config_path(&vm.name)
                .unwrap_or_else(|_| vm.dir(&self.config.vms_dir).join("vm.toml")),
            vcpus: vm.vcpus,
            memory_mib: vm.memory_mib,
            created_at: vm.created_at,
            error: _error,
        }
    }

    pub fn stop(&self, name: &str) -> Result<()> {
        self.manager_by_name(name)?.stop()
    }

    /// Delete a VM directory.
    ///
    /// Exported projects are unassigned first and never deleted. With
    /// `delete_files = true` the VM directory (including managed workspaces) is
    /// removed; otherwise it is renamed so the workspaces survive.
    pub fn delete(&self, name: &str, delete_files: bool) -> Result<DeleteOutcome> {
        let manager = self.manager_by_name(name)?;
        if manager.is_running() {
            manager.stop()?;
        }
        let mut registry = ProjectRegistry::load(&self.config.state_dir)?;
        let members: Vec<String> = registry
            .entries()
            .iter()
            .filter(|entry| {
                entry.vm.as_deref() == Some(name) && entry.kind != ProjectKind::Worktree
            })
            .map(|entry| entry.name.clone())
            .collect();
        for member in members {
            registry.unassign(&member)?;
        }
        network::release(&self.config, name);
        let dir = self.store.dir(name)?;
        if !dir.exists() {
            bail!("VM '{name}' does not exist");
        }
        if delete_files {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to delete {}", dir.display()))?;
            return Ok(DeleteOutcome {
                removed: true,
                kept_workspace: None,
            });
        }
        let kept = unique_kept_path(self.store.root(), name);
        fs::rename(&dir, &kept)
            .with_context(|| format!("failed to move {} to {}", dir.display(), kept.display()))?;
        let workspaces = kept.join("workspaces");
        Ok(DeleteOutcome {
            removed: true,
            kept_workspace: workspaces.is_dir().then_some(workspaces),
        })
    }
}

fn unique_kept_path(root: &Path, name: &str) -> PathBuf {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let mut candidate = root.join(format!("{name}-files-{stamp}"));
    let mut counter = 1;
    while candidate.exists() {
        candidate = root.join(format!("{name}-files-{stamp}-{counter}"));
        counter += 1;
    }
    candidate
}

/// Whether two project paths refer to the same directory.
///
/// The TUI discovers projects as `<projects_root>/<name>` while the CLI
/// canonicalizes them, so a symlinked projects root would otherwise look like a
/// different project (and trigger a bogus "VM already running" error).
pub fn same_project(a: &Path, b: &Path) -> bool {
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
    use super::*;
    use crate::vm::manager::test_support::config_with_assets;

    fn project(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join("projects").join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn same_project_compares_canonical_paths() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(same_project(&real, &link));
        assert!(!same_project(&real, &dir.path().join("missing")));
    }

    #[test]
    fn ensure_for_project_creates_a_solo_vm_and_assigns_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let registry = VmRegistry::new(config.clone());
        let demo = project(dir.path(), "demo");

        let vm = registry.ensure_for_project(&demo, Harness::Shell).unwrap();
        assert_eq!(vm.name, "demo");
        assert_eq!(vm.primary.as_deref(), Some("demo"));
        assert!(config.vms_dir.join("demo/vm.toml").is_file());

        let projects = ProjectRegistry::load(&config.state_dir).unwrap();
        let entry = projects.get("demo").unwrap();
        assert_eq!(entry.vm.as_deref(), Some("demo"));

        // A second call reuses the assignment.
        assert_eq!(
            registry
                .ensure_for_project(&demo, Harness::Shell)
                .unwrap()
                .name,
            "demo"
        );
        assert_eq!(registry.resolve("demo").unwrap().name, "demo");
        assert_eq!(
            registry.resolve(demo.to_str().unwrap()).unwrap().name,
            "demo"
        );
    }

    #[test]
    fn ensure_for_project_registers_an_unassigned_registry_entry() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let registry = VmRegistry::new(config.clone());
        let demo = project(dir.path(), "demo");
        let mut projects = ProjectRegistry::load(&config.state_dir).unwrap();
        projects
            .upsert(ProjectEntry {
                name: "picker".into(),
                path: demo.canonicalize().unwrap(),
                vm: None,
                read_only: false,
                kind: ProjectKind::External,
                parent: None,
                managed: false,
                hidden: false,
                created_at: Utc::now(),
            })
            .unwrap();

        let vm = registry.ensure_for_project(&demo, Harness::Codex).unwrap();
        assert_eq!(vm.name, "picker");
        assert_eq!(vm.harness, Harness::Codex);
        assert_eq!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("picker")
                .unwrap()
                .vm
                .as_deref(),
            Some("picker")
        );
    }

    #[test]
    fn summaries_are_empty_when_no_vms_exist() {
        let dir = tempfile::tempdir().unwrap();
        let registry = VmRegistry::new(config_with_assets(dir.path()));
        assert!(!registry.config().vms_dir.exists());
        assert!(registry.list().unwrap().is_empty());
        assert!(registry.summaries().unwrap().is_empty());
    }

    #[test]
    fn resolve_names_both_lookup_locations_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let registry = VmRegistry::new(config_with_assets(dir.path()));
        let err = registry.resolve("missing").unwrap_err().to_string();
        assert!(err.contains("no VM named 'missing'"), "{err}");
        assert!(err.contains("projects"), "{err}");
    }

    #[test]
    fn ensure_for_project_requires_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let registry = VmRegistry::new(config_with_assets(dir.path()));
        let missing = dir.path().join("missing-project");
        let err = registry
            .ensure_for_project(&missing, Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn summary_describes_the_primary_member_and_hides_a_recycled_pid() {
        use crate::vm::manager::SessionRecord;

        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let registry = VmRegistry::new(config.clone());
        let demo = project(dir.path(), "demo");
        let vm = registry.ensure_for_project(&demo, Harness::Shell).unwrap();
        let manager = registry.manager(vm.clone()).unwrap();
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: demo.clone(),
            workspace: demo.clone(),
            vm: vm.name.clone(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid: std::process::id(),
            // The recorded binary is gone, so a recycled pid must not be
            // reported as a running VM.
            vm_binary: Some(dir.path().join("missing-cloud-hypervisor")),
            relay_pid: 0,
            virtiofs_pids: Vec::new(),
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();

        let summary = registry.summary(vm.clone());
        assert_eq!(summary.state, "stopped");
        assert_eq!(summary.vm_pid, None);
        assert_eq!(summary.name, "demo");
        assert_eq!(summary.primary.as_deref(), Some("demo"));
        assert_eq!(summary.members, 1);
        assert_eq!(summary.project, demo.canonicalize().unwrap());
        assert_eq!(summary.vcpus, vm.vcpus);
        assert_eq!(summary.config_path, config.vms_dir.join("demo/vm.toml"));
    }

    #[test]
    fn delete_unassigns_projects_and_never_removes_them() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let registry = VmRegistry::new(config.clone());
        let demo = project(dir.path(), "demo");
        registry.ensure_for_project(&demo, Harness::Shell).unwrap();
        let outcome = registry.delete("demo", true).unwrap();
        assert!(outcome.removed);
        assert!(!config.vms_dir.join("demo").exists());
        assert!(demo.is_dir(), "the project directory must survive");
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("demo")
                .unwrap()
                .vm
                .is_none()
        );
    }

    #[test]
    fn unique_name_never_reuses_an_existing_vm() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let registry = VmRegistry::new(config.clone());
        registry
            .ensure_for_project(&project(dir.path(), "demo"), Harness::Shell)
            .unwrap();
        assert_eq!(registry.unique_name("demo").unwrap(), "demo-1");
    }

    #[test]
    fn remove_private_rootfs_only_deletes_the_per_vm_copy() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let store = VmStore::new(&config);
        let vm = VmConfig::from_config(&config, "demo", Some(Harness::Shell));
        store.save(&vm).unwrap();
        let rootfs = vm.rootfs_path(&config);
        std::fs::create_dir_all(rootfs.parent().unwrap()).unwrap();
        std::fs::write(&rootfs, b"disk").unwrap();
        std::fs::write(config.vms_dir.join("demo/keep.txt"), b"keep").unwrap();

        let removed = remove_private_rootfs(&config, &vm).unwrap();
        assert_eq!(removed.as_deref(), Some(rootfs.as_path()));
        assert!(!rootfs.exists());
        assert!(config.vms_dir.join("demo/keep.txt").is_file());
        assert_eq!(remove_private_rootfs(&config, &vm).unwrap(), None);
    }

    #[test]
    fn remove_private_rootfs_refuses_explicit_symlinked_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let store = VmStore::new(&config);
        let mut vm = VmConfig::from_config(&config, "demo", Some(Harness::Shell));
        vm.rootfs_image = Some(dir.path().join("shared.qcow2"));
        let err = remove_private_rootfs(&config, &vm).unwrap_err().to_string();
        assert!(err.contains("explicit rootfs_image"), "{err}");

        vm.rootfs_image = None;
        store.save(&vm).unwrap();
        let rootfs = vm.rootfs_path(&config);
        let target = dir.path().join("outside.img");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &rootfs).unwrap();
        let err = remove_private_rootfs(&config, &vm).unwrap_err().to_string();
        assert!(err.contains("symlinked rootfs"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }
}
