//! Project removal: unassigning the project from its VM, deleting a real
//! directory or unlinking an imported symlink, with paths always constrained to
//! the projects root.
//!
//! Removing a project never deletes a VM or a sibling project. A running VM
//! keeps its current shares until `hegn vm apply` restarts it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::import::validate_export_source;
use super::model::ProjectDeleteOutcome;
use super::registry::{ProjectEntry, ProjectKind, ProjectRegistry};
use crate::vm::config::VmStore;

/// How a picker entry was removed.
enum EntryRemoval {
    /// A real project directory was deleted.
    Deleted,
    /// An import symlink was removed; the target directory is untouched.
    RemovedLink,
    /// A real project directory remains on disk and will be hidden.
    Kept,
}

/// A primary change made before removing a project, so it can be rolled back.
pub(crate) struct PrimaryUpdate {
    vm: String,
    previous: Option<String>,
}

impl PrimaryUpdate {
    /// Move `removed`'s VM away from it when it is the current primary.
    ///
    /// Runs while the project is still registered, so the successor can be
    /// chosen from the remaining exported members in canonical-path order.
    pub(crate) fn prepare(
        config: &crate::config::Config,
        registry: &ProjectRegistry,
        removed: &ProjectEntry,
    ) -> Result<Option<Self>> {
        if removed.kind == ProjectKind::Worktree {
            return Ok(None);
        }
        let Some(vm_name) = removed.vm.as_deref() else {
            return Ok(None);
        };
        let store = VmStore::new(config);
        let mut vm = store.load(vm_name)?;
        if vm.primary.as_deref() != Some(removed.name.as_str()) {
            return Ok(None);
        }
        let previous = vm.primary.clone();
        vm.primary = registry
            .exported_members(vm_name)
            .into_iter()
            .find(|entry| entry.name != removed.name)
            .map(|entry| entry.name.clone());
        store.save(&vm)?;
        Ok(Some(Self {
            vm: vm_name.to_string(),
            previous,
        }))
    }

    /// Put the old primary back after a failed removal.
    pub(crate) fn restore(&self, config: &crate::config::Config) -> Result<()> {
        let store = VmStore::new(config);
        let mut vm = store.load(&self.vm)?;
        vm.primary = self.previous.clone();
        store.save(&vm)
    }
}

/// Remove a project from the picker.
///
/// The project is first removed from the private registry (updating the VM's
/// primary when needed). When `delete_files` is true the project directory is
/// removed; an imported symlink only loses the link and its target always
/// stays. When files are kept for a real directory, the entry is recorded as
/// hidden so the next scan does not re-add it.
pub fn delete_project(
    config: &crate::config::Config,
    entry: &Path,
    delete_files: bool,
) -> Result<ProjectDeleteOutcome> {
    // Validate containment before any destructive work. A caller passing an
    // outside/nested path must not lose that project's registry entry first.
    direct_child_root(&config.projects_root, entry)?;

    let canonical = entry
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", entry.display()))?;
    let mut registry = ProjectRegistry::load(&config.state_dir)?;
    let project = registry.find_by_path(&canonical).cloned();
    let primary = match &project {
        Some(project) => PrimaryUpdate::prepare(config, &registry, project)?,
        None => None,
    };
    if let Some(project) = &project {
        let removed = match registry.remove(&project.name) {
            Ok(removed) => removed,
            Err(err) => {
                if let Some(primary) = &primary {
                    let _ = primary.restore(config);
                }
                return Err(err);
            }
        };
        if !removed {
            if let Some(primary) = &primary {
                let _ = primary.restore(config);
            }
            bail!("project '{}' disappeared from the registry", project.name);
        }
        if let Err(err) = registry.validate(config, &VmStore::new(config)) {
            let _ = registry.upsert(project.clone());
            if let Some(primary) = &primary {
                let _ = primary.restore(config);
            }
            return Err(err);
        }
    }

    let removal = match remove_entry(&config.projects_root, entry, delete_files) {
        Ok(removal) => removal,
        Err(err) => {
            if let Some(project) = &project {
                let _ = registry.upsert(project.clone());
            }
            if let Some(primary) = &primary {
                let _ = primary.restore(config);
            }
            return Err(err);
        }
    };
    let kept = matches!(removal, EntryRemoval::Kept);
    let files_deleted = matches!(removal, EntryRemoval::Deleted);
    let mut store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
    store.forget_project(entry)?;
    store.set_project_hidden(entry, kept)?;
    Ok(ProjectDeleteOutcome {
        files_deleted,
        // Removing a project never removes a VM.
        instances: Vec::new(),
    })
}

