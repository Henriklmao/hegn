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

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::RootfsFormat;

/// Comfortable floor for an instance root filesystem. Sparse files mean this
/// costs almost no host disk space until the guest actually writes to it.
pub const MIN_INSTANCE_ROOTFS_BYTES: u64 = 16 * 1024 * 1024 * 1024;

const QCOW2_MAGIC: u32 = 0x5146_49fb;
const HEADER_BYTES: usize = 104;
const INCOMPAT_DIRTY: u64 = 1 << 0;
const INCOMPAT_CORRUPT: u64 = 1 << 1;
const INCOMPAT_EXTERNAL_DATA: u64 = 1 << 2;
const INCOMPAT_COMPRESSION_TYPE: u64 = 1 << 3;

/// Give `path` at least [`MIN_INSTANCE_ROOTFS_BYTES`] of virtual space.
///
/// Returns `true` when the file was extended.
pub fn grow_to_minimum(path: &Path, format: RootfsFormat) -> Result<bool> {
    grow(path, format, MIN_INSTANCE_ROOTFS_BYTES)
}

/// Extend `path` to `target` bytes. A larger file is left untouched.
pub fn grow(path: &Path, format: RootfsFormat, target: u64) -> Result<bool> {
    if !crate::images::is_regular_file(path) {
        bail!("{} is not a regular file", path.display());
    }
    match format {
        RootfsFormat::Raw => grow_raw(path, target),
        RootfsFormat::Qcow2 => match grow_qcow2(path, target) {
            Ok(grew) => Ok(grew),
            Err(err) => match qemu_img_resize(path, target) {
                Ok(grew) => Ok(grew),
                Err(fallback) => {
                    Err(err).context(format!("qemu-img fallback also failed: {fallback:#}"))
                }
            },
        },
    }
}

fn open_rw(path: &Path) -> Result<fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to open {} for resizing", path.display()))
}

