//! Project creation: validated directory name plus seeded agent instructions,
//! README and git repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::meta::validate_project_name;

/// Create a project directory with a seeded `AGENTS.md`, README and git repo.
///
/// Returns the created project path. Refuses to overwrite an existing directory.
pub fn create_project(
    root: &Path,
    name: &str,
    template: Option<&Path>,
    harness: crate::config::Harness,
) -> Result<PathBuf> {
    let name = validate_project_name(name)?;
    let path = root.join(name);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;

    let agents = template_agents(template)?;
    fs::write(path.join("AGENTS.md"), agents)?;
    fs::write(
        path.join("README.md"),
        format!(
            "# {name}\n\nCreated with hegn. Harness: {}.\n",
            harness.label()
        ),
    )?;

    let git = Command::new("git")
        .arg("-C")
        .arg(&path)
        .arg("init")
        .arg("--quiet")
        .output();
    match git {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            // Not fatal: the project is still usable without git.
            eprintln!(
                "warning: git init failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(err) => eprintln!("warning: git is not installed: {err}"),
    }
    Ok(path)
}

/// Seed content for `AGENTS.md`: an explicit template file or the built-in one.
fn template_agents(template: Option<&Path>) -> Result<String> {
    match template {
        Some(template) if template.is_file() => fs::read_to_string(template)
            .with_context(|| format!("failed to read template {}", template.display())),
        _ => Ok(crate::config::DEFAULT_AGENTS_MD.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_project_skeleton_and_git_repo() {
        let root = tempfile::tempdir().unwrap();
        let path =
            create_project(root.path(), "demo", None, crate::config::Harness::Opencode).unwrap();
        assert!(path.join("AGENTS.md").is_file());
        assert!(path.join("README.md").is_file());
        assert!(path.join(".git").exists());
        assert!(create_project(root.path(), "demo", None, crate::config::Harness::Shell).is_err());
    }

    #[test]
    fn create_project_rejects_unsafe_names_without_creating_anything() {
        let root = tempfile::tempdir().unwrap();
        let mut bad = vec![
            "".to_string(),
            "   ".to_string(),
            "..".to_string(),
            "../escape".to_string(),
            "a/b".to_string(),
            "a\\b".to_string(),
            ".hidden".to_string(),
            "bad;name".to_string(),
        ];
        bad.push("a".repeat(81));

        for name in &bad {
            assert!(
                create_project(root.path(), name, None, crate::config::Harness::Shell).is_err(),
                "name {name:?} must be refused"
            );
        }
        assert!(
            std::fs::read_dir(root.path()).unwrap().next().is_none(),
            "invalid names must not create directories"
        );
    }

    #[test]
    fn create_project_refuses_existing_directories_files_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        let existing = root.path().join("demo");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("README.md"), "keep me").unwrap();
        let err = create_project(root.path(), "demo", None, crate::config::Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(
            std::fs::read_to_string(existing.join("README.md")).unwrap(),
            "keep me"
        );

        std::fs::write(root.path().join("plain-file"), "keep").unwrap();
        assert!(
            create_project(
                root.path(),
                "plain-file",
                None,
                crate::config::Harness::Shell
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("plain-file")).unwrap(),
            "keep"
        );

        let link = root.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        assert!(
            create_project(root.path(), "linked", None, crate::config::Harness::Shell).is_err()
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "an existing symlink must be refused, not followed"
        );

        let broken = root.path().join("broken");
        let missing = root.path().join("missing-target");
        std::os::unix::fs::symlink(&missing, &broken).unwrap();
        assert!(
            create_project(root.path(), "broken", None, crate::config::Harness::Shell).is_err()
        );
        assert!(
            std::fs::symlink_metadata(&broken)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!missing.exists());
    }

    #[test]
    fn create_project_uses_a_template_or_the_builtin_default_agents_file() {
        let root = tempfile::tempdir().unwrap();
        let template = root.path().join("custom.md");
        std::fs::write(&template, "# Custom instructions\nBe careful.\n").unwrap();

        let path = create_project(
            root.path(),
            "templated",
            Some(&template),
            crate::config::Harness::Codex,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("AGENTS.md")).unwrap(),
            "# Custom instructions\nBe careful.\n"
        );
        let readme = std::fs::read_to_string(path.join("README.md")).unwrap();
        assert!(readme.contains("# templated"), "{readme}");
        assert!(readme.contains("Codex"), "{readme}");

        // A missing template silently falls back to the built-in instructions.
        let missing = root.path().join("missing.md");
        let path = create_project(
            root.path(),
            "defaulted",
            Some(&missing),
            crate::config::Harness::Shell,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("AGENTS.md")).unwrap(),
            crate::config::DEFAULT_AGENTS_MD
        );
    }
}