/// Remove a registered External project, including one outside `projects_root`.
///
/// Imported symlinks and direct children of `projects_root` delegate to
/// [`delete_project`] so the picker entry and its file policy stay unchanged.
/// A registry-only path is removed from the registry; with `delete_files` it is
/// first re-validated as a safe export source and only then deleted.
pub fn delete_registered_project(
    config: &crate::config::Config,
    entry: &ProjectEntry,
    delete_files: bool,
) -> Result<ProjectDeleteOutcome> {
    if entry.kind != ProjectKind::External {
        bail!(
            "project '{}' is not an external project; use the workspace removal path",
            entry.name
        );
    }

    if let Some(link) = imported_picker_link(config, entry) {
        return delete_project(config, &link, delete_files);
    }
    if is_direct_child(&config.projects_root, &entry.path) {
        return delete_project(config, &entry.path, delete_files);
    }

    if !delete_files {
        remove_registry_only(config, entry)?;
        forget_registered_project(config, &entry.path)?;
        return Ok(ProjectDeleteOutcome {
            files_deleted: false,
            instances: Vec::new(),
        });
    }

    // Re-validate the canonical path before any registry or filesystem change.
    // `validate_export_source` rejects `/`, `$HOME`, `projects_root` and its
    // ancestors, and every path overlapping hegn state.
    let canonical = validate_export_source(
        &entry.path,
        &config.projects_root,
        &config.protected_paths(),
    )
    .with_context(|| {
        format!(
            "refusing to delete outside project '{}' at {}",
            entry.name,
            entry.path.display()
        )
    })?;
    if canonical != entry.path {
        bail!(
            "refusing to delete '{}': {} is not canonical (expected {})",
            entry.name,
            canonical.display(),
            entry.path.display()
        );
    }
    let root = config.projects_root.canonicalize().ok();
    if let Some(root) = &root
        && canonical.starts_with(root)
        && canonical.parent() != Some(root.as_path())
    {
        bail!(
            "refusing to delete {}: it is nested inside {} but is not a direct project",
            canonical.display(),
            root.display()
        );
    }
    match fs::symlink_metadata(&canonical) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to delete symlinked project path {}",
                canonical.display()
            );
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("project path {} is not a directory", canonical.display());
        }
        Ok(_) => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", canonical.display()));
        }
    }

    remove_registry_only(config, entry)?;
    if let Err(err) = fs::remove_dir_all(&canonical) {
        return Err(err).with_context(|| format!("failed to delete {}", canonical.display()));
    }
    forget_registered_project(config, &canonical)?;
    Ok(ProjectDeleteOutcome {
        files_deleted: true,
        instances: Vec::new(),
    })
}

/// Drop an outside-root registry entry without touching any project files.
fn remove_registry_only(config: &crate::config::Config, entry: &ProjectEntry) -> Result<()> {
    let mut registry = ProjectRegistry::load(&config.state_dir)?;
    let current = registry
        .get(&entry.name)
        .cloned()
        .with_context(|| format!("project '{}' is not registered", entry.name))?;
    let primary = PrimaryUpdate::prepare(config, &registry, &current)?;
    let result = (|| -> Result<()> {
        if !registry.remove(&current.name)? {
            bail!("project '{}' disappeared from the registry", current.name);
        }
        registry.validate(config, &VmStore::new(config))?;
        Ok(())
    })();
    if let Err(err) = result {
        let _ = registry.upsert(current);
        if let Some(primary) = &primary {
            let _ = primary.restore(config);
        }
        return Err(err);
    }
    Ok(())
}

