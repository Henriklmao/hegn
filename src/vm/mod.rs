//! Cloud Hypervisor virtual-machine domain.
//!
//! Everything that knows about VMM processes, disk images, virtio-fs exports
//! and per-instance VM state lives here.
//!
//! - [`hypervisor`] — typed Cloud Hypervisor REST client
//! - [`virtiofs`] — hardened `virtiofsd` process construction
//! - [`rootfs`] — sparse per-instance root filesystem growth
//! - [`images`] — pinned upstream assets, download and checksum verification
//! - [`cloud_init`] — NoCloud seed generation
//! - [`instance_config`] — per-instance `hegn.toml`, clones and rootfs copies
//! - [`instance`] — VM lifecycle, registry and session records

pub mod cloud_init;
pub mod hypervisor;
pub mod images;
pub mod instance;
pub mod instance_config;
pub mod rootfs;
pub mod virtiofs;