fn grow_raw(path: &Path, target: u64) -> Result<bool> {
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

fn qemu_img_resize(path: &Path, target: u64) -> Result<bool> {
    let output = Command::new("qemu-img")
        .arg("resize")
        .arg(path)
        .arg(target.to_string())
        .output()
        .context("qemu-img is not installed")?;
    if !output.status.success() {
        bail!(
            "qemu-img resize failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy)]
struct Header {
    cluster_bits: u32,
    size: u64,
    l1_size: u32,
    l1_table_offset: u64,
    refcount_table_offset: u64,
    refcount_table_clusters: u32,
    nb_snapshots: u32,
    incompatible_features: u64,
    refcount_order: u32,
    backing_file_offset: u64,
    crypt_method: u32,
}

fn be_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn be_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

fn read_header(file: &fs::File) -> Result<Header> {
    let mut buf = [0u8; HEADER_BYTES];
    let read = file
        .read_at(&mut buf, 0)
        .context("failed to read the qcow2 header")?;
    if read < 72 {
        bail!("not a qcow2 image ({read} bytes)");
    }
    let magic = be_u32(&buf, 0);
    if magic != QCOW2_MAGIC {
        bail!("not a qcow2 image (magic 0x{magic:08x})");
    }
    let version = be_u32(&buf, 4);
    if !matches!(version, 2 | 3) {
        bail!("unsupported qcow2 version {version}");
    }
    let header_length = if version >= 3 { be_u32(&buf, 100) } else { 72 };
    if !(72..=(1 << 20)).contains(&header_length) {
        bail!("unsupported qcow2 header length {header_length}");
    }
    let refcount_order = if version >= 3 { be_u32(&buf, 96) } else { 4 };
    Ok(Header {
        cluster_bits: be_u32(&buf, 20),
        size: be_u64(&buf, 24),
        crypt_method: be_u32(&buf, 32),
        l1_size: be_u32(&buf, 36),
        l1_table_offset: be_u64(&buf, 40),
        refcount_table_offset: be_u64(&buf, 48),
        refcount_table_clusters: be_u32(&buf, 56),
        nb_snapshots: be_u32(&buf, 60),
        incompatible_features: be_u64(&buf, 72),
        refcount_order,
        backing_file_offset: be_u64(&buf, 8),
    })
}

fn grow_qcow2(path: &Path, target: u64) -> Result<bool> {
    let file = open_rw(path)?;
    let header = read_header(&file)?;
    if header.backing_file_offset != 0 {
        bail!("qcow2 images with a backing file are not resized in-process");
    }
    if header.crypt_method != 0 {
        bail!("encrypted qcow2 images are not resized in-process");
    }
    if header.nb_snapshots != 0 {
        bail!("qcow2 images with internal snapshots are not resized in-process");
    }
    let incompatible = header.incompatible_features;
    if incompatible & (INCOMPAT_DIRTY | INCOMPAT_CORRUPT) != 0 {
        bail!("qcow2 image is dirty or corrupt; refusing to resize it");
    }
    if incompatible & INCOMPAT_EXTERNAL_DATA != 0 {
        bail!("qcow2 images with external data files are not resized in-process");
    }
    if incompatible & INCOMPAT_COMPRESSION_TYPE != 0 {
        bail!("qcow2 images with extended compression metadata are not resized in-process");
    }
    if !(9..=21).contains(&header.cluster_bits) {
        bail!("invalid qcow2 cluster size (2^{})", header.cluster_bits);
    }
    let cluster_size = 1u64 << header.cluster_bits;
    if target <= header.size {
        return Ok(false);
    }
    if header.l1_size == 0 || header.l1_size as u64 > (1 << 24) {
        bail!("unreasonable qcow2 L1 table size {}", header.l1_size);
    }
    let target = target
        .checked_add(cluster_size - 1)
        .map(|size| size & !(cluster_size - 1))
        .context("rootfs size overflow")?;
    let l2_entries = cluster_size / 8;
    let l2_coverage = l2_entries
        .checked_mul(cluster_size)
        .context("qcow2 coverage overflow")?;
    let new_l1_size = target.div_ceil(l2_coverage);
    if new_l1_size > u32::MAX as u64 {
        bail!("requested qcow2 size is too large");
    }

    if new_l1_size <= header.l1_size as u64 {
        // The existing L1 table already covers the new space; only the header
        // size needs to change.
        file.write_all_at(&target.to_be_bytes(), 24)
            .context("failed to update the qcow2 size")?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", path.display()))?;
        return Ok(true);
    }

    if new_l1_size * 8 > cluster_size {
        bail!("new qcow2 L1 table does not fit in one cluster");
    }
    let (new_l1_cluster, refcount_entry_bytes) = allocate_l1_cluster(&file, &header, cluster_size)?;
    let new_l1_offset = new_l1_cluster * cluster_size;
    let old_l1_len = header.l1_size as usize * 8;
    let mut old_l1 = vec![0u8; old_l1_len];
    file.read_exact_at(&mut old_l1, header.l1_table_offset)
        .context("failed to read the old qcow2 L1 table")?;
    let mut new_table = vec![0u8; cluster_size as usize];
    new_table[..old_l1_len].copy_from_slice(&old_l1);
    file.write_all_at(&new_table, new_l1_offset)
        .context("failed to write the new qcow2 L1 table")?;
    let new_end = new_l1_offset + cluster_size;
    if file.metadata()?.len() < new_end {
        file.set_len(new_end)
            .context("failed to extend the qcow2 image")?;
    }
    write_refcount(
        &file,
        &header,
        cluster_size,
        refcount_entry_bytes,
        new_l1_cluster,
    )?;

    // Layout matches the header: size (24), crypt method (32, untouched),
    // L1 size (36), L1 offset (40).
    let mut header_update = [0u8; 24];
    header_update[..8].copy_from_slice(&target.to_be_bytes());
    header_update[12..16].copy_from_slice(&(new_l1_size as u32).to_be_bytes());
    header_update[16..24].copy_from_slice(&new_l1_offset.to_be_bytes());
    file.write_all_at(&header_update, 24)
        .context("failed to update the qcow2 header")?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    Ok(true)
}

/// Pick a free cluster for the new L1 table and mark it allocated.
///
/// Existing holes are preferred so the image does not have to grow its
/// refcount table. When no hole is available, a cluster is appended as long as
/// the current refcount table already covers it.
fn allocate_l1_cluster(
    file: &fs::File,
    header: &Header,
    cluster_size: u64,
) -> Result<(u64, usize)> {
    let order = header.refcount_order;
    if !(3..=6).contains(&order) {
        bail!("unsupported qcow2 refcount order {order}");
    }
    let entry_bytes = (1usize << order) / 8;
    let entries_per_block = cluster_size as usize / entry_bytes;
    let table_entries =
        header.refcount_table_clusters as usize * cluster_size as usize / size_of::<u64>();
    let coverage = table_entries as u64 * entries_per_block as u64;
    let file_len = file.metadata()?.len();

    for block_index in 0..table_entries {
        let block_offset = refcount_block_offset(file, header, block_index)?;
        if block_offset == 0 {
            continue;
        }
        let mut block = vec![0u8; cluster_size as usize];
        file.read_exact_at(&mut block, block_offset)
            .with_context(|| {
                format!(
                    "failed to read qcow2 refcount block {block_index} at {block_offset} \
                 (file is {} bytes)",
                    file_len
                )
            })?;
        for entry in 0..entries_per_block {
            if read_refcount(&block, entry, entry_bytes) != 0 {
                continue;
            }
            let cluster = block_index as u64 * entries_per_block as u64 + entry as u64;
            // Never touch a cluster that the refcount table cannot describe.
            // Cluster zero holds the header, so a zero refcount there means the
            // image is already inconsistent; do not overwrite it.
            if cluster == 0 || cluster >= coverage {
                continue;
            }
            return Ok((cluster, entry_bytes));
        }
    }

    let cluster = file_len.div_ceil(cluster_size);
    if cluster >= coverage {
        bail!("qcow2 refcount table is full; cannot append an L1 table");
    }
    let block_index = (cluster as usize) / entries_per_block;
    if refcount_block_offset(file, header, block_index)? == 0 {
        bail!("qcow2 refcount block is missing for cluster {cluster}");
    }
    Ok((cluster, entry_bytes))
}

fn refcount_block_offset(file: &fs::File, header: &Header, block_index: usize) -> Result<u64> {
    let mut buf = [0u8; 8];
    file.read_exact_at(
        &mut buf,
        header.refcount_table_offset + block_index as u64 * 8,
    )
    .context("failed to read the qcow2 refcount table")?;
    Ok(u64::from_be_bytes(buf) & 0x00ff_ffff_ffff_fe00)
}

fn read_refcount(block: &[u8], index: usize, entry_bytes: usize) -> u64 {
    let start = index * entry_bytes;
    let mut value = 0u64;
    for byte in &block[start..start + entry_bytes] {
        value = (value << 8) | *byte as u64;
    }
    value
}

fn write_refcount(
    file: &fs::File,
    header: &Header,
    cluster_size: u64,
    entry_bytes: usize,
    cluster: u64,
) -> Result<()> {
    let entries_per_block = cluster_size as usize / entry_bytes;
    let block_index = cluster as usize / entries_per_block;
    let entry = cluster as usize % entries_per_block;
    let block_offset = refcount_block_offset(file, header, block_index)?;
    if block_offset == 0 {
        bail!("qcow2 refcount block is missing for cluster {cluster}");
    }
    let mut block = vec![0u8; cluster_size as usize];
    file.read_exact_at(&mut block, block_offset)
        .context("failed to read a qcow2 refcount block")?;
    if read_refcount(&block, entry, entry_bytes) != 0 {
        bail!("qcow2 cluster {cluster} is already allocated");
    }
    let start = entry * entry_bytes;
    for byte in &mut block[start..start + entry_bytes] {
        *byte = 0;
    }
    // Refcounts are big-endian like every other qcow2 integer.
    block[start + entry_bytes - 1] = 1;
    file.write_all_at(&block, block_offset)
        .context("failed to update a qcow2 refcount block")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    const CLUSTER: usize = 512;

    fn write_be_u32(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn write_be_u64(buf: &mut [u8], offset: usize, value: u64) {
        buf[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
    }

    /// Build a tiny valid qcow2 with one data cluster and a refcount table for
    /// 256 clusters.
    fn synthetic_qcow2(path: &Path) -> u64 {
        let mut image = vec![0u8; CLUSTER * 6];
        write_be_u32(&mut image, 0, QCOW2_MAGIC);
        write_be_u32(&mut image, 4, 3);
        write_be_u32(&mut image, 20, 9);
        write_be_u64(&mut image, 24, (CLUSTER * 64) as u64);
        write_be_u32(&mut image, 36, 1);
        write_be_u64(&mut image, 40, (CLUSTER * 3) as u64);
        write_be_u64(&mut image, 48, CLUSTER as u64);
        write_be_u32(&mut image, 56, 1);
        write_be_u32(&mut image, 96, 4);
        write_be_u32(&mut image, 100, 104);
        // Refcount table -> block in cluster 2.
        write_be_u64(&mut image, CLUSTER, (CLUSTER * 2) as u64);
        // Clusters 0..=5 are allocated, every other cluster is free.
        for cluster in 0..6 {
            image[CLUSTER * 2 + cluster * 2 + 1] = 1;
        }
        // L1[0] -> L2 table in cluster 4; L2[0] -> data cluster 5.
        write_be_u64(&mut image, CLUSTER * 3, (CLUSTER * 4) as u64);
        write_be_u64(&mut image, CLUSTER * 4, (CLUSTER * 5) as u64);
        image[CLUSTER * 5..CLUSTER * 6].fill(0x5a);
        let mut file = fs::File::create(path).unwrap();
        file.write_all(&image).unwrap();
        file.sync_all().unwrap();
        (CLUSTER * 64) as u64
    }

    fn read_u64(file: &fs::File, offset: u64) -> u64 {
        let mut buf = [0u8; 8];
        file.read_exact_at(&mut buf, offset).unwrap();
        u64::from_be_bytes(buf)
    }

    fn read_u32(file: &fs::File, offset: u64) -> u32 {
        let mut buf = [0u8; 4];
        file.read_exact_at(&mut buf, offset).unwrap();
        u32::from_be_bytes(buf)
    }

    #[test]
    fn qcow2_grows_without_losing_the_first_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        synthetic_qcow2(&path);
        let target = 1024 * 1024;
        assert!(grow(&path, RootfsFormat::Qcow2, target).unwrap());

        let file = OpenOptions::new().read(true).open(&path).unwrap();
        assert_eq!(read_u64(&file, 24), target);
        let l1_size = read_u32(&file, 36);
        let l1_offset = read_u64(&file, 40);
        assert_eq!(l1_size, 32);
        assert_eq!(l1_offset % CLUSTER as u64, 0);
        // The new L1 cluster is allocated with a big-endian refcount of one.
        let refcount_table = read_u64(&file, CLUSTER as u64);
        let mut refcount = [0u8; 2];
        file.read_exact_at(
            &mut refcount,
            refcount_table + (l1_offset / CLUSTER as u64) * 2,
        )
        .unwrap();
        assert_eq!(u16::from_be_bytes(refcount), 1);
        // The old L1 entry still points at the same L2 table.
        assert_eq!(read_u64(&file, l1_offset), (CLUSTER * 4) as u64);
        // And the data cluster is still readable through the new table.
        let l2_offset = read_u64(&file, l1_offset) & 0x00ff_ffff_ffff_fe00;
        let data_offset = read_u64(&file, l2_offset) & 0x00ff_ffff_ffff_fe00;
        let mut data = [0u8; CLUSTER];
        file.read_exact_at(&mut data, data_offset).unwrap();
        assert!(data.iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn qcow2_larger_than_the_floor_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        let size = synthetic_qcow2(&path);
        assert!(!grow(&path, RootfsFormat::Qcow2, size).unwrap());
    }

    #[test]
    fn raw_images_are_extended_sparsely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        fs::write(&path, b"rootfs").unwrap();
        assert!(grow(&path, RootfsFormat::Raw, 1024 * 1024).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), 1024 * 1024);
        let mut prefix = [0u8; 6];
        fs::File::open(&path)
            .unwrap()
            .read_exact_at(&mut prefix, 0)
            .unwrap();
        assert_eq!(&prefix, b"rootfs");
    }

    #[test]
    fn non_qcow2_bytes_are_rejected_with_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        fs::write(&path, b"not a disk").unwrap();
        let err = grow(&path, RootfsFormat::Qcow2, 1024 * 1024).unwrap_err();
        assert!(format!("{err:#}").contains("not a qcow2 image"), "{err:#}");
    }

    #[test]
    fn writing_the_header_after_growth_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        synthetic_qcow2(&path);
        let target = 1024 * 1024;
        grow(&path, RootfsFormat::Qcow2, target).unwrap();
        let before = fs::metadata(&path).unwrap().len();
        assert!(!grow(&path, RootfsFormat::Qcow2, target).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), before);
    }

    #[test]
    fn append_falls_back_to_a_hole_when_eof_is_outside_the_refcount_table() {
        // Keep the helper honest: a file with an allocated cluster count at the
        // refcount coverage boundary must still find an earlier hole instead of
        // appending a cluster it cannot describe.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        synthetic_qcow2(&path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // Make cluster 6 look allocated, leaving cluster 7 as the first hole.
        let refcount = CLUSTER * 2;
        file.seek(SeekFrom::Start(refcount as u64 + 6 * 2)).unwrap();
        file.write_all(&1u16.to_be_bytes()).unwrap();
        file.sync_all().unwrap();
        let target = 1024 * 1024;
        grow(&path, RootfsFormat::Qcow2, target).unwrap();
        let file = OpenOptions::new().read(true).open(&path).unwrap();
        let l1_offset = read_u64(&file, 40);
        assert_eq!(l1_offset, (CLUSTER * 7) as u64);
    }
}
