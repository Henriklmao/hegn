//! Private filesystem helpers for the per-instance profile bundle.
//!
//! These helpers never follow symlinks planted by a guest: secure directories
//! are recreated instead, and files are opened with `O_NOFOLLOW`.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub(super) fn secure_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        bail!("refusing symlinked profile directory {}", path.display());
    }
    crate::shared::fs::create_private_dir(path)
        .with_context(|| format!("failed to secure profile directory {}", path.display()))
}

/// Ensure `path` is a real directory owned by hegn. A guest can replace a
/// per-instance bundle subdirectory with a symlink; following it on the host
/// would let the guest redirect writes outside the instance. Remove such a
/// plant and recreate a private directory.
pub(super) fn ensure_real_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        remove_path(path)?;
    }
    secure_dir(path)
}

pub(super) fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        Ok(_) => {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    Ok(())
}

pub(super) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    crate::shared::fs::write_private(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))
}

pub(super) fn copy_file_private(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        secure_dir(parent)?;
    }
    let _ = fs::remove_file(dest);
    let source_metadata =
        fs::metadata(source).with_context(|| format!("failed to inspect {}", source.display()))?;
    let mut input = fs::File::open(source)
        .with_context(|| format!("failed to open profile source {}", source.display()))?;
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
    }
    output.sync_all()?;
    // Keep executable skill helpers usable while never widening the owner
    // permissions (the profile directory itself is 0700).
    let mode = (source_metadata.permissions().mode() & 0o755) | 0o600;
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn write_private_creates_0600_files_and_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_private(&path, b"secret").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"secret");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let victim = dir.path().join("victim");
        fs::write(&victim, b"do not touch").unwrap();
        let link = dir.path().join("evil");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(write_private(&link, b"overwrite").is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
    }

    #[test]
    fn secure_dir_is_private_and_rejects_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle");
        secure_dir(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let trap = dir.path().join("trap");
        fs::create_dir(&trap).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&trap, &link).unwrap();
        assert!(secure_dir(&link).is_err());
    }

    #[test]
    fn ensure_real_dir_replaces_a_planted_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let trap = dir.path().join("trap");
        fs::create_dir(&trap).unwrap();
        fs::write(trap.join("do-not-touch"), b"host").unwrap();
        let planted = dir.path().join("files");
        std::os::unix::fs::symlink(&trap, &planted).unwrap();

        ensure_real_dir(&planted).unwrap();
        let metadata = fs::symlink_metadata(&planted).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        assert_eq!(fs::read(trap.join("do-not-touch")).unwrap(), b"host");
        assert!(!trap.join("new").exists());
    }

    #[test]
    fn copy_file_private_never_writes_through_a_destination_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, b"payload").unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"host").unwrap();
        let dest = dir.path().join("dest");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();

        copy_file_private(&source, &dest).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"host");
        let metadata = fs::symlink_metadata(&dest).unwrap();
        assert!(metadata.is_file() && !metadata.file_type().is_symlink());
        assert_eq!(metadata.uid(), fs::metadata(&source).unwrap().uid());
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
        assert_eq!(metadata.permissions().mode() & 0o600, 0o600);
    }
}
