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

    #[test]
    fn resolves_an_external_directory_but_rejects_files() {
        let (dir, config) = config();
        let external = dir.path().join("external-project");
        std::fs::create_dir_all(&external).unwrap();
        assert_eq!(
            resolve_project(&config, external.to_str().unwrap()).unwrap(),
            external.canonicalize().unwrap()
        );

        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();
        let err = resolve_project(&config, file.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn missing_project_reports_both_looked_up_locations() {
        let (_dir, config) = config();
        let err = resolve_project(&config, "nope").unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
        assert!(
            err.contains(&config.projects_root.display().to_string()),
            "{err}"
        );
        assert!(err.contains("looked at"), "{err}");
    }

    #[test]
    fn entry_resolution_stays_inside_the_projects_root() {
        let (dir, config) = config();
        std::fs::create_dir_all(config.projects_root.join("demo")).unwrap();
        std::fs::create_dir_all(config.projects_root.join("nested/deep")).unwrap();

        let resolved = resolve_project_entry(&config, "demo").unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            config.projects_root.join("demo").canonicalize().unwrap()
        );

        let absolute = config.projects_root.join("demo");
        assert_eq!(
            resolve_project_entry(&config, absolute.to_str().unwrap())
                .unwrap()
                .canonicalize()
                .unwrap(),
            absolute.canonicalize().unwrap()
        );

        assert!(
            resolve_project_entry(&config, "nested/deep").is_err(),
            "nested selectors are not root entries"
        );
        assert!(resolve_project_entry(&config, "missing").is_err());

        let external = dir.path().join("external");
        std::fs::create_dir_all(&external).unwrap();
        let imported = config.projects_root.join("imported");
        std::os::unix::fs::symlink(&external, &imported).unwrap();
        assert_eq!(
            resolve_project_entry(&config, "imported")
                .unwrap()
                .canonicalize()
                .unwrap(),
            external.canonicalize().unwrap()
        );
    }
}
