//! Private file and directory helpers.
//!
//! The helpers here implement hegn's on-disk invariants in one place: regular
//! metadata files are `0600`, private directories are `0700`, and neither
//! follows a planted symlink.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Write `contents` to `path` as a private (`0600`) regular file.
///
/// The file is opened with `O_NOFOLLOW`, so a symlink at `path` is refused
/// instead of redirecting the write. An existing file is truncated and its
/// permissions are tightened to `0600`.
pub fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(contents.as_ref())?;
    file.sync_all()?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Create `path` recursively with mode `0700`, refusing a symlinked directory.
///
/// Existing directories are tightened to `0700` as well, because all callers
/// store sockets, logs or metadata below them.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing symlinked directory {}", path.display()),
        ));
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Sum the apparent size of regular files directly inside `dir`.
///
/// Symlinks and special files are ignored. A missing or unreadable directory
/// yields zero so callers can use this for best-effort accounting.
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::symlink_metadata(entry.path()).ok())
        .filter(|meta| meta.file_type().is_file())
        .map(|meta| meta.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_private_creates_a_0600_file_and_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        write_private(&path, b"{}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let target = dir.path().join("target");
        fs::write(&target, b"keep").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_private(&link, b"replace").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn create_private_dir_tightens_and_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/logs");
        create_private_dir(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(create_private_dir(&link).is_err());
    }

    #[test]
    fn dir_size_counts_only_regular_files_directly_inside() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), vec![0u8; 10]).unwrap();
        fs::write(dir.path().join("b"), vec![0u8; 5]).unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/c"), vec![0u8; 100]).unwrap();
        assert_eq!(dir_size(dir.path()), 15);
        assert_eq!(dir_size(&dir.path().join("missing")), 0);
    }
}
