//! Growing per-instance root filesystems before the first boot.
//!
//! The pinned cloud image is only about 2.2 GiB. That is enough for a bare
//! Ubuntu cloud image, but not for Node.js, a global npm package and the
//! user's extra apt selections. Each instance therefore gets a sparse disk
//! with a comfortable floor: raw images are extended with `set_len`, and qcow2
//! images get a larger virtual size (a bigger L1 table) without rewriting the
//! existing clusters.
//!
//! Growing a qcow2 image is deliberately narrow: only images without backing
//! files, snapshots, encryption or external data are handled in-process. For
//! anything else `qemu-img resize` is used as a fallback when it is installed.

mod qcow2;
mod raw;

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::RootfsFormat;

/// Comfortable floor for an instance root filesystem. Sparse files mean this
/// costs almost no host disk space until the guest actually writes to it.
pub const MIN_INSTANCE_ROOTFS_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Give `path` at least [`MIN_INSTANCE_ROOTFS_BYTES`] of virtual space, without
/// exceeding `max_bytes`.
///
/// Returns `true` when the file was extended.
pub fn grow_to_minimum(path: &Path, format: RootfsFormat, max_bytes: u64) -> Result<bool> {
    if max_bytes < MIN_INSTANCE_ROOTFS_BYTES {
        bail!(
            "rootfs ceiling is {} bytes, below the {} byte minimum needed for the harness \
             preset",
            max_bytes,
            MIN_INSTANCE_ROOTFS_BYTES
        );
    }
    grow(path, format, MIN_INSTANCE_ROOTFS_BYTES, max_bytes)
}

/// Extend `path` to `target` bytes, refusing to exceed `max_bytes`. A larger
/// file is left untouched.
pub fn grow(path: &Path, format: RootfsFormat, target: u64, max_bytes: u64) -> Result<bool> {
    if target > max_bytes {
        bail!(
            "refusing to grow {} to {} bytes: the configured ceiling is {} bytes",
            path.display(),
            target,
            max_bytes
        );
    }
    if !crate::vm::images::is_regular_file(path) {
        bail!("{} is not a regular file", path.display());
    }
    match format {
        RootfsFormat::Raw => raw::grow_raw(path, target),
        RootfsFormat::Qcow2 => match qcow2::grow_qcow2(path, target) {
            Ok(grew) => Ok(grew),
            Err(err) => match qcow2::qemu_img_resize(path, target) {
                Ok(grew) => Ok(grew),
                Err(fallback) => {
                    Err(err).context(format!("qemu-img fallback also failed: {fallback:#}"))
                }
            },
        },
    }
}

/// Open a disk for resizing without following symlinks or leaking the handle.
pub(super) fn open_rw(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to open {} for resizing", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn growth_refuses_to_exceed_the_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        fs::write(&path, b"disk").unwrap();
        let err = grow(&path, RootfsFormat::Raw, 2 * 1024 * 1024, 1024 * 1024).unwrap_err();
        assert!(format!("{err:#}").contains("ceiling"), "{err:#}");
        assert!(grow(&path, RootfsFormat::Raw, 1024 * 1024, 1024 * 1024).is_ok());
    }

    #[test]
    fn grow_to_minimum_rejects_a_ceiling_below_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        fs::write(&path, b"disk").unwrap();
        let err =
            grow_to_minimum(&path, RootfsFormat::Raw, MIN_INSTANCE_ROOTFS_BYTES - 1).unwrap_err();
        assert!(format!("{err:#}").contains("below"), "{err:#}");
    }
}
