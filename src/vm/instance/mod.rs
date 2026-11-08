//! Cloud Hypervisor lifecycle orchestration.
//!
//! One hegn state directory manages one VM at a time:
//!
//! ```text
//! <state>/run/
//!   cloud-hypervisor.sock   REST API socket
//!   serial.sock             guest serial socket (Cloud Hypervisor listener)
//!   console.sock            relay socket used by `hegn attach`
//!   virtiofs.sock           virtiofsd vhost-user socket
//!   profile-virtiofs.sock   second virtiofsd socket for the profile copy
//!   cloud-hypervisor.pid
//!   console-relay.pid
//!   virtiofs.pid
//!   profile-virtiofs.pid
//!   autosave.pid            detached periodic-snapshot worker
//!   session.json
//!   cloud-hypervisor.log    VMM log
//!   console-relay.log       relay stderr (diagnostics only)
//!   autosave.log            autosnapshot worker stderr (diagnostics only)
//!   hegn.log                lifecycle/start diagnostics
//!   hegn.lock               lifecycle flock
//!   cloud-init.img          NoCloud seed for the official cloud images
//! ```
//!
//! `start` boots virtiofsd, Cloud Hypervisor and the detached serial relay. The
//! child processes deliberately outlive the CLI process; `stop` terminates
//! them. VM lifecycle operations hold an exclusive lock on `hegn.lock` so two
//! concurrent `hegn open` calls cannot boot a VMM on the same sockets.
//!
//! Cloud Hypervisor supports virtio-fs natively, so no special fork or
//! patched build is required. The VMM always runs with seccomp filters and
//! Landlock restricting its filesystem view to the kernel, disks and
//! serial/fs sockets.
//!
//! The implementation is split by concern: `paths` owns the runtime path
//! layout, `session` the persisted metadata and reports, `args` the hardened
//! command lines, `process` the identity-checked process helpers, `util` the
//! small filesystem helpers, `registry` multi-instance lookup and `manager`
//! the lifecycle operations themselves.

mod args;
pub mod autosave;
mod manager;
mod paths;
mod process;
mod registry;
mod session;
mod util;

pub use manager::InstanceManager;
pub use paths::InstancePaths;
pub use process::{process_alive, process_matches};
pub use registry::{InstanceRegistry, InstanceSummary, same_project};
pub use session::{InstanceReport, SessionRecord};

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use crate::config::{BackendKind, Config, Harness};
    use crate::vm::instance::InstanceManager;
    use crate::vm::instance_config::InstanceConfig;

    pub(crate) fn config_with_assets(dir: &Path) -> Config {
        let mut config = Config::default();
        config.state_dir = dir.join("state");
        config.projects_root = dir.join("projects");
        config.snapshots_dir = dir.join("snapshots");
        config.instances_dir = dir.join("instances");
        config.profiles_dir = dir.join("profiles");
        config.templates_dir = dir.join("templates");
        config.kernel_image = dir.join("vmlinux");
        config.rootfs_image = dir.join("rootfs.qcow2");
        config.rootfs_format = crate::config::RootfsFormat::Qcow2;
        config.root_device = "/dev/vda1".into();
        config.backend = BackendKind::CloudHypervisor;
        config.cloud_hypervisor_bin = PathBuf::from("/definitely/missing/cloud-hypervisor");
        config.virtiofsd_bin = PathBuf::from("/definitely/missing/virtiofsd");
        std::fs::write(&config.kernel_image, b"kernel").unwrap();
        std::fs::write(&config.rootfs_image, b"rootfs").unwrap();
        std::fs::create_dir_all(&config.projects_root).unwrap();
        config
    }

    pub(crate) fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    pub(crate) fn instance_for(workspace: &Path) -> InstanceConfig {
        std::fs::create_dir_all(workspace).unwrap();
        InstanceConfig {
            name: "demo".into(),
            project: workspace.to_path_buf(),
            workspace: workspace.to_path_buf(),
            harness: Harness::Shell,
            vcpus: 1,
            memory_mib: 128,
            ..InstanceConfig::default()
        }
    }

    pub(crate) fn manager_for(config: Config, workspace: &Path) -> InstanceManager {
        InstanceManager::new(config, instance_for(workspace))
    }

    pub(crate) fn manager(config: Config, dir: &Path) -> InstanceManager {
        manager_for(config, &dir.join("project"))
    }
}
