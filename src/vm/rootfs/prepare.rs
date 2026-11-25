//! Per-VM root filesystem preparation.
//!
//! The first start copies (or reflinks) the shared base image into the VM and
//! grows it so cloud-init has room for the harness preset. Every VM therefore
//! has its own writable disk, shared by all of that VM's projects.

use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::vm::config::VmConfig;

/// Ensure the VM has a private root filesystem and return its path.
///
/// The first start copies (or reflinks) the global image into the VM. A
/// pre-existing explicit image is never copied; a missing one is an error.
pub fn prepare_rootfs(config: &Config, vm: &VmConfig) -> Result<PathBuf> {
    let dest = vm.rootfs_path(config);
    if crate::vm::images::is_regular_file(&dest) {
        // A VM created by an older hegn version may still use the cloud
        // image's 2.2 GiB size. Grow it before the next boot so the
        // provisioner has room for Node.js and the harness.
        grow_rootfs(config, &dest)?;
        return Ok(dest);
    }
    if vm.rootfs_image.is_some() {
        bail!("VM rootfs_image {} does not exist", dest.display());
    }
    if !crate::vm::images::is_regular_file(&config.rootfs_image) {
        bail!("base rootfs {} is missing", config.rootfs_image.display());
    }
    crate::shared::fs::create_private_dir(dest.parent().unwrap_or(&dest))?;
    let tmp = dest.with_file_name(format!(".rootfs.part-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let result = (|| -> Result<()> {
        let mut source = fs::OpenOptions::new()
            .read(true)
            .open(&config.rootfs_image)
            .with_context(|| format!("failed to open {}", config.rootfs_image.display()))?;
        let mut target = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        // FICLONE is supported by btrfs/XFS/overlayfs. Fall back to a full
        // copy on filesystems without reflink support.
        use std::os::unix::io::AsRawFd;
        const FICLONE: libc::c_ulong = 0x4004_9409;
        let cloned = unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0;
        if !cloned {
            target.set_len(0)?;
            std::io::copy(&mut source, &mut target)?;
        }
        target.sync_all()?;
        fs::rename(&tmp, &dest).with_context(|| format!("failed to install {}", dest.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    grow_rootfs(config, &dest)?;
    Ok(dest)
}

/// Grow the guest disk before the first boot. Cloud-init installs the harness
/// inside the guest, so the upstream 2.2 GiB image is too small.
fn grow_rootfs(config: &Config, path: &Path) -> Result<()> {
    if !config.cloud_init {
        return Ok(());
    }
    crate::vm::rootfs::grow_to_minimum(path, config.rootfs_format, config.max_rootfs_bytes())
        .with_context(|| {
            format!(
                "failed to grow the VM rootfs {} (the first boot needs room for the harness \
                 preset)",
                path.display()
            )
        })?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::vm::config::VmConfig;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.vms_dir = dir.join("vms");
        config.projects_root = dir.join("projects");
        config
    }

    fn vm(name: &str) -> VmConfig {
        VmConfig {
            name: name.into(),
            ..VmConfig::default()
        }
    }

    #[test]
    fn prepares_a_private_rootfs_copy_without_touching_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_image = dir.path().join("base.qcow2");
        cfg.rootfs_format = crate::config::RootfsFormat::Qcow2;
        // This fixture is not a real qcow2 image, so disable the first-boot
        // growth path and assert the plain copy behaviour.
        cfg.cloud_init = false;
        fs::write(&cfg.rootfs_image, b"rootfs-bytes").unwrap();
        let rootfs = prepare_rootfs(&cfg, &vm("demo")).unwrap();
        assert!(rootfs.starts_with(cfg.vms_dir.join("demo")));
        assert_eq!(fs::read(&rootfs).unwrap(), b"rootfs-bytes");
        assert_eq!(fs::read(&cfg.rootfs_image).unwrap(), b"rootfs-bytes");
        // A second call reuses the same private copy.
        assert_eq!(prepare_rootfs(&cfg, &vm("demo")).unwrap(), rootfs);
    }

    #[test]
    fn rootfs_path_uses_the_vm_directory_and_configured_format() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_format = crate::config::RootfsFormat::Raw;
        let vm = vm("demo");
        assert_eq!(vm.rootfs_path(&cfg), cfg.vms_dir.join("demo/rootfs.raw"));
        cfg.rootfs_format = crate::config::RootfsFormat::Qcow2;
        assert_eq!(vm.rootfs_path(&cfg), cfg.vms_dir.join("demo/rootfs.qcow2"));
        // An explicit per-VM image always wins.
        let mut vm = vm;
        vm.rootfs_image = Some(dir.path().join("explicit.img"));
        assert_eq!(vm.rootfs_path(&cfg), dir.path().join("explicit.img"));
    }

    #[test]
    fn prepare_rootfs_reports_a_missing_base_image() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_image = dir.path().join("missing-base.qcow2");
        cfg.cloud_init = false;

        let err = prepare_rootfs(&cfg, &vm("demo")).unwrap_err().to_string();
        assert!(err.contains("base rootfs"), "{err}");
        assert!(!vm("demo").rootfs_path(&cfg).exists());
    }

    #[test]
    fn prepare_rootfs_reports_a_missing_explicit_vm_image() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_image = dir.path().join("base.qcow2");
        cfg.cloud_init = false;
        fs::write(&cfg.rootfs_image, b"base").unwrap();
        let mut vm = vm("demo");
        vm.rootfs_image = Some(dir.path().join("gone.qcow2"));

        let err = prepare_rootfs(&cfg, &vm).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert_eq!(fs::read(&cfg.rootfs_image).unwrap(), b"base");
    }

    #[test]
    fn prepared_rootfs_is_private_and_leaves_no_partial_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_image = dir.path().join("base.img");
        cfg.cloud_init = false;
        fs::write(&cfg.rootfs_image, b"base").unwrap();
        let rootfs = prepare_rootfs(&cfg, &vm("demo")).unwrap();

        assert_eq!(
            fs::metadata(&rootfs).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let leftovers: Vec<_> = fs::read_dir(rootfs.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".rootfs.part-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
