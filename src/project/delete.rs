//! Project removal: instance cleanup, deleting a real directory or unlinking an
//! imported symlink, with paths always constrained to the projects root.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::model::ProjectDeleteOutcome;

/// How a picker entry was removed.
enum EntryRemoval {
    /// A real project directory was deleted.
    Deleted,
    /// An import symlink was removed; the target directory is untouched.
    RemovedLink,
    /// A real project directory remains on disk and will be hidden.
    Kept,
}

/// Remove a project from the picker.
///
/// Every instance belonging to the project is stopped and deleted. When
/// `delete_files` is true the project directory is removed; an imported symlink
/// only loses the link and its target always stays. When files are kept for a
/// real directory, the entry is recorded as hidden so the next scan does not
/// re-add it.
pub fn delete_project(
    config: &crate::config::Config,
    entry: &Path,
    delete_files: bool,
) -> Result<ProjectDeleteOutcome> {
    // Validate containment before any destructive work. A caller passing an
    // outside/nested path must not lose that project's instances first.
    direct_child_root(&config.projects_root, entry)?;

    let registry = crate::vm::instance::InstanceRegistry::new(config.clone());
    let instances: Vec<String> = registry
        .summaries()?
        .into_iter()
        .filter(|instance| {
            crate::vm::instance::same_project(&instance.project, entry)
                || crate::vm::instance::same_project(&instance.workspace, entry)
        })
        .map(|instance| instance.name)
        .collect();
    for name in &instances {
        registry.delete(name, true)?;
    }

    let removal = remove_entry(&config.projects_root, entry, delete_files)?;
    let kept = matches!(removal, EntryRemoval::Kept);
    let files_deleted = matches!(removal, EntryRemoval::Deleted);
    let mut store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
    store.forget_project(entry)?;
    store.set_project_hidden(entry, kept)?;
    Ok(ProjectDeleteOutcome {
        files_deleted,
        instances,
    })
}

/// Resolve `root` and verify that `entry` is a direct child of it.
///
/// This is a pure containment check with no filesystem mutation, so callers
/// can run it before stopping or deleting any related instance.
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
        config.instances_dir = dir.join("instances");
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
    fn delete_project_removes_matching_instances_and_leaves_unrelated_ones() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let project = config.projects_root.join("demo");
        let other = config.projects_root.join("other");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let instances = crate::vm::instance_config::InstanceStore::new(config.clone());
        instances
            .create_from_project(&project, Some("demo-vm"), None, false)
            .unwrap();
        instances
            .create_from_project(&other, Some("other-vm"), None, false)
            .unwrap();
        let instance_dir = config.instances_dir.join("demo-vm");
        assert!(instance_dir.is_dir());

        let outcome = delete_project(&config, &project, true).unwrap();
        assert_eq!(outcome.instances, vec!["demo-vm".to_string()]);
        assert!(outcome.files_deleted);
        assert!(!instance_dir.exists());
        assert!(!project.exists());
        assert!(config.instances_dir.join("other-vm").is_dir());
        assert!(other.is_dir());
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

    #[test]
    fn delete_project_refuses_paths_outside_the_projects_root() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&config.projects_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "keep").unwrap();
        let instances = crate::vm::instance_config::InstanceStore::new(config.clone());
        instances
            .create_from_project(&outside, Some("outside-vm"), None, false)
            .unwrap();
        let instance_dir = config.instances_dir.join("outside-vm");
        assert!(instance_dir.is_dir());

        let err = delete_project(&config, &outside, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not directly inside"), "{err}");
        assert_eq!(
            std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
            "keep"
        );
        assert!(
            instance_dir.is_dir(),
            "an invalid entry must not destroy its instances first"
        );
    }
}
