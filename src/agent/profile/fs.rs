//! Private filesystem helpers for the per-VM profile bundle.
//!
//! These helpers never follow symlinks planted by a guest: secure directories
//! are recreated instead, and files are opened with `O_NOFOLLOW`.

use std::ffi::{CString, OsStr};
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// A directory descriptor used for descriptor-relative traversal.
///
/// Sync-back destinations must not be traversed by path: a guest-controlled
/// name could otherwise race a symlink replacement between `symlink_metadata`
/// and `create_dir_all`/`File::create`. Every operation below is relative to a
/// held descriptor and uses `O_NOFOLLOW`.
pub(super) struct DirFd {
    fd: OwnedFd,
    path: std::path::PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl DirFd {
    /// Open the configured source root with normal path resolution.
    ///
    /// The root itself may be a symlink; only descendants are no-follow.
    pub(super) fn open_root(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        Ok(Self {
            fd: file.into(),
            path: path.to_path_buf(),
        })
    }

    /// Open a guest-controlled root without following a replacement symlink.
    pub(super) fn open_source_root(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("failed to open source {}", path.display()))?;
        Ok(Self {
            fd: file.into(),
            path: path.to_path_buf(),
        })
    }

    pub(super) fn entries(&self) -> Result<Vec<std::ffi::OsString>> {
        let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", self.fd.as_raw_fd()));
        let mut names = Vec::new();
        for entry in fs::read_dir(&path)
            .with_context(|| format!("failed to read directory {}", self.path.display()))?
        {
            names.push(entry?.file_name());
        }
        names.sort();
        Ok(names)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn create_dir_at(&self, name: &OsStr) -> Result<Self> {
        let c_name = c_name(name)?;
        let rc = unsafe { libc::mkdirat(self.fd.as_raw_fd(), c_name.as_ptr(), 0o700) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(err).with_context(|| format!("failed to create {name:?}"));
            }
        }
        self.open_dir_at(name)
    }

    pub(super) fn open_dir_at(&self, name: &OsStr) -> Result<Self> {
        let c_name = c_name(name)?;
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to open directory {name:?}"));
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            path: self.path.join(name),
        })
    }

    pub(super) fn kind_at(&self, name: &OsStr) -> Result<Option<EntryKind>> {
        let c_name = c_name(name)?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                c_name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(err).with_context(|| format!("failed to inspect {name:?}"));
        }
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => EntryKind::File,
            libc::S_IFDIR => EntryKind::Dir,
            libc::S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        };
        Ok(Some(kind))
    }

    pub(super) fn open_file_read_at(&self, name: &OsStr) -> Result<fs::File> {
        let c_name = c_name(name)?;
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to open {name:?}"));
        }
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }

    pub(super) fn create_file_at(&self, name: &OsStr) -> Result<fs::File> {
        let c_name = c_name(name)?;
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to create {name:?}"));
        }
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }

    pub(super) fn rename_at(&self, from: &OsStr, to: &OsStr) -> Result<()> {
        let from = c_name(from)?;
        let to = c_name(to)?;
        let rc = unsafe {
            libc::renameat(
                self.fd.as_raw_fd(),
                from.as_ptr(),
                self.fd.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to replace destination");
        }
        Ok(())
    }

    pub(super) fn unlink_at(&self, name: &OsStr) -> Result<()> {
        let name = c_name(name)?;
        let rc = unsafe { libc::unlinkat(self.fd.as_raw_fd(), name.as_ptr(), 0) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to remove temporary file");
        }
        Ok(())
    }

    pub(super) fn sync(&self) -> Result<()> {
        let rc = unsafe { libc::fsync(self.fd.as_raw_fd()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to sync directory");
        }
        Ok(())
    }
}

fn c_name(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| anyhow::anyhow!("file name contains a NUL byte"))
}

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
/// per-VM bundle subdirectory with a symlink; following it on the host
/// would let the guest redirect writes outside the VM. Remove such a
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

/// Whether `error` means the current user may not read or delete a path.
///
/// hegn invoked through `sudo` (for example `sudo hegn vm apply`) writes the
/// per-VM bundle as root. A later user run owns the parent directory but can
/// neither read the root-owned directory nor rename it (`rename` must update
/// the directory's own `..` entry).
pub(super) fn owner_blocked(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|error| {
            matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
                || error.kind() == std::io::ErrorKind::PermissionDenied
        })
}

