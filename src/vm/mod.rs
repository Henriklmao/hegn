//! Cloud Hypervisor virtual-machine domain.
//!
//! Everything that knows about VMM processes, disk images, virtio-fs exports
//! and per-VM state lives here.
//!
//! - [`hypervisor`] — typed Cloud Hypervisor REST client
//! - [`virtiofs`] — hardened `virtiofsd` process construction
//! - [`rootfs`] — per-VM root filesystem preparation and growth
//! - [`images`] — pinned upstream assets, download and checksum verification
//! - [`cloud_init`] — NoCloud seed generation
//! - [`config`] — per-VM `vm.toml` and its store
//! - [`registry`] — VM summaries, resolution and project-to-VM selection
//! - [`manager`] — VM lifecycle, indexed multi-project shares and session records

pub mod cloud_init;
pub mod config;
pub mod hypervisor;
pub mod images;
pub mod manager;
pub mod registry;
pub mod rootfs;
pub mod virtiofs;

pub use manager::{
    BootPlan, SessionRecord, VmManager, VmPaths, VmReport, process_alive, process_matches,
};
pub use registry::{DeleteOutcome, VmRegistry, VmSummary, same_project};
