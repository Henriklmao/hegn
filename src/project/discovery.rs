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

    fn names(root: &Path) -> Vec<String> {
        discover(root)
            .unwrap()
            .into_iter()
            .map(|project| project.name)
            .collect()
    }

    #[test]
    fn discover_sorts_direct_children_and_skips_noise_and_nested_projects() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "zeta",
            "Alpha",
            "beta",
            "target",
            "node_modules",
            ".git",
            ".hidden",
            "venv",
            ".venv",
            ".cache",
        ] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        std::fs::write(root.path().join("notes.txt"), "not a project").unwrap();
        // Only direct children are projects; nested repositories are not.
        std::fs::create_dir_all(root.path().join("zeta/nested-project")).unwrap();

        let projects = discover(root.path()).unwrap();
        let discovered: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(discovered, ["Alpha", "beta", "zeta"]);
        assert!(
            projects
                .iter()
                .all(|project| project.path.parent() == Some(root.path()))
        );
    }

    #[test]
    fn discover_skips_links_to_root_home_and_project_root_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let open = dir.path().join("outside/open");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&open).unwrap();

        std::os::unix::fs::symlink("/", root.join("rootfs")).unwrap();
        std::os::unix::fs::symlink(dir.path(), root.join("ancestor")).unwrap();
        std::os::unix::fs::symlink(&open, root.join("open")).unwrap();
        if let Some(home) = dirs::home_dir() {
            std::os::unix::fs::symlink(home, root.join("home")).unwrap();
        }

        assert_eq!(names(&root), ["open"]);
    }

    #[test]
    fn discover_skips_broken_links_and_accepts_valid_imports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let external = dir.path().join("outside/repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(root.join("file.txt"), "plain file").unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), root.join("broken")).unwrap();
        std::os::unix::fs::symlink(&external, root.join("linked")).unwrap();

        let projects = discover(&root).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "linked");
        // The picker keeps the import path so deletion removes only the link.
        assert_eq!(projects[0].path, root.join("linked"));
        assert_eq!(
            projects[0].path.canonicalize().unwrap(),
            external.canonicalize().unwrap()
        );
    }

    #[test]
    fn discover_protected_skips_protected_directories_and_their_relatives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let state = dir.path().join("state");
        let deep = state.join("nested/deeper");
        let open = dir.path().join("outside/open");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(&open).unwrap();
        std::fs::create_dir(root.join("secret-real")).unwrap();
        std::fs::create_dir(root.join("open-real")).unwrap();

        std::os::unix::fs::symlink(&deep, root.join("state-child")).unwrap();
        std::os::unix::fs::symlink(&state, root.join("state-parent")).unwrap();
        std::os::unix::fs::symlink(&open, root.join("open-link")).unwrap();

        let protected = [deep, root.join("secret-real")];
        let projects = discover_protected(&root, &protected).unwrap();
        let discovered: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(discovered, ["open-link", "open-real"]);
    }

    #[test]
    fn discover_of_a_missing_root_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(&dir.path().join("missing")).unwrap().is_empty());
    }
}
