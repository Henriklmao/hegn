//! Runtime paths for one instance's sockets, pid files and metadata.
//!
//! All paths live under `<instances_dir>/<name>/run/`, which is created with
//! private (`0700`) permissions.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::vm::instance_config::InstanceConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePaths {
    pub run_dir: PathBuf,
    /// Cloud Hypervisor REST API.
    pub api_socket: PathBuf,
    /// Guest serial socket created by Cloud Hypervisor.
    pub serial_socket: PathBuf,
    /// Relay socket consumed by the TUI and `hegn attach`.
    pub console_socket: PathBuf,
    pub virtiofs_socket: PathBuf,
    /// Second virtiofsd socket for the per-instance profile/skill copy.
    pub profile_socket: PathBuf,
    pub vm_pid: PathBuf,
    pub relay_pid: PathBuf,
    pub virtiofs_pid: PathBuf,
    pub profile_virtiofs_pid: PathBuf,
    /// Detached periodic-snapshot worker.
    pub autosave_pid: PathBuf,
    pub session: PathBuf,
    pub log: PathBuf,
    /// Relay stderr. A detached relay has no terminal, so this file is the only
    /// place its startup error can be read from.
    pub relay_log: PathBuf,
    /// Automatic snapshot worker stderr, for diagnosing a failed checkpoint.
    pub autosave_log: PathBuf,
    /// hegn lifecycle/start errors, kept in the run directory for post-mortems.
    pub event_log: PathBuf,
    /// Exclusive lock serializing VM lifecycle operations.
    pub lock: PathBuf,
    /// Generated NoCloud seed disk.
    pub cloud_init: PathBuf,
}

impl InstancePaths {
    pub fn from_instance(config: &Config, instance: &InstanceConfig) -> Self {
        let run_dir = instance.dir(&config.instances_dir).join("run");
        Self {
            api_socket: run_dir.join("cloud-hypervisor.sock"),
            serial_socket: run_dir.join("serial.sock"),
            console_socket: run_dir.join("console.sock"),
            virtiofs_socket: run_dir.join("virtiofs.sock"),
            profile_socket: run_dir.join("profile-virtiofs.sock"),
            vm_pid: run_dir.join("cloud-hypervisor.pid"),
            relay_pid: run_dir.join("console-relay.pid"),
            virtiofs_pid: run_dir.join("virtiofs.pid"),
            profile_virtiofs_pid: run_dir.join("profile-virtiofs.pid"),
            autosave_pid: run_dir.join("autosave.pid"),
            session: run_dir.join("session.json"),
            log: run_dir.join("cloud-hypervisor.log"),
            relay_log: run_dir.join("console-relay.log"),
            autosave_log: run_dir.join("autosave.log"),
            event_log: run_dir.join("hegn.log"),
            lock: run_dir.join("hegn.lock"),
            cloud_init: run_dir.join("cloud-init.img"),
            run_dir,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        // The API, serial, relay and virtiofs sockets live here; a private
        // directory keeps other local users from driving the microVM.
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            // A symlinked run directory could redirect the VMM/console sockets
            // outside the private state tree. Refuse instead of following it.
            if let Ok(metadata) = fs::symlink_metadata(&self.run_dir)
                && metadata.file_type().is_symlink()
            {
                bail!(
                    "refusing symlinked run directory {}",
                    self.run_dir.display()
                );
            }
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(&self.run_dir)
                .with_context(|| format!("failed to create {}", self.run_dir.display()))?;
            // The directory may already exist with wider permissions. Sockets
            // for the VMM and the console relay live here, so tightening it is
            // mandatory, not best effort.
            fs::set_permissions(&self.run_dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", self.run_dir.display()))?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(&self.run_dir)
            .with_context(|| format!("failed to create {}", self.run_dir.display()))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::vm::instance::test_support::{config_with_assets, instance_for, manager};

    #[test]
    fn paths_live_under_instance_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance = instance_for(&dir.path().join("project"));
        let paths = InstancePaths::from_instance(&config, &instance);
        assert_eq!(paths.run_dir, config.instances_dir.join("demo").join("run"));
        assert_eq!(
            paths.api_socket.file_name().unwrap(),
            "cloud-hypervisor.sock"
        );
        assert_eq!(paths.serial_socket.file_name().unwrap(), "serial.sock");
        assert_eq!(paths.console_socket.file_name().unwrap(), "console.sock");
        assert_eq!(paths.virtiofs_socket.file_name().unwrap(), "virtiofs.sock");
    }

    #[test]
    fn run_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let run_dir = manager.paths().run_dir;
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        manager.paths().ensure().unwrap();
        let mode = std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "an existing run directory must be tightened");
    }

    #[test]
    fn run_directory_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(paths.run_dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &paths.run_dir).unwrap();

        let err = paths.ensure().unwrap_err().to_string();
        assert!(err.contains("symlinked run directory"), "{err}");
    }

    #[test]
    fn ensure_creates_a_fresh_run_directory_with_private_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        assert!(!paths.run_dir.exists());

        paths.ensure().unwrap();
        assert!(paths.run_dir.is_dir());
        assert_eq!(
            std::fs::metadata(&paths.run_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        // Repeated ensure calls keep the invariant.
        paths.ensure().unwrap();
        assert_eq!(
            std::fs::metadata(&paths.run_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn every_runtime_path_stays_inside_the_instance_run_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance = instance_for(&dir.path().join("project"));
        let paths = InstancePaths::from_instance(&config, &instance);
        let runtime = [
            &paths.api_socket,
            &paths.serial_socket,
            &paths.console_socket,
            &paths.virtiofs_socket,
            &paths.profile_socket,
            &paths.vm_pid,
            &paths.relay_pid,
            &paths.virtiofs_pid,
            &paths.profile_virtiofs_pid,
            &paths.autosave_pid,
            &paths.session,
            &paths.log,
            &paths.relay_log,
            &paths.autosave_log,
            &paths.event_log,
            &paths.lock,
            &paths.cloud_init,
        ];
        for path in runtime {
            assert_eq!(
                path.parent(),
                Some(paths.run_dir.as_path()),
                "{} escapes the run directory",
                path.display()
            );
        }
        assert!(paths.run_dir.starts_with(config.instances_dir.join("demo")));
    }
}
