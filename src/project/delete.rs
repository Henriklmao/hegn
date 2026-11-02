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

/// Remove a picker entry from `root`.
///
/// Imported projects are symlinks: removing the link never touches the imported
/// directory. Real directories are deleted only when `delete_files` is true and
/// are always constrained to a direct child of `root`.
fn remove_entry(root: &Path, entry: &Path, delete_files: bool) -> Result<EntryRemoval> {
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
}
