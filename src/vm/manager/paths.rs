//! Runtime paths for one VM's sockets, pid files and metadata.
//!
//! All paths live under `<vms_dir>/<name>/run/`, which is created with
//! private (`0700`) permissions. Every exported project gets an indexed
//! virtiofsd socket and pid file; the profile share keeps its fixed socket.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::vm::config::VmConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPaths {
    pub run_dir: PathBuf,
    /// Cloud Hypervisor REST API.
    pub api_socket: PathBuf,
    /// Guest serial socket created by Cloud Hypervisor.
    pub serial_socket: PathBuf,
    /// Relay socket consumed by the TUI and `hegn attach`.
    pub console_socket: PathBuf,
    /// Second virtiofsd socket for the per-VM profile/skill copy.
    pub profile_socket: PathBuf,
    pub vm_pid: PathBuf,
    pub relay_pid: PathBuf,
    pub profile_virtiofs_pid: PathBuf,
    /// Detached periodic-snapshot worker.
    pub autosave_pid: PathBuf,
    pub session: PathBuf,
    /// Membership manifest written at boot (`membership.json`).
    pub membership: PathBuf,
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

impl VmPaths {
    pub fn from_vm(config: &Config, vm: &VmConfig) -> Self {
        let run_dir = vm.dir(&config.vms_dir).join("run");
        Self {
            api_socket: run_dir.join("cloud-hypervisor.sock"),
            serial_socket: run_dir.join("serial.sock"),
            console_socket: run_dir.join("console.sock"),
            profile_socket: run_dir.join("profile-virtiofs.sock"),
            vm_pid: run_dir.join("cloud-hypervisor.pid"),
            relay_pid: run_dir.join("console-relay.pid"),
            profile_virtiofs_pid: run_dir.join("profile-virtiofs.pid"),
            autosave_pid: run_dir.join("autosave.pid"),
            session: run_dir.join("session.json"),
            membership: run_dir.join("membership.json"),
            log: run_dir.join("cloud-hypervisor.log"),
            relay_log: run_dir.join("console-relay.log"),
            autosave_log: run_dir.join("autosave.log"),
            event_log: run_dir.join("hegn.log"),
            lock: run_dir.join("hegn.lock"),
            cloud_init: run_dir.join("cloud-init.img"),
            run_dir,
        }
    }

    /// Socket for the member at `index` in the boot plan.
    pub fn virtiofs_socket(&self, index: usize) -> PathBuf {
        self.run_dir.join(format!("virtiofs-{index}.sock"))
    }

    /// Pid file for the member at `index` in the boot plan.
    pub fn virtiofs_pid(&self, index: usize) -> PathBuf {
        self.run_dir.join(format!("virtiofs-{index}.pid"))
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
    use crate::vm::config::VmConfig;

    fn config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.vms_dir = dir.join("vms");
        config
    }

    fn vm(name: &str) -> VmConfig {
        VmConfig {
            name: name.into(),
            ..VmConfig::default()
        }
    }

    #[test]
    fn paths_live_under_vm_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let paths = VmPaths::from_vm(&config, &vm("demo"));
        assert_eq!(paths.run_dir, config.vms_dir.join("demo").join("run"));
        assert_eq!(
            paths.api_socket.file_name().unwrap(),
            "cloud-hypervisor.sock"
        );
        assert_eq!(paths.serial_socket.file_name().unwrap(), "serial.sock");
        assert_eq!(paths.console_socket.file_name().unwrap(), "console.sock");
        assert_eq!(paths.membership.file_name().unwrap(), "membership.json");
        assert_eq!(
            paths.virtiofs_socket(2).file_name().unwrap(),
            "virtiofs-2.sock"
        );
        assert_eq!(paths.virtiofs_pid(2).file_name().unwrap(), "virtiofs-2.pid");
    }

    #[test]
    fn run_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = VmPaths::from_vm(&config(dir.path()), &vm("demo"));
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        std::fs::set_permissions(&paths.run_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        paths.ensure().unwrap();
        let mode = std::fs::metadata(&paths.run_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "an existing run directory must be tightened");
    }

    #[test]
    fn run_directory_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let paths = VmPaths::from_vm(&config(dir.path()), &vm("demo"));
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
        let paths = VmPaths::from_vm(&config(dir.path()), &vm("demo"));
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
    fn every_runtime_path_stays_inside_the_vm_run_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let paths = VmPaths::from_vm(&config, &vm("demo"));
        let runtime = [
            &paths.api_socket,
            &paths.serial_socket,
            &paths.console_socket,
            &paths.profile_socket,
            &paths.vm_pid,
            &paths.relay_pid,
            &paths.profile_virtiofs_pid,
            &paths.autosave_pid,
            &paths.session,
            &paths.membership,
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
        assert!(paths.run_dir.starts_with(config.vms_dir.join("demo")));
    }
}