fn forget_registered_project(config: &crate::config::Config, path: &Path) -> Result<()> {
    let mut store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
    store.forget_project(path)?;
    store.set_project_hidden(path, false)
}

/// `projects_root/<name>` when it is an import symlink pointing at `entry`.
fn imported_picker_link(config: &crate::config::Config, entry: &ProjectEntry) -> Option<PathBuf> {
    let picker = config.projects_root.join(&entry.name);
    let metadata = fs::symlink_metadata(&picker).ok()?;
    if !metadata.file_type().is_symlink() {
        return None;
    }
    let target = picker.canonicalize().ok()?;
    (target == entry.path).then_some(picker)
}

fn is_direct_child(root: &Path, path: &Path) -> bool {
    root.canonicalize()
        .ok()
        .is_some_and(|root| path.parent() == Some(root.as_path()))
}

/// Resolve `root` and verify that `entry` is a direct child of it.
///
/// This is a pure containment check with no filesystem mutation, so callers
/// can run it before touching the registry or any files.
fn direct_child_root(root: &Path, entry: &Path) -> Result<std::path::PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", root.display()))?;
    let parent = entry
        .parent()
        .with_context(|| format!("{} has no parent directory", entry.display()))?
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", entry.display()))?;
    if parent != root {
        bail!(
            "refusing to delete {}: it is not directly inside {}",
            entry.display(),
            root.display()
        );
    }
    if root == Path::new("/") {
        bail!("refusing to delete projects directly under the filesystem root");
    }
    Ok(root)
}

