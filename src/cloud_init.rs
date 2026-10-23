//! Minimal NoCloud seed for the official Cloud Hypervisor cloud images.
//!
//! The upstream images have no usable local account until cloud-init runs. hegn
//! therefore writes a small FAT volume labelled `CIDATA` and attaches it as a
//! second disk. The seed only configures an automatic root login on the serial
//! console: the VM has no network device, the serial socket lives in a `0700`
//! run directory, and the guest is disposable.
//!
//! The seed contains no secrets and is regenerated on every boot.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};

/// 4 MiB is enough for the two tiny files and keeps the seed cheap.
const SEED_BYTES: u64 = 4 * 1024 * 1024;

const USER_DATA: &str = r#"#cloud-config
users:
  - default
  - name: hegn
    groups: [sudo]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    lock_passwd: true
write_files:
  - path: /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
    permissions: '0644'
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin root --keep-baud 115200,38400,9600 %I $TERM
  - path: /etc/systemd/system/serial-getty@ttyAMA0.service.d/autologin.conf
    permissions: '0644'
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin root --keep-baud 115200,38400,9600 %I $TERM
runcmd:
  - systemctl daemon-reload || true
  - systemctl enable --now serial-getty@ttyS0.service || true
  - systemctl enable --now serial-getty@ttyAMA0.service || true
"#;

const META_DATA: &str = "instance-id: hegn-sandbox\nlocal-hostname: hegn\n";

/// Write (or replace) the NoCloud seed at `path`.
///
/// The image is created with mode `0600` and `O_NOFOLLOW`; a planted symlink
/// cannot redirect the write.
pub fn write_seed(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to create cloud-init seed {}", path.display()))?;
    file.set_len(SEED_BYTES)
        .with_context(|| format!("failed to size cloud-init seed {}", path.display()))?;
    file.seek(SeekFrom::Start(0))?;

    fatfs::format_volume(
        &mut file,
        FormatVolumeOptions::new().volume_label(*b"CIDATA     "),
    )
    .with_context(|| format!("failed to format cloud-init seed {}", path.display()))?;

    {
        let fs = FileSystem::new(&mut file, FsOptions::new())
            .with_context(|| format!("failed to open cloud-init seed {}", path.display()))?;
        let root = fs.root_dir();
        write_file(&root, "user-data", USER_DATA)?;
        write_file(&root, "meta-data", META_DATA)?;
    }
    file.sync_all()?;
    Ok(())
}

fn write_file<IO: std::io::Read + std::io::Write + std::io::Seek>(
    root: &fatfs::Dir<'_, IO>,
    name: &str,
    contents: &str,
) -> Result<()> {
    let mut file = root
        .create_file(name)
        .with_context(|| format!("failed to create {name} in the cloud-init seed"))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {name} in the cloud-init seed"))?;
    file.flush()?;
    Ok(())
}

/// The exact `--disk` value for the seed.
pub fn disk_arg(path: &Path) -> String {
    format!("path={},image_type=raw", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn writes_a_readable_cidata_volume_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        write_seed(&path).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let file = OpenOptions::new().read(true).open(&path).unwrap();
        let fs = FileSystem::new(file, FsOptions::new()).unwrap();
        assert_eq!(fs.volume_label().trim(), "CIDATA");
        let root = fs.root_dir();
        let mut user_data = String::new();
        root.open_file("user-data")
            .unwrap()
            .read_to_string(&mut user_data)
            .unwrap();
        assert!(user_data.contains("autologin root"), "{user_data}");
        let mut meta_data = String::new();
        root.open_file("meta-data")
            .unwrap()
            .read_to_string(&mut meta_data)
            .unwrap();
        assert!(
            meta_data.contains("instance-id: hegn-sandbox"),
            "{meta_data}"
        );
    }

    #[test]
    fn refuses_to_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"do not touch").unwrap();
        let link = dir.path().join("cloud-init.img");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(write_seed(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"do not touch");
    }

    #[test]
    fn replaces_an_existing_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        write_seed(&path).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() >= SEED_BYTES);
    }
}
