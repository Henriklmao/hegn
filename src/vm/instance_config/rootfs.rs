//! Per-instance root filesystem preparation.
//!
//! The first start copies (or reflinks) the shared image into the instance and
//! grows it so cloud-init has room for the harness preset. Concurrent instances
//! therefore never share a writable disk.

use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Config;

use super::model::InstanceConfig;

impl InstanceConfig {
    /// Root filesystem used by this instance, without creating it.
    pub fn rootfs_path(&self, config: &Config) -> PathBuf {
        self.rootfs_image.clone().unwrap_or_else(|| {
            self.dir(&config.instances_dir)
                .join(format!("rootfs.{}", config.rootfs_format.as_str()))
        })
    }

    /// Ensure the instance has a private root filesystem.
    ///
    /// The first start copies (or reflinks) the global image into the instance.
    /// Concurrent instances therefore never open the same writable disk.
    pub fn prepare_rootfs(&self, config: &Config) -> Result<PathBuf> {
        let dest = self.rootfs_path(config);
        if crate::vm::images::is_regular_file(&dest) {
            // An instance that was prepared by an older hegn version may still
            // be at the cloud image's 2.2 GiB. Grow it before the next boot so
            // the provisioner has room for Node.js and the harness.
            self.grow_rootfs(config, &dest)?;
            return Ok(dest);
        }
        if self.rootfs_image.is_some() {
            bail!("instance rootfs_image {} does not exist", dest.display());
        }
        if !crate::vm::images::is_regular_file(&config.rootfs_image) {
            bail!("base rootfs {} is missing", config.rootfs_image.display());
        }
        super::clone::create_private_dir(dest.parent().unwrap_or(&dest))?;
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
            // FICLONE is supported by btrfs/XFS/overlayfs. Fall back to a
            // full copy on filesystems without reflink support.
            use std::os::unix::io::AsRawFd;
            const FICLONE: libc::c_ulong = 0x4004_9409;
            let cloned =
                unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0;
            if !cloned {
                target.set_len(0)?;
                std::io::copy(&mut source, &mut target)?;
            }
            target.sync_all()?;
            fs::rename(&tmp, &dest)
                .with_context(|| format!("failed to install {}", dest.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        self.grow_rootfs(config, &dest)?;
        Ok(dest)
    }

    /// Grow the guest disk before the first boot. Cloud-init installs the
    /// harness inside the guest, so the upstream 2.2 GiB image is too small.
    fn grow_rootfs(&self, config: &Config, path: &Path) -> Result<()> {
        if !config.cloud_init {
            return Ok(());
        }
        crate::vm::rootfs::grow_to_minimum(path, config.rootfs_format).with_context(|| {
            format!(
                "failed to grow the instance rootfs {} (the first boot needs room for the \
                 harness preset)",
                path.display()
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::instance_config::test_support::{config, project};

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
        let store = crate::vm::instance_config::InstanceStore::new(cfg.clone());
        let project = project(dir.path(), "demo");
        let instance = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        let rootfs = instance.prepare_rootfs(&cfg).unwrap();
        assert!(rootfs.starts_with(store.dir("demo").unwrap()));
        assert_eq!(fs::read(&rootfs).unwrap(), b"rootfs-bytes");
        assert_eq!(fs::read(&cfg.rootfs_image).unwrap(), b"rootfs-bytes");
        // A second call reuses the same private copy.
        assert_eq!(instance.prepare_rootfs(&cfg).unwrap(), rootfs);
    }

    #[test]
    fn rootfs_path_uses_the_instance_directory_and_configured_format() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_format = crate::config::RootfsFormat::Raw;
        let instance = crate::vm::instance_config::InstanceConfig {
            name: "demo".into(),
            project: dir.path().join("project"),
            workspace: dir.path().join("project"),
            ..Default::default()
        };
        assert_eq!(
            instance.rootfs_path(&cfg),
            cfg.instances_dir.join("demo/rootfs.raw")
        );
        cfg.rootfs_format = crate::config::RootfsFormat::Qcow2;
        assert_eq!(
            instance.rootfs_path(&cfg),
            cfg.instances_dir.join("demo/rootfs.qcow2")
        );
        // An explicit per-instance image always wins.
        let mut instance = instance;
        instance.rootfs_image = Some(dir.path().join("explicit.img"));
        assert_eq!(instance.rootfs_path(&cfg), dir.path().join("explicit.img"));
    }

    #[test]
    fn prepare_rootfs_reports_a_missing_base_image() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_image = dir.path().join("missing-base.qcow2");
        cfg.cloud_init = false;
        let store = crate::vm::instance_config::InstanceStore::new(cfg.clone());
        let project = project(dir.path(), "demo");
        let instance = store
            .create_from_project(&project, None, None, false)
            .unwrap();

        let err = instance.prepare_rootfs(&cfg).unwrap_err().to_string();
        assert!(err.contains("base rootfs"), "{err}");
        assert!(!instance.rootfs_path(&cfg).exists());
    }

    #[test]
    fn prepare_rootfs_reports_a_missing_explicit_instance_image() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_image = dir.path().join("base.qcow2");
        cfg.cloud_init = false;
        fs::write(&cfg.rootfs_image, b"base").unwrap();
        let store = crate::vm::instance_config::InstanceStore::new(cfg.clone());
        let project = project(dir.path(), "demo");
        let mut instance = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        instance.rootfs_image = Some(dir.path().join("gone.qcow2"));

        let err = instance.prepare_rootfs(&cfg).unwrap_err().to_string();
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
        let store = crate::vm::instance_config::InstanceStore::new(cfg.clone());
        let project = project(dir.path(), "demo");
        let instance = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        let rootfs = instance.prepare_rootfs(&cfg).unwrap();

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
