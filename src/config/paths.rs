//! XDG locations and the private runtime directories exposed by [`Config`].

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::Config;

impl Config {
    /// Path of the user configuration file.
    pub fn config_path() -> PathBuf {
        if let Some(path) = std::env::var_os("HEGN_CONFIG") {
            return PathBuf::from(path);
        }
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        dirs::config_dir()
            .unwrap_or_else(|| home.join(".config"))
            .join("hegn/config.toml")
    }

    /// Runtime directory for API and virtiofs sockets.
    pub fn run_dir(&self) -> PathBuf {
        self.state_dir.join("run")
    }

    /// Private directory for persistent serial-console logs.
    ///
    /// Derived from `state_dir` so existing configurations keep working and
    /// logs always live next to the run directory they belong to.
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }

    /// Paths that must never be exported to a guest or imported as a project.
    ///
    /// This covers runtime sockets/logs, snapshots, templates and the config
    /// directory. Project import and discovery both consult this list.
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        let mut protected = vec![
            self.state_dir.clone(),
            self.snapshots_dir.clone(),
            self.instances_dir.clone(),
            self.templates_dir.clone(),
            self.profiles_dir.clone(),
        ];
        if let Some(parent) = self.instances_dir.parent() {
            let legacy = parent.join("sandboxes");
            if legacy != self.instances_dir && legacy.exists() {
                protected.push(legacy);
            }
        }
        if let Some(parent) = Self::config_path().parent() {
            protected.push(parent.to_path_buf());
        }
        protected
    }

    /// Import an existing directory into the project picker as a symlink.
    ///
    /// hegn state, snapshots and templates are protected so an imported
    /// project can never expose VM sockets, logs or snapshot memory to the
    /// guest.
    pub fn import_project(&self, source: &Path, name: Option<&str>) -> Result<PathBuf> {
        crate::project::import_project(&self.projects_root, source, name, &self.protected_paths())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::env::{EnvVarGuard, HEGN_ENV_VARS, env_lock};

    fn isolated_config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.projects_root = dir.join("projects");
        config.state_dir = dir.join("state");
        config.snapshots_dir = dir.join("snapshots");
        config.instances_dir = dir.join("instances");
        config.templates_dir = dir.join("templates");
        config.profiles_dir = dir.join("profiles");
        config
    }

    #[test]
    fn config_path_honors_the_hegn_config_override() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set(
            HEGN_ENV_VARS,
            &[("HEGN_CONFIG", "/tmp/hegn-test/custom.toml")],
        );
        assert_eq!(
            Config::config_path(),
            PathBuf::from("/tmp/hegn-test/custom.toml")
        );
    }

    #[test]
    fn config_path_defaults_to_the_hegn_directory() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set(HEGN_ENV_VARS, &[]);

        let path = Config::config_path();
        assert!(path.ends_with("hegn/config.toml"), "{}", path.display());
        assert_eq!(
            path.parent().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("hegn"))
        );
        if let Some(config_dir) = dirs::config_dir() {
            assert_eq!(path, config_dir.join("hegn/config.toml"));
        }
    }

    #[test]
    fn run_and_log_directories_are_derived_from_the_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        assert_eq!(config.run_dir(), config.state_dir.join("run"));
        assert_eq!(config.logs_dir(), config.state_dir.join("logs"));
    }

    #[test]
    fn protected_paths_cover_state_and_legacy_sandbox_directories() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let config_file = dir.path().join("config/config.toml");
        let config_file_str = config_file.to_str().unwrap().to_string();

        let _lock = env_lock();
        let _guard = EnvVarGuard::set(HEGN_ENV_VARS, &[("HEGN_CONFIG", config_file_str.as_str())]);

        let protected = config.protected_paths();
        for path in [
            &config.state_dir,
            &config.snapshots_dir,
            &config.instances_dir,
            &config.templates_dir,
            &config.profiles_dir,
        ] {
            assert!(
                protected.contains(path),
                "{} is not protected",
                path.display()
            );
        }
        assert!(
            protected.contains(&dir.path().join("config")),
            "the config directory must be protected"
        );
        assert!(
            !protected.contains(&dir.path().join("sandboxes")),
            "a missing legacy directory is not reported"
        );

        std::fs::create_dir(dir.path().join("sandboxes")).unwrap();
        let protected = config.protected_paths();
        assert!(
            protected.contains(&dir.path().join("sandboxes")),
            "an existing legacy directory is protected"
        );
    }

    #[test]
    fn import_project_creates_a_symlink_and_rejects_protected_sources() {
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let config_file = dir.path().join("config/config.toml");
        let config_file_str = config_file.to_str().unwrap().to_string();

        let _lock = env_lock();
        let _guard = EnvVarGuard::set(HEGN_ENV_VARS, &[("HEGN_CONFIG", config_file_str.as_str())]);

        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let link = config.import_project(&source, Some("demo")).unwrap();
        assert_eq!(link.canonicalize().unwrap(), source.canonicalize().unwrap());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(link.file_name().unwrap(), "demo");

        std::fs::create_dir_all(&config.state_dir).unwrap();
        let err = config
            .import_project(&config.state_dir, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("overlaps hegn state"), "{err}");
    }
}
