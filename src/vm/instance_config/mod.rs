//! Per-instance configuration and workspace cloning.
//!
//! Every instance lives in its own directory under `instances_dir`:
//!
//! ```text
//! <instances_dir>/<name>/
//!   hegn.toml     per-instance settings (private, 0600)
//!   workspace/    clone for multi-agent instances
//!   run/          runtime sockets/pids/lock (created on start)
//! ```
//!
//! A normal instance points `workspace` at the original project; a multi-agent
//! clone gets a private workspace inside the instance directory. Deleting an
//! instance never deletes a workspace outside its own directory.
//!
//! The module is split by concern: `model` holds the persisted configuration
//! and its validation, `store` the filesystem store, `clone` creation and
//! deletion, and `rootfs` the private per-instance disk preparation.

mod clone;
mod model;
mod rootfs;
mod store;

pub use clone::{CloneOutcome, DeleteOutcome};
pub use model::{CONFIG_FILE, InstanceConfig, WORKSPACE_DIR, validate_name};
pub use store::InstanceStore;

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::config::{Config, Harness};

    #[allow(clippy::field_reassign_with_default)]
    pub(crate) fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.instances_dir = dir.join("instances");
        config.projects_root = dir.join("projects");
        config.harness = Harness::Shell;
        config
    }

    pub(crate) fn project(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join("projects").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("README.md"), b"hello\n").unwrap();
        path
    }
}
