//! Safe removal of the throwaway projects created by `benchmark --clean`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::TEMP_PREFIX;
use crate::config::Config;

/// Remove a throwaway project, refusing anything that is not one of ours.
pub(super) fn remove_temp_project(config: &Config, path: &Path) -> Result<()> {
    let projects_root = config
        .projects_root
        .canonicalize()
        .unwrap_or_else(|_| config.projects_root.clone());
    let parent = path.parent().and_then(|dir| dir.canonicalize().ok());
    let in_projects_root = parent.as_deref() == Some(projects_root.as_path());
    let prefixed = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(TEMP_PREFIX));
    if !in_projects_root || !prefixed {
        bail!(
            "refusing to remove {}: not a benchmark project under {}",
            path.display(),
            config.projects_root.display()
        );
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!("refusing to remove {}: not a directory", path.display());
    }
    fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_temp_project_only_deletes_its_own_projects() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            projects_root: dir.path().join("projects"),
            ..Config::default()
        };
        let temp = config.projects_root.join("hegn-benchmark-1");
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("README.md"), b"x").unwrap();

        remove_temp_project(&config, &temp).unwrap();
        assert!(!temp.exists());

        let foreign = config.projects_root.join("my-project");
        fs::create_dir_all(&foreign).unwrap();
        let err = remove_temp_project(&config, &foreign).unwrap_err();
        assert!(err.to_string().contains("refusing to remove"), "{err}");
        assert!(foreign.exists());

        let outside = dir.path().join("hegn-benchmark-outside");
        fs::create_dir_all(&outside).unwrap();
        let err = remove_temp_project(&config, &outside).unwrap_err();
        assert!(err.to_string().contains("refusing to remove"), "{err}");
        assert!(outside.exists());
    }
}
