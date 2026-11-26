//! Per-VM configuration (`vm.toml`) and its filesystem store.
//!
//! ```text
//! <vms_dir>/<name>/
//!   vm.toml       tool profile, resources and runtime policy (0600)
//!   workspaces/   hegn-managed clone workspaces
//!   run/          runtime sockets/pids/locks (created on start)
//! ```
//!
//! Membership is not stored here. The private project registry decides which
//! projects a VM exports; this config only describes the VM itself.

mod model;
mod store;

pub use model::{CONFIG_VERSION, SshUser, VM_CONFIG_FILE, VmConfig, validate_name};
pub use store::VmStore;
