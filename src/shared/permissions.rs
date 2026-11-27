//! Ownership inspection and repair helpers.
//!
//! hegn can be invoked through `sudo` for network and doctor tasks. Anything
//! written during such a run belongs to root and blocks the next unprivileged
//! run, which owns the parent directory but cannot read or replace the
//! root-owned entries. These helpers identify that mismatch and hand the tree
//! back to the invoking user when the process can do so.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The user a hegn invocation should leave its files to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    /// `None` keeps the file's group instead of forcing a specific one.
    pub gid: Option<u32>,
}

/// The owner hegn should leave its state to.
///
/// When running as root through `sudo`, that is the invoking user from
/// `SUDO_UID`/`SUDO_GID`. Without those variables root keeps root ownership:
/// guessing a different user would be worse than the leftover.
pub fn invoking_owner() -> Owner {
    // Safety: geteuid/getegid have no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0
        && let Some(uid) = sudo_id("SUDO_UID")
    {
        return Owner {
            uid,
            gid: sudo_id("SUDO_GID"),
        };
    }
    Owner {
        uid: euid,
        gid: Some(unsafe { libc::getegid() }),
    }
}

fn sudo_id(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.parse::<u32>().ok()
}

/// Whether `metadata` belongs to a different user or group than `owner`.
pub fn is_foreign(metadata: &fs::Metadata, owner: Owner) -> bool {
    metadata.uid() != owner.uid || owner.gid.is_some_and(|gid| metadata.gid() != gid)
}

/// Hand `root` and everything below it to the invoking user.
///
/// A no-op unless the process runs as root through `sudo`; a plain root run
/// keeps root ownership instead of guessing a target user.
pub fn adopt_invoking_owner(root: &Path) -> io::Result<()> {
    // Safety: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let owner = invoking_owner();
    if owner.uid == 0 {
        return Ok(());
    }
    adopt_owner(root, owner)
}

/// Recursively `chown` `root` and its descendants to `owner`.
///
/// The walk is descriptor-relative: every directory is opened with
/// `O_DIRECTORY | O_NOFOLLOW`, every entry is inspected with
/// `fstatat(AT_SYMLINK_NOFOLLOW)` and every change uses `fchownat` with the same
/// flag. A guest that swaps a directory for a symlink between two calls cannot
/// redirect the root chown outside the tree. Symlinks themselves are skipped.
pub fn adopt_owner(root: &Path, owner: Owner) -> io::Result<()> {
    // The doctor also hands back single files (owner files, seed generations).
    // `O_DIRECTORY` fails with ENOTDIR for those; fall back to a
    // descriptor-relative chown of the entry itself.
    let root_fd = match open_dir_nofollow(root) {
        Ok(fd) => fd,
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
            return adopt_file(root, owner);
        }
        Err(error) => return Err(hand_error(root, error)),
    };
    let mut pending: Vec<(OwnedFd, PathBuf)> = vec![(root_fd, root.to_path_buf())];
    while let Some((fd, path)) = pending.pop() {
        let metadata = fstat(fd.as_raw_fd()).map_err(|error| hand_error(&path, error))?;
        if needs_hand(&metadata, owner) {
            fchown(fd.as_raw_fd(), owner).map_err(|error| hand_error(&path, error))?;
        }
        let names = entry_names(fd.as_raw_fd()).map_err(|error| hand_error(&path, error))?;
        for name in names {
            let c_name = CString::new(name.as_os_str().as_bytes()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "file name contains a NUL byte")
            })?;
            let child = path.join(&name);
            let metadata =
                fstatat(fd.as_raw_fd(), &c_name).map_err(|error| hand_error(&child, error))?;
            match metadata.st_mode & libc::S_IFMT {
                libc::S_IFLNK => {}
                libc::S_IFDIR => {
                    let child_fd = open_dir_at(fd.as_raw_fd(), &c_name)
                        .map_err(|error| hand_error(&child, error))?;
                    pending.push((child_fd, child));
                }
                _ if needs_hand(&metadata, owner) => {
                    fchownat(fd.as_raw_fd(), &c_name, owner)
                        .map_err(|error| hand_error(&child, error))?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Hand a single non-directory entry to `owner` without following a symlink.
fn adopt_file(path: &Path, owner: Owner) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let parent_fd = open_dir_nofollow(parent).map_err(|error| hand_error(parent, error))?;
    let c_name = CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "file name contains a NUL byte")
    })?;
    let metadata =
        fstatat(parent_fd.as_raw_fd(), &c_name).map_err(|error| hand_error(path, error))?;
    if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing symlinked path {}", path.display()),
        ));
    }
    if needs_hand(&metadata, owner) {
        fchownat(parent_fd.as_raw_fd(), &c_name, owner).map_err(|error| hand_error(path, error))?;
    }
    Ok(())
}

fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    Ok(file.into())
}

fn open_dir_at(parent: RawFd, name: &CString) -> io::Result<OwnedFd> {
    // Safety: `parent` is an open directory descriptor, `name` is a valid
    // NUL-terminated entry name, and the returned descriptor is owned here.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // Safety: `fd` is an open descriptor and `stat` is writable.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

fn fstatat(parent: RawFd, name: &CString) -> io::Result<libc::stat> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // Safety: as in `fstat`, relative to the held directory descriptor.
    if unsafe { libc::fstatat(parent, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

fn fchown(fd: RawFd, owner: Owner) -> io::Result<()> {
    // Safety: `fd` is an open descriptor; `-1` keeps the current group.
    if unsafe { libc::fchown(fd, owner.uid, owner.gid.unwrap_or(!0)) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fchownat(parent: RawFd, name: &CString, owner: Owner) -> io::Result<()> {
    // Safety: as above, resolved relative to the held directory and never
    // through a symlink.
    if unsafe {
        libc::fchownat(
            parent,
            name.as_ptr(),
            owner.uid,
            owner.gid.unwrap_or(!0),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn entry_names(fd: RawFd) -> io::Result<Vec<std::ffi::OsString>> {
    let path = PathBuf::from(format!("/proc/self/fd/{fd}"));
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        names.push(entry?.file_name());
    }
    names.sort();
    Ok(names)
}

fn needs_hand(metadata: &libc::stat, owner: Owner) -> bool {
    metadata.st_uid != owner.uid || owner.gid.is_some_and(|gid| metadata.st_gid != gid)
}

fn hand_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "failed to hand {} to the invoking user: {error}",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn invoking_owner_matches_the_effective_user_without_sudo() {
        let owner = invoking_owner();
        // Safety: geteuid has no preconditions and cannot fail.
        assert_eq!(owner.uid, unsafe { libc::geteuid() });
    }

    #[test]
    fn is_foreign_compares_uid_and_optional_gid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"x").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let own = Owner {
            uid: metadata.uid(),
            gid: Some(metadata.gid()),
        };
        assert!(!is_foreign(&metadata, own));
        assert!(is_foreign(
            &metadata,
            Owner {
                uid: metadata.uid() + 1,
                gid: Some(metadata.gid()),
            }
        ));
        assert!(is_foreign(
            &metadata,
            Owner {
                uid: metadata.uid(),
                gid: Some(metadata.gid() + 1),
            }
        ));
        assert!(!is_foreign(
            &metadata,
            Owner {
                uid: metadata.uid(),
                gid: None,
            }
        ));
    }

    #[test]
    fn adopt_owner_handles_a_single_file_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seed-generation");
        fs::write(&path, b"5\n").unwrap();
        let owner = Owner {
            uid: unsafe { libc::geteuid() },
            gid: Some(unsafe { libc::getegid() }),
        };

        adopt_owner(&path, owner).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"5\n");
    }

    #[test]
    fn adopt_owner_refuses_a_symlinked_root() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let owner = Owner {
            uid: unsafe { libc::geteuid() },
            gid: Some(unsafe { libc::getegid() }),
        };

        assert!(adopt_owner(&link, owner).is_err());
    }

    #[test]
    fn adopt_owner_walks_regular_entries_and_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"x").unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/file"), b"x").unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let owner = Owner {
            uid: unsafe { libc::geteuid() },
            gid: Some(unsafe { libc::getegid() }),
        };
        adopt_owner(&root, owner).unwrap();

        // The current user already owns the tree, so this is a structural test:
        // every regular entry stays private and the symlink target is untouched.
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        assert_eq!(
            fs::metadata(root.join("file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }
}
