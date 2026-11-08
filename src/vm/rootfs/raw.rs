//! Sparse growth of raw root filesystem images.
//!
//! A raw image is extended with `set_len`, which leaves the new range as
//! unwritten sparse space, so growing costs almost no host disk until the
//! guest writes to it.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::open_rw;

pub(super) fn grow_raw(path: &Path, target: u64) -> Result<bool> {
    let len = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .len();
    if len >= target {
        return Ok(false);
    }
    let file = open_rw(path)?;
    file.set_len(target)
        .with_context(|| format!("failed to grow {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootfsFormat;
    use crate::vm::rootfs::grow;
    use std::os::unix::fs::FileExt;

    #[test]
    fn raw_images_are_extended_sparsely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        fs::write(&path, b"rootfs").unwrap();
        assert!(grow(&path, RootfsFormat::Raw, 1024 * 1024, u64::MAX).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), 1024 * 1024);
        let mut prefix = [0u8; 6];
        fs::File::open(&path)
            .unwrap()
            .read_exact_at(&mut prefix, 0)
            .unwrap();
        assert_eq!(&prefix, b"rootfs");
    }
}