/// Recreate `path` as a private directory, repairing content a root-run hegn
/// left behind. Returns whether a repair was needed.
///
/// A root-owned directory cannot be read or renamed by a later user run, but
/// `rmdir` of an empty directory only needs write access to the parent, which
/// the user owns. Non-empty leftovers are impossible to fix without root, so
/// they fail with the exact command instead of a bare `Permission denied`.
pub(super) fn reset_dir(path: &Path) -> Result<bool> {
    match remove_path(path) {
        Ok(()) => {
            ensure_real_dir(path)?;
            Ok(false)
        }
        Err(error) if owner_blocked(&error) => {
            remove_empty_dir(path)?;
            ensure_real_dir(path)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// Ensure `path` is a private directory the current user controls.
///
/// Unlike [`reset_dir`] this keeps usable content; only a directory the current
/// user cannot access is replaced. Returns whether a repair was needed.
pub(super) fn ensure_usable_dir(path: &Path) -> Result<bool> {
    if let Err(error) = fs::read_dir(path)
        && error.kind() != std::io::ErrorKind::NotFound
        && owner_blocked(&anyhow::Error::new(error))
    {
        remove_empty_dir(path)?;
        ensure_real_dir(path)?;
        return Ok(true);
    }
    match ensure_real_dir(path) {
        Ok(()) => Ok(false),
        Err(error) if owner_blocked(&error) => {
            remove_empty_dir(path)?;
            ensure_real_dir(path)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

fn remove_empty_dir(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "cannot repair {} automatically; a root-run hegn left it root-owned and it is \
                 not empty. Run `sudo rm -rf {}` and start again",
                path.display(),
                crate::shared::text::shell_quote_always(&path.to_string_lossy())
            )
        }),
    }
}

/// Warning text for a directory that [`reset_dir`] or [`ensure_usable_dir`]
/// replaced because a root-run hegn created it.
pub(super) fn repaired_warning(path: &Path) -> String {
    format!(
        "replaced {} after a root-run hegn left it root-owned; its content was not readable \
         by this user",
        path.display()
    )
}

/// Hand a bundle written while running as root back to the invoking user.
///
/// Without this, a root-run `hegn vm apply` leaves root-owned directories and
/// files in the user's private VM state, which later user runs cannot read or
/// replace.
pub(super) fn adopt_invoking_owner(root: &Path) -> Result<()> {
    // Safety: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let Some(uid) = std::env::var("SUDO_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return Ok(());
    };
    let gid = std::env::var("SUDO_GID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    adopt_owner(root, uid, gid)
}

fn adopt_owner(root: &Path, uid: u32, gid: Option<u32>) -> Result<()> {
    // The bundle is guest-writable, so walk it iteratively: a poisoned guest
    // can nest directories deeply enough to overflow a recursive walk.
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.uid() != uid || gid.is_some_and(|gid| metadata.gid() != gid) {
            std::os::unix::fs::chown(&path, Some(uid), gid)
                .with_context(|| format!("failed to hand {} to user {uid}", path.display()))?;
        }
        if metadata.is_dir() {
            for entry in
                fs::read_dir(&path).with_context(|| format!("failed to read {}", path.display()))?
            {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

pub(super) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    crate::shared::fs::write_private_replacing(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))
}

pub(super) fn copy_file_private(source: &Path, dest: &Path, follow_symlinks: bool) -> Result<()> {
    if let Some(parent) = dest.parent() {
        secure_dir(parent)?;
    }
    let _ = fs::remove_file(dest);
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    let result = (|| -> Result<()> {
        let source_metadata = copy_source_to_file(source, &mut output, follow_symlinks)?;
        output.sync_all()?;
        // Keep executable skill helpers usable while never widening the owner
        // permissions (the profile directory itself is 0700).
        let mode = (source_metadata.permissions().mode() & 0o755) | 0o600;
        output
            .set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to set permissions on {}", dest.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

pub(super) fn copy_source_to_file(
    source: &Path,
    output: &mut fs::File,
    follow_symlinks: bool,
) -> Result<fs::Metadata> {
    let flags = if follow_symlinks {
        libc::O_CLOEXEC | libc::O_NONBLOCK
    } else {
        libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
    };
    let mut input = fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(source)
        .with_context(|| format!("failed to open profile source {}", source.display()))?;
    copy_open_source_to_file(&mut input, output)
}

pub(super) fn copy_open_source_to_file(
    input: &mut fs::File,
    output: &mut fs::File,
) -> Result<fs::Metadata> {
    let metadata = input
        .metadata()
        .context("failed to inspect profile source")?;
    if !metadata.is_file() {
        bail!("profile source is not a regular file");
    }
    std::io::copy(input, output)?;
    Ok(metadata)
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
    fn copy_file_private_rejects_symlinked_sources_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"payload").unwrap();
        let source = dir.path().join("source-link");
        std::os::unix::fs::symlink(&target, &source).unwrap();
        let dest = dir.path().join("dest");

        assert!(copy_file_private(&source, &dest, false).is_err());
        assert!(!dest.exists());

        copy_file_private(&source, &dest, true).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
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

        copy_file_private(&source, &dest, true).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"host");
        let metadata = fs::symlink_metadata(&dest).unwrap();
        assert!(metadata.is_file() && !metadata.file_type().is_symlink());
        assert_eq!(metadata.uid(), fs::metadata(&source).unwrap().uid());
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
        assert_eq!(metadata.permissions().mode() & 0o600, 0o600);
    }

    #[test]
    fn reset_dir_repairs_an_empty_directory_the_user_cannot_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("files");
        fs::create_dir(&path).unwrap();
        // An empty directory that cannot be opened makes `remove_dir_all` fail
        // with `EACCES`, while `rmdir` only needs write access to the parent.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o300)).unwrap();

        assert!(reset_dir(&path).unwrap());
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn reset_dir_removes_a_writable_directory_without_a_repair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("files");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("previous"), b"copied").unwrap();

        assert!(!reset_dir(&path).unwrap());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn reset_dir_names_the_manual_command_for_a_non_empty_leftover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("files");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("root-owned"), b"leftover").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();

        let err = format!("{:#}", reset_dir(&path).unwrap_err());
        assert!(err.contains("sudo rm -rf"), "{err}");
        assert!(path.is_dir());
    }

    #[test]
    fn ensure_usable_dir_repairs_an_unreadable_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills");
        fs::create_dir(&path).unwrap();
        // A root-owned 0700 directory is unreadable for the user who owns the
        // parent; write-only simulates that for the owning user.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o300)).unwrap();

        assert!(ensure_usable_dir(&path).unwrap());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn ensure_usable_dir_keeps_readable_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("guest"), b"keep").unwrap();

        assert!(!ensure_usable_dir(&path).unwrap());
        assert_eq!(fs::read(path.join("guest")).unwrap(), b"keep");
    }

    #[test]
    fn write_private_replaces_a_file_the_user_cannot_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.json");
        fs::write(&path, b"old").unwrap();
        // Owned by the user, but a root-run hegn left it 0400.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();

        write_private(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
