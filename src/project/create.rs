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
}