/// Remove a picker entry from `root`.
///
/// Imported projects are symlinks: removing the link never touches the imported
/// directory. Real directories are deleted only when `delete_files` is true and
/// are always constrained to a direct child of `root`.
fn remove_entry(root: &Path, entry: &Path, delete_files: bool) -> Result<EntryRemoval> {
    let root = direct_child_root(root, entry)?;

    let metadata = fs::symlink_metadata(entry)
        .with_context(|| format!("failed to inspect {}", entry.display()))?;
    if metadata.file_type().is_symlink() {
        fs::remove_file(entry).with_context(|| format!("failed to remove {}", entry.display()))?;
        return Ok(EntryRemoval::RemovedLink);
    }
    if !metadata.is_dir() {
        bail!("{} is not a project directory", entry.display());
    }
    if !delete_files {
        return Ok(EntryRemoval::Kept);
    }

    let canonical = entry
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", entry.display()))?;
    let home = dirs::home_dir().and_then(|home| home.canonicalize().ok());
    if canonical == Path::new("/")
        || canonical == root
        || home.as_ref().is_some_and(|home| &canonical == home)
    {
        bail!("refusing to delete {}", canonical.display());
    }
    if !canonical.starts_with(&root) {
        bail!(
            "refusing to delete {}: it resolves outside {}",
            entry.display(),
            root.display()
        );
    }
    fs::remove_dir_all(&canonical)
        .with_context(|| format!("failed to delete {}", canonical.display()))?;
    Ok(EntryRemoval::Deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_project_can_keep_or_delete_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let project = root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), "demo").unwrap();

        assert!(matches!(
            remove_entry(&root, &project, false).unwrap(),
            EntryRemoval::Kept
        ));
        assert!(project.is_dir(), "keeping files must not remove them");

        assert!(matches!(
            remove_entry(&root, &project, true).unwrap(),
            EntryRemoval::Deleted
        ));
        assert!(!project.exists());
    }

    #[test]
    fn removing_an_imported_project_only_removes_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let target = dir.path().join("elsewhere/demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("README.md"), "demo").unwrap();
        let link = root.join("demo");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Even an explicit file deletion must never follow the import link.
        assert!(matches!(
            remove_entry(&root, &link, true).unwrap(),
            EntryRemoval::RemovedLink
        ));
        assert!(!link.exists());
        assert!(target.join("README.md").is_file());
    }

    #[test]
    fn removing_refuses_paths_outside_the_projects_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert!(remove_entry(&root, &outside, true).is_err());
        assert!(outside.is_dir());
    }

    #[test]
    fn removing_refuses_entries_that_are_not_direct_children() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let project = root.join("demo");
        let nested = root.join("sub/nested");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("keep.txt"), "keep").unwrap();

        let err = remove_entry(&root, &nested, true)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("not directly inside"), "{err}");
        assert!(nested.join("keep.txt").is_file());

        // The projects root itself is never a removable entry either.
        let err = remove_entry(&root, &root, true).err().unwrap().to_string();
        assert!(err.contains("not directly inside"), "{err}");
        assert!(project.is_dir());
    }

    #[test]
    fn removing_refuses_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let file = root.join("notes.txt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&file, "keep").unwrap();

        let err = remove_entry(&root, &file, true).err().unwrap().to_string();
        assert!(err.contains("not a project directory"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
    }

    #[test]
    fn removing_a_real_directory_never_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let project = root.join("demo");
        let shared = dir.path().join("external");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir(root.join("keep")).unwrap();
        std::fs::write(shared.join("secret.txt"), "do not delete").unwrap();
        std::fs::write(project.join("README.md"), "demo").unwrap();
        std::os::unix::fs::symlink(&shared, project.join("external")).unwrap();
        std::os::unix::fs::symlink(&root, project.join("root-link")).unwrap();

        assert!(matches!(
            remove_entry(&root, &project, true).unwrap(),
            EntryRemoval::Deleted
        ));
        assert!(!project.exists());
        assert_eq!(
            std::fs::read_to_string(shared.join("secret.txt")).unwrap(),
            "do not delete"
        );
        assert!(root.join("keep").is_dir(), "siblings must survive");
    }

    #[test]
    fn removing_a_link_to_the_projects_root_only_removes_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let link = root.join("self");
        std::os::unix::fs::symlink(&root, &link).unwrap();

        assert!(matches!(
            remove_entry(&root, &link, true).unwrap(),
            EntryRemoval::RemovedLink
        ));
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(root.is_dir(), "the link target must never be deleted");
    }

    #[test]
    fn removing_a_broken_link_succeeds_without_touching_missing_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let missing = dir.path().join("missing-target");
        std::fs::create_dir_all(&root).unwrap();
        let link = root.join("broken");
        std::os::unix::fs::symlink(&missing, &link).unwrap();

        assert!(matches!(
            remove_entry(&root, &link, true).unwrap(),
            EntryRemoval::RemovedLink
        ));
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(!missing.exists());
    }

    #[allow(clippy::field_reassign_with_default)]
    fn test_config(dir: &Path) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.projects_root = dir.join("projects");
        config.state_dir = dir.join("state");
        config.snapshots_dir = dir.join("snapshots");
        config.vms_dir = dir.join("vms");
        config.templates_dir = dir.join("templates");
        config.profiles_dir = dir.join("profiles");
        config
    }

    #[test]
    fn delete_project_keeps_files_and_marks_the_entry_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), "demo").unwrap();

        let outcome = delete_project(&config, &project, false).unwrap();
        assert!(!outcome.files_deleted);
        assert!(outcome.instances.is_empty());
        assert!(project.join("README.md").is_file());

        let store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(store.is_hidden(&project));

        let outcome = delete_project(&config, &project, true).unwrap();
        assert!(outcome.files_deleted);
        assert!(!project.exists());
        let store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(!store.is_hidden(&project));
    }

    #[test]
    fn delete_project_unassigns_without_touching_the_vm_or_siblings() {
        use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
        use crate::vm::config::{VmConfig, VmStore};
        use chrono::Utc;

        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let project = config.projects_root.join("demo");
        let other = config.projects_root.join("other");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let project = project.canonicalize().unwrap();
        let other = other.canonicalize().unwrap();

        let store = VmStore::new(&config);
        store
            .save(&VmConfig::from_config(&config, "shared", None))
            .unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        for (name, path) in [("demo", &project), ("other", &other)] {
            registry
                .upsert(ProjectEntry {
                    name: name.into(),
                    path: path.clone(),
                    vm: None,
                    read_only: false,
                    kind: ProjectKind::External,
                    parent: None,
                    managed: false,
                    hidden: false,
                    created_at: Utc::now(),
                })
                .unwrap();
            registry.assign(name, "shared", false).unwrap();
        }

        let outcome = delete_project(&config, &project, true).unwrap();
        assert!(
            outcome.instances.is_empty(),
            "deleting a project never deletes a VM"
        );
        assert!(outcome.files_deleted);
        assert!(!project.exists());
        assert!(other.is_dir(), "sibling projects must survive");
        assert!(
            config.vms_dir.join("shared").is_dir(),
            "the VM must survive"
        );
        let registry = ProjectRegistry::load(&config.state_dir).unwrap();
        assert!(registry.find_by_path(&project).is_none());
        assert_eq!(
            registry.find_by_path(&other).unwrap().vm.as_deref(),
            Some("shared")
        );
    }

    #[test]
    fn delete_project_removes_only_the_import_link() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "demo").unwrap();
        let link =
            crate::project::import_project(&config.projects_root, &source, None, &[]).unwrap();

        let outcome = delete_project(&config, &link, true).unwrap();
        assert!(!outcome.files_deleted);
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert_eq!(
            std::fs::read_to_string(source.join("README.md")).unwrap(),
            "demo"
        );
        let store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(!store.is_hidden(&link));
    }

    fn external_entry(name: &str, path: &Path, vm: Option<&str>) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            path: path.canonicalize().unwrap(),
            vm: vm.map(str::to_string),
            read_only: false,
            kind: ProjectKind::External,
            parent: None,
            managed: false,
            hidden: false,
            created_at: chrono::Utc::now(),
        }
    }

    fn save_vm(config: &crate::config::Config, name: &str, primary: Option<&str>) {
        let mut vm = crate::vm::config::VmConfig::from_config(config, name, None);
        vm.primary = primary.map(str::to_string);
        crate::vm::config::VmStore::new(config).save(&vm).unwrap();
    }

    fn load_primary(config: &crate::config::Config, name: &str) -> Option<String> {
        crate::vm::config::VmStore::new(config)
            .load(name)
            .unwrap()
            .primary
    }

    #[test]
    fn delete_registered_external_outside_root_keeps_files_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let project = dir.path().join("code/parser");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), "keep me").unwrap();
        save_vm(&config, "rust", Some("parser"));
        let entry = external_entry("parser", &project, Some("rust"));
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(entry.clone())
            .unwrap();

        let outcome = delete_registered_project(&config, &entry, false).unwrap();
        assert!(!outcome.files_deleted);
        assert!(project.join("README.md").is_file());
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("parser")
                .is_none()
        );
        assert!(load_primary(&config, "rust").is_none());
    }

    #[test]
    fn delete_registered_external_outside_root_deletes_files_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let project = dir.path().join("code/parser");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), "delete me").unwrap();
        save_vm(&config, "rust", Some("parser"));
        let entry = external_entry("parser", &project, Some("rust"));
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(entry.clone())
            .unwrap();

        let outcome = delete_registered_project(&config, &entry, true).unwrap();
        assert!(outcome.files_deleted);
        assert!(!project.exists());
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("parser")
                .is_none()
        );
        assert!(load_primary(&config, "rust").is_none());
    }

    #[test]
    fn delete_registered_external_import_link_removes_only_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let source = dir.path().join("code/parser");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "target").unwrap();
        let link =
            crate::project::import_project(&config.projects_root, &source, None, &[]).unwrap();
        let entry = external_entry("parser", &source, None);
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(entry.clone())
            .unwrap();

        let outcome = delete_registered_project(&config, &entry, true).unwrap();
        assert!(!outcome.files_deleted, "an import link is not the target");
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert_eq!(
            std::fs::read_to_string(source.join("README.md")).unwrap(),
            "target"
        );
    }

    #[test]
    fn delete_registered_external_refuses_unsafe_outside_paths() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let root_entry = external_entry("root", Path::new("/"), None);
        let err = format!(
            "{:#}",
            delete_registered_project(&config, &root_entry, true).unwrap_err()
        );
        assert!(err.contains("filesystem root"), "{err}");

        if let Some(home) = dirs::home_dir()
            && home.is_dir()
        {
            let home_entry = external_entry("home", &home, None);
            let err = format!(
                "{:#}",
                delete_registered_project(&config, &home_entry, true).unwrap_err()
            );
            assert!(err.contains("home directory"), "{err}");
        }

        std::fs::create_dir_all(&config.state_dir).unwrap();
        let state_entry = external_entry("state", &config.state_dir, None);
        let err = format!(
            "{:#}",
            delete_registered_project(&config, &state_entry, true).unwrap_err()
        );
        assert!(err.contains("overlaps hegn state"), "{err}");
        assert!(config.state_dir.is_dir());
    }

    #[test]
    fn delete_registered_external_refuses_nested_paths_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let nested = config.projects_root.join("parser/vendor/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("keep.txt"), "keep").unwrap();
        let entry = external_entry("nested", &nested, None);
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(entry.clone())
            .unwrap();

        let err = delete_registered_project(&config, &entry, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nested inside"), "{err}");
        assert!(nested.join("keep.txt").is_file());
    }

    #[test]
    fn deleting_the_primary_hands_the_flag_to_the_next_member() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let parser = config.projects_root.join("parser");
        let cli = config.projects_root.join("cli");
        std::fs::create_dir_all(&parser).unwrap();
        std::fs::create_dir_all(&cli).unwrap();
        save_vm(&config, "rust", Some("parser"));
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry
            .upsert(external_entry("parser", &parser, Some("rust")))
            .unwrap();
        registry
            .upsert(external_entry("cli", &cli, Some("rust")))
            .unwrap();

        delete_project(&config, &parser, true).unwrap();
        assert_eq!(load_primary(&config, "rust").as_deref(), Some("cli"));
    }

    #[test]
    fn deleting_the_last_primary_clears_the_vm_primary() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let parser = config.projects_root.join("parser");
        std::fs::create_dir_all(&parser).unwrap();
        save_vm(&config, "rust", Some("parser"));
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(external_entry("parser", &parser, Some("rust")))
            .unwrap();

        delete_project(&config, &parser, true).unwrap();
        assert!(load_primary(&config, "rust").is_none());
    }

    #[test]
    fn a_failed_removal_restores_the_old_primary() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let parser = config.projects_root.join("parser");
        let worktree = parser.join(".hegn/worktrees/experiment");
        std::fs::create_dir_all(&worktree).unwrap();
        save_vm(&config, "rust", Some("parser"));
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry
            .upsert(external_entry("parser", &parser, Some("rust")))
            .unwrap();
        let mut child = external_entry("experiment", &worktree, Some("rust"));
        child.kind = ProjectKind::Worktree;
        child.parent = Some("parser".into());
        registry.upsert(child).unwrap();

        let err = delete_project(&config, &parser, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("worktrees still name it"), "{err}");
        assert_eq!(load_primary(&config, "rust").as_deref(), Some("parser"));
        assert!(parser.is_dir());
    }

    #[test]
    fn delete_project_refuses_paths_outside_the_projects_root() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&config.projects_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "keep").unwrap();

        let err = delete_project(&config, &outside, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not directly inside"), "{err}");
        assert_eq!(
            std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
            "keep"
        );
    }
}
