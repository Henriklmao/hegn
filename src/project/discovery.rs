//! Project discovery: scan the projects root and skip unsafe entries.
//!
//! Symlinked entries are resolved before they are accepted so a planted link
//! can never widen the virtio-fs export to the filesystem root, the home
//! directory, an ancestor of the projects root or hegn's own state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::model::Project;

/// Directories that are never treated as projects.
const SKIP_DIRS: &[&str] = &["target", "node_modules", ".git", ".cache", "venv", ".venv"];

/// Read a project list from `root`. Non-directories and noisy build folders are
/// skipped. Git is queried for each project; this may block and should run off
/// the UI thread.
pub fn discover(root: &Path) -> io::Result<Vec<Project>> {
    discover_protected(root, &[])
}

/// Like [`discover`], but also skips any project that overlaps hegn state.
///
/// Callers with a [`crate::config::Config`] should pass
/// `config.protected_paths()` so a manually planted symlink to the run
/// directory, logs or snapshots can never be opened as a project.
pub fn discover_protected(root: &Path, protected: &[PathBuf]) -> io::Result<Vec<Project>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let home = dirs::home_dir().and_then(|home| home.canonicalize().ok());
    let protected: Vec<PathBuf> = protected
        .iter()
        .map(|path| absolute_or_canonical(path))
        .collect();
    let mut projects = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        // Resolve imports (symlinks) but never let a link widen the picker to
        // the filesystem root, the home directory, an ancestor of the projects
        // root, or hegn state. Opening such a "project" would export far too
        // much to the guest through virtiofsd.
        let canonical = match path.canonicalize() {
            Ok(canonical) if canonical.is_dir() => canonical,
            _ => continue,
        };
        if canonical == Path::new("/")
            || home.as_ref().is_some_and(|home| &canonical == home)
            || root_canonical.starts_with(&canonical)
            || protected
                .iter()
                .any(|path| canonical.starts_with(path) || path.starts_with(&canonical))
        {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        projects.push(Project::load(path));
    }
    projects.sort_by_key(|project| project.name.to_lowercase());
    Ok(projects)
}

/// Resolve `path`, falling back to an absolute form when it does not exist.
pub(super) fn absolute_or_canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_only_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("one")).unwrap();
        std::fs::create_dir(root.path().join(".hidden")).unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("file.txt"), "x").unwrap();
        let projects = discover(root.path()).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "one");
    }

    #[test]
    fn discover_skips_links_to_dangerous_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::os::unix::fs::symlink("/", root.join("rootfs")).unwrap();
        std::os::unix::fs::symlink(&state, root.join("state-link")).unwrap();
        assert!(
            discover_protected(&root, std::slice::from_ref(&state))
                .unwrap()
                .is_empty()
        );
    }
}
