//! Importing existing directories into the picker as symlinks, with safety
//! checks that prevent exposing the filesystem root, the home directory,
//! ancestors of the projects root or hegn state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::discovery::absolute_or_canonical;
use super::meta::validate_project_name;

/// Expand a leading `~` in a path typed into the TUI or a config file.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    let raw = path.to_string_lossy();
    if raw == "~" {
        home
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        path.to_path_buf()
    }
}

/// Import an existing directory into the project picker without moving it.
///
/// The import is a symlink inside `root`; the project keeps living at its
/// original path. This is deliberately the only host-side mutation, so a large
/// or already-versioned project is never copied or rewritten.
///
/// Safety checks reject the filesystem root, the home directory, anything
/// inside `root` (it is already discoverable), any ancestor of `root`, and any
/// path that overlaps hegn's own state (`protected`).
pub fn import_project(
    root: &Path,
    source: &Path,
    name: Option<&str>,
    protected: &[PathBuf],
) -> Result<PathBuf> {
    let source = expand_tilde(source);
    if !source.exists() {
        bail!("{} does not exist", source.display());
    }
    let source = source
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", source.display()))?;
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }
    if source == Path::new("/") {
        bail!("refusing to import the filesystem root");
    }
    if let Some(home) = dirs::home_dir().and_then(|home| home.canonicalize().ok())
        && source == home
    {
        bail!("refusing to import the home directory");
    }

    fs::create_dir_all(root).with_context(|| format!("failed to create {}", root.display()))?;
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", root.display()))?;
    if source == root {
        bail!("refusing to import the projects root itself");
    }
    if root.starts_with(&source) {
        bail!(
            "{} is an ancestor of the projects root; import a single project instead",
            source.display()
        );
    }
    if source.starts_with(&root) {
        bail!(
            "{} is already inside the projects root; no import needed",
            source.display()
        );
    }
    for protected in protected {
        let protected = absolute_or_canonical(protected);
        if source.starts_with(&protected) || protected.starts_with(&source) {
            bail!(
                "refusing to import {}: it overlaps hegn state at {}",
                source.display(),
                protected.display()
            );
        }
    }

    let name = match name {
        Some(name) => name.trim().to_string(),
        None => source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .with_context(|| {
                format!(
                    "{} has no usable directory name; pass --name",
                    source.display()
                )
            })?,
    };
    let name = validate_project_name(&name)?.to_string();
    let link = root.join(&name);
    match fs::symlink_metadata(&link) {
        Ok(_) => bail!("{} already exists", link.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", link.display()));
        }
    }
    std::os::unix::fs::symlink(&source, &link)
        .with_context(|| format!("failed to create {}", link.display()))?;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::discover;

    #[test]
    fn imports_an_existing_directory_without_moving_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let source = dir.path().join("elsewhere/demo");
        std::fs::create_dir_all(&source).unwrap();

        let link = import_project(&root, &source, None, &[]).unwrap();
        assert_eq!(link, root.canonicalize().unwrap().join("demo"));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(link.canonicalize().unwrap(), source.canonicalize().unwrap());
        assert!(source.is_dir(), "the original directory must stay in place");

        let projects = discover(&root).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "demo");

        let err = import_project(&root, &source, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn import_rejects_roots_ancestors_and_hegn_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();

        assert!(import_project(&root, Path::new("/"), None, &[]).is_err());
        assert!(import_project(&root, dir.path(), None, &[]).is_err());
        assert!(import_project(&root, &root, None, &[]).is_err());
        assert!(import_project(&root, &state, None, std::slice::from_ref(&state)).is_err());
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let err = import_project(&root, &root.join("nested"), None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already inside"), "{err}");
        let err = import_project(&root, &source, Some("../escape"), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("separator"), "{err}");
    }

    #[test]
    fn expands_a_leading_tilde() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde(Path::new("~/demo")), home.join("demo"));
            assert_eq!(expand_tilde(Path::new("~")), home);
        }
        assert_eq!(
            expand_tilde(Path::new("relative")),
            PathBuf::from("relative")
        );
    }
}
