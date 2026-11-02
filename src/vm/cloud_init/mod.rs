//! Minimal NoCloud seed for the official Cloud Hypervisor cloud images.
//!
//! The upstream images have no usable local account until cloud-init runs. hegn
//! therefore writes a small FAT volume labelled `CIDATA` and attaches it as a
//! second disk. The seed configures an automatic root login on the serial
//! console and one first-boot provisioner for the selected harness preset.
//!
//! The seed contains no secrets and is regenerated on every boot.

mod network;
mod seed;
mod user_data;

pub use seed::{SEED_VERSION, disk_arg, instance_id, write_seed};
pub(crate) use seed::{safe_mount, safe_token};
