//! Process and socket helpers.
//!
//! PID files and `session.json` outlive crashes and reboots, so termination is
//! only ever attempted after the process identity has been verified. Socket
//! cleanup similarly refuses to remove anything that is not a socket.

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::session::relay;
use crate::shared::process::executable_matches;
pub use crate::shared::process::process_alive;

/// Whether `pid` is alive *and* still runs the expected executable.
///
/// PID files and `session.json` outlive crashes and reboots. Without this check
/// a recycled PID could make hegn report a stale VM as running, or worse, send
/// SIGKILL to an unrelated process. Absolute paths are compared against
/// `/proc/<pid>/exe`; argv[0] is only a fallback.
pub fn process_matches(pid: u32, binary: &Path) -> bool {
    executable_matches(pid, binary)
}

pub(super) fn terminate(pid: u32, binary: &Path) {
    if !process_matches(pid, binary) {
        return;
    }
    signal_and_wait(pid, || process_matches(pid, binary));
}

pub(super) fn terminate_relay(pid: u32, binary: &Path) {
    if !relay::process_matches_relay(pid, binary) {
        return;
    }
    signal_and_wait(pid, || relay::process_matches_relay(pid, binary));
}

pub(super) fn terminate_autosave(pid: u32, binary: &Path) {
    if !super::autosave::process_matches(pid, binary) {
        return;
    }
    signal_and_wait(pid, || super::autosave::process_matches(pid, binary));
}

fn signal_and_wait(pid: u32, still_running: impl Fn() -> bool) {
    // Safety: the pid was verified to belong to the expected executable.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !still_running() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !still_running() {
        return;
    }
    // Safety: same verified process as above; escalate after the grace period.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

pub(super) fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub(super) fn hegn_binary() -> Result<PathBuf> {
    std::env::current_exe().context("failed to locate the hegn executable")
}

pub(super) fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_socket())
        .unwrap_or(false)
}

/// Remove a socket or stale socket file, but never a regular file or symlink.
pub(super) fn remove_socket(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path)
            .with_context(|| format!("failed to remove stale socket {}", path.display())),
        Ok(_) => bail!(
            "refusing to replace {}: it exists and is not a socket",
            path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

pub(super) fn ensure_socket(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!(
            "no Cloud Hypervisor API socket at {} — start an instance first",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_alive_detects_self() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(4_000_000_000));
    }

    #[test]
    fn process_alive_rejects_pid_zero_and_out_of_range() {
        // `kill(0, 0)` probes hegn's own process group and would otherwise
        // report true (and allow terminate(0) to signal our own group).
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
        assert!(!process_alive(i32::MAX as u32 + 1));
    }

    #[test]
    fn process_matches_verifies_the_executable() {
        let exe = std::env::current_exe().unwrap();
        assert!(process_matches(std::process::id(), &exe));
        assert!(!process_matches(
            std::process::id(),
            Path::new("/definitely/not/this-binary")
        ));
        assert!(!process_matches(0, &exe));
    }

    #[test]
    fn socket_cleanup_refuses_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-hypervisor.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        let err = remove_socket(&path).unwrap_err().to_string();
        assert!(err.contains("not a socket"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    }

    #[test]
    fn read_pid_handles_missing_garbage_and_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pid(&dir.path().join("missing.pid")), None);

        let garbage = dir.path().join("garbage.pid");
        std::fs::write(&garbage, b"not-a-pid\n").unwrap();
        assert_eq!(read_pid(&garbage), None);

        let signed = dir.path().join("signed.pid");
        std::fs::write(&signed, b"-1\n").unwrap();
        assert_eq!(read_pid(&signed), None);

        let valid = dir.path().join("valid.pid");
        std::fs::write(&valid, b"  4321\n").unwrap();
        assert_eq!(read_pid(&valid), Some(4321));
    }

    #[test]
    fn remove_socket_removes_a_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_socket(&path));
        remove_socket(&path).unwrap();
        assert!(!path.exists());
        drop(listener);
    }

    #[test]
    fn remove_socket_refuses_symlinks_and_leaves_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"do not delete").unwrap();
        let link = dir.path().join("api.sock");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = remove_socket(&link).unwrap_err().to_string();
        assert!(err.contains("not a socket"), "{err}");
        assert!(link.is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"do not delete");
    }

    #[test]
    fn is_socket_distinguishes_sockets_from_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("regular");
        std::fs::write(&regular, b"x").unwrap();
        assert!(!is_socket(&regular));
        assert!(!is_socket(&dir.path().join("missing")));

        let socket = dir.path().join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(is_socket(&socket));
    }

    #[test]
    fn process_matches_rejects_paths_without_a_binary_name() {
        let exe = std::env::current_exe().unwrap();
        assert!(!process_matches(std::process::id(), Path::new("/")));
        assert!(!process_matches(std::process::id(), Path::new("")));
        assert!(process_matches(std::process::id(), &exe));
    }

    #[test]
    fn ensure_socket_reports_a_missing_api_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-hypervisor.sock");
        let err = ensure_socket(&path).unwrap_err().to_string();
        assert!(err.contains("no Cloud Hypervisor API socket"), "{err}");
        assert!(err.contains(&path.display().to_string()), "{err}");
        std::fs::write(&path, b"x").unwrap();
        assert!(ensure_socket(&path).is_ok());
    }
}
