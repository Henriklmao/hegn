//! Non-interactive CLI. Every TUI capability has a CLI counterpart so hegn can
//! be scripted in CI, used over SSH, or driven by an agent.
//!
//! The module is split along command boundaries: [`args`] holds the clap
//! surface, [`run`] dispatches, `commands` contains one handler module per
//! command family, and [`output`] / [`resolve`] hold the shared helpers.

mod args;
mod commands;
mod output;
mod resolve;
mod run;

pub use self::args::{
    Cli, Commands, ConfigCommands, ImageCommands, InstanceCommands, LogCommands, NetworkCommands,
    ProfileCommands, ProjectCommands, SkillCommands, SnapshotCommands,
};
pub use self::commands::doctor::check_command_for_test;
pub use self::resolve::{resolve_project, resolve_project_entry};
pub use self::run::{load_config, run};

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the CLI submodule tests.

    use crate::config::{BackendKind, Config};

    /// Config rooted in an isolated temporary directory.
    #[allow(clippy::field_reassign_with_default)]
    pub(crate) fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.projects_root = dir.path().join("projects");
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.instances_dir = dir.path().join("instances");
        config.templates_dir = dir.path().join("templates");
        config.profiles_dir = dir.path().join("profiles");
        config.backend = BackendKind::Auto;
        std::fs::create_dir_all(&config.projects_root).unwrap();
        (dir, config)
    }
}
