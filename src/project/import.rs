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

    #[test]
    fn expand_tilde_only_handles_a_leading_home_shorthand() {
        if let Some(home) = dirs::home_dir() {
            // `~/` joins an empty rest, which is still the home directory.
            assert_eq!(expand_tilde(Path::new("~/")), home);
        }
        for untouched in ["~user/demo", "a/~/b", "/tmp/~", "~~"] {
            assert_eq!(expand_tilde(Path::new(untouched)), PathBuf::from(untouched));
        }
    }

    #[test]
    fn import_rejects_missing_and_non_directory_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "not a project").unwrap();

        let err = import_project(&root, &dir.path().join("missing"), None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        let err = import_project(&root, &file, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a directory"), "{err}");
        assert!(
            !root.exists(),
            "a rejected import must not create the projects root"
        );
    }

    #[test]
    fn import_rejects_home_even_when_reached_through_a_symlink() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        if !home.is_dir() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");

        let err = import_project(&root, &home, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("home directory"), "{err}");

        let link = dir.path().join("home-link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let err = import_project(&root, &link, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("home directory"), "{err}");
        assert!(
            !root.exists(),
            "rejection must happen before any link exists"
        );
    }

    #[test]
    fn import_rejects_the_projects_root_and_its_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();

        let err = import_project(&root, &root, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("projects root itself"), "{err}");

        let err = import_project(&root, dir.path(), None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ancestor of the projects root"), "{err}");

        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let err = import_project(&root, &nested, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already inside the projects root"), "{err}");

        let entries: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["nested"], "no import link may be created");
    }

    #[test]
    fn import_rejects_sources_overlapping_protected_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let protected = [
            ("state", dir.path().join("state")),
            ("snapshots", dir.path().join("snapshots")),
            ("templates", dir.path().join("templates")),
            ("config", dir.path().join("config/hegn")),
        ];

        for (label, path) in &protected {
            std::fs::create_dir_all(path.join("inner")).unwrap();
            // A child of protected state would expose sockets/logs to the guest.
            let err = import_project(&root, &path.join("inner"), None, std::slice::from_ref(path))
                .unwrap_err()
                .to_string();
            assert!(err.contains("overlaps hegn state"), "{label} child: {err}");
            // The protected directory itself is refused as well.
            let err = import_project(&root, path, None, std::slice::from_ref(path))
                .unwrap_err()
                .to_string();
            assert!(err.contains("overlaps hegn state"), "{label}: {err}");
        }

        // A source that is an ancestor of protected state is refused too.
        let state = dir.path().join("state");
        let deep = state.join("secrets");
        std::fs::create_dir_all(&deep).unwrap();
        let err = import_project(&root, &state, None, std::slice::from_ref(&deep))
            .unwrap_err()
            .to_string();
        assert!(err.contains("overlaps hegn state"), "{err}");
    }

    #[test]
    fn import_refuses_to_overwrite_existing_entries_and_broken_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let source = dir.path().join("source/demo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&root).unwrap();

        let existing = root.join("demo");
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &existing).unwrap();
        let err = import_project(&root, &source, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(
            std::fs::symlink_metadata(&existing)
                .unwrap()
                .file_type()
                .is_symlink(),
            "an existing link must not be replaced"
        );

        let broken = root.join("broken");
        std::os::unix::fs::symlink(dir.path().join("missing-target"), &broken).unwrap();
        let err = import_project(&root, &source, Some("broken"), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(
            std::fs::symlink_metadata(&broken)
                .unwrap()
                .file_type()
                .is_symlink(),
            "a broken link must be refused instead of followed"
        );
        assert!(!dir.path().join("missing-target").exists());

        let occupied = root.join("occupied");
        std::fs::write(&occupied, "keep me").unwrap();
        let err = import_project(&root, &source, Some("occupied"), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&occupied).unwrap(), "keep me");
    }

    #[test]
    fn import_with_an_explicit_name_creates_only_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let source = dir.path().join("work/repo");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("README.md"), "original").unwrap();

        let link = import_project(&root, &source, Some("alias"), &[]).unwrap();
        assert_eq!(link, root.canonicalize().unwrap().join("alias"));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(link.canonicalize().unwrap(), source.canonicalize().unwrap());

        let entries: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["alias"], "import must not copy project files");
        assert!(source.join("nested").is_dir());
        assert_eq!(
            std::fs::read_to_string(source.join("README.md")).unwrap(),
            "original"
        );
    }

    #[test]
    fn import_resolves_symlink_sources_before_exposing_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();

        let to_root = dir.path().join("link-root");
        std::os::unix::fs::symlink(&root, &to_root).unwrap();
        let err = import_project(&root, &to_root, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("projects root itself"), "{err}");

        let to_ancestor = dir.path().join("link-ancestor");
        std::os::unix::fs::symlink(dir.path(), &to_ancestor).unwrap();
        let err = import_project(&root, &to_ancestor, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ancestor of the projects root"), "{err}");

        // A symlink to an ordinary project is imported as its real target.
        let target = dir.path().join("work/repo");
        std::fs::create_dir_all(&target).unwrap();
        let to_project = dir.path().join("link-repo");
        std::os::unix::fs::symlink(&target, &to_project).unwrap();
        let link = import_project(&root, &to_project, Some("via-link"), &[]).unwrap();
        assert_eq!(link, root.canonicalize().unwrap().join("via-link"));
        assert_eq!(link.canonicalize().unwrap(), target.canonicalize().unwrap());
    }

    #[test]
    fn import_rejects_names_that_escape_or_start_with_a_dot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();

        for name in [
            "",
            "   ",
            ".hidden",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            "bad;name",
        ] {
            assert!(
                import_project(&root, &source, Some(name), &[]).is_err(),
                "name {name:?} must be refused"
            );
        }
        assert!(
            std::fs::read_dir(&root).unwrap().next().is_none(),
            "refused names must not leave links behind"
        );
    }
}
