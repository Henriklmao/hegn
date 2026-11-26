//! Cloud Hypervisor lifecycle orchestration.
//!
//! A VM boots one Cloud Hypervisor process and one sandboxed `virtiofsd` per
//! exported project:
//!
//! ```text
//! <vms_dir>/<name>/run/
//!   cloud-hypervisor.sock   REST API socket
//!   serial.sock             guest serial socket (Cloud Hypervisor listener)
//!   console.sock            relay socket used by `hegn attach`
//!   virtiofs-<i>.sock       member virtiofsd vhost-user socket
//!   virtiofs-<i>.pid        member virtiofsd pid
//!   profile-virtiofs.sock   second virtiofsd socket for the profile copy
//!   cloud-hypervisor.pid
//!   console-relay.pid
//!   profile-virtiofs.pid
//!   autosave.pid            detached periodic-snapshot worker
//!   session.json
//!   membership.json         exported members of the running boot
//!   cloud-hypervisor.log    VMM log
//!   console-relay.log       relay stderr (diagnostics only)
//!   autosave.log            autosnapshot worker stderr (diagnostics only)
//!   hegn.log                lifecycle/start diagnostics
//!   hegn.lock               lifecycle flock
//!   cloud-init.img          NoCloud seed for the official cloud images
//! ```
//!
//! `start` boots one virtiofsd per exported project, Cloud Hypervisor and the
//! detached serial relay. The child processes deliberately outlive the CLI
//! process; `stop` terminates them. VM lifecycle operations hold an exclusive
//! lock on `hegn.lock` so two concurrent `hegn open` calls cannot boot a VMM on
//! the same sockets.
//!
//! The implementation is split by concern: `paths` owns the runtime path
//! layout, `plan` resolves registry membership into indexed shares,
//! `membership` persists it, `session` holds the persisted metadata and
//! reports, `args` the hardened command lines, `process` the identity-checked
//! process helpers, `util` the small filesystem helpers and `core` the shared
//! manager state.

mod args;
pub mod autosave;
mod cleanup;
mod core;
mod membership;
mod paths;
mod plan;
mod process;
mod repair;
mod seed_generation;
mod session;
mod snapshot;
mod start;
mod status;
mod stop;
mod util;

pub use core::VmManager;
pub use membership::{MEMBERSHIP_FILE, MEMBERSHIP_VERSION, MembershipManifest, MembershipMember};
pub use paths::VmPaths;
pub use plan::{BootPlan, MAX_SLUG_BYTES, MAX_TAG_BYTES, SharedMember, slugify};
pub use process::{process_alive, process_matches};
pub use session::{SessionRecord, VmReport};

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use chrono::Utc;

    use crate::config::{BackendKind, Config, Harness};
    use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
    use crate::vm::config::{VmConfig, VmStore};
    use crate::vm::manager::VmManager;

    pub(crate) fn config_with_assets(dir: &Path) -> Config {
        let mut config = Config::default();
        config.state_dir = dir.join("state");
        config.projects_root = dir.join("projects");
        config.snapshots_dir = dir.join("snapshots");
        config.vms_dir = dir.join("vms");
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

    pub(crate) fn vm_for(workspace: &Path) -> VmConfig {
        std::fs::create_dir_all(workspace).unwrap();
        VmConfig {
            name: "demo".into(),
            harness: Harness::Shell,
            vcpus: 1,
            memory_mib: 128,
            ..VmConfig::default()
        }
    }

    /// Persist a one-member VM for `workspace` and return its manager.
    pub(crate) fn manager_for(config: Config, workspace: &Path) -> VmManager {
        std::fs::create_dir_all(workspace).unwrap();
        let vm = vm_for(workspace);
        VmStore::new(&config).save(&vm).unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry
            .upsert(ProjectEntry {
                name: "demo".into(),
                path: workspace.canonicalize().unwrap(),
                vm: None,
                read_only: false,
                kind: ProjectKind::External,
                parent: None,
                managed: false,
                hidden: false,
                created_at: Utc::now(),
            })
            .unwrap();
        registry.assign("demo", &vm.name, false).unwrap();
        VmManager::new(config, vm).unwrap()
    }

    pub(crate) fn manager(config: Config, dir: &Path) -> VmManager {
        manager_for(config, &dir.join("project"))
    }

    pub(crate) fn plan_for(manager: &VmManager) -> crate::vm::manager::BootPlan {
        crate::vm::manager::BootPlan::build(
            manager.config(),
            manager.vm(),
            manager.store(),
            manager.registry(),
            &manager.paths(),
        )
        .unwrap()
    }
}
