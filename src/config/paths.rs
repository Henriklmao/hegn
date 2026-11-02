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
