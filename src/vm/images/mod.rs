//! Downloading and verifying the upstream Cloud Hypervisor VM assets.
//!
//! hegn deliberately does not build or publish its own guest image. The kernel
//! comes from the official Cloud Hypervisor Linux release and the root
//! filesystem from Cloud Hypervisor's published test images, pinned by URL
//! **and** SHA-256. Downloads are streamed into a private temporary file,
//! verified, and atomically renamed; a checksum mismatch never leaves a usable
//! file behind.

mod assets;
mod download;
mod verify;

/// Refuse absurdly large downloads before they fill the disk. The official
/// aarch64 cloud image is larger than 2 GiB, so the cap is 4 GiB.
pub const MAX_ASSET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub use assets::*;
pub use download::*;
pub use verify::*;
