//! Project selector resolution shared by every handler that accepts a project
//! name or path.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::config::Config;

pub fn resolve_project(config: &Config, arg: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(arg);
    if direct.exists() {
        if !direct.is_dir() {
            bail!("{} is not a directory", direct.display());
        }
        return Ok(direct
            .canonicalize()
            .unwrap_or_else(|_| direct.to_path_buf()));
    }
    let candidate = config.projects_root.join(arg);
    if candidate.is_dir() {
        return Ok(candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.to_path_buf()));
    }
    bail!(
        "project '{arg}' not found; looked at {} and {}",
        direct.display(),
        candidate.display()
    )
}

pub fn resolve_project_entry(config: &Config, arg: &str) -> Result<PathBuf> {
    let root = config
        .projects_root
        .canonicalize()
        .unwrap_or_else(|_| config.projects_root.clone());
    for candidate in [config.projects_root.join(arg), PathBuf::from(arg)] {
        if std::fs::symlink_metadata(&candidate).is_err() {
            continue;
        }
        let parent = candidate
            .parent()
            .and_then(|parent| parent.canonicalize().ok());
        if parent.as_ref() == Some(&root) {
            return Ok(candidate);
        }
    }
    bail!(
        "project '{arg}' not found directly inside {}",
        config.projects_root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;

    #[test]
    fn resolves_project_by_name_and_path() {
        let (_dir, config) = config();
        let path = config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        assert_eq!(
            resolve_project(&config, "demo").unwrap(),
            path.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_project(&config, path.to_str().unwrap()).unwrap(),
            path.canonicalize().unwrap()
        );
        assert!(resolve_project(&config, "missing").is_err());
    }
}
