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
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure {}", path.display()))?;
    Ok(())
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
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", path.display()))?;
    Ok(())
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
