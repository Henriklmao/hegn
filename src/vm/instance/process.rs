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

pub fn process_alive(pid: u32) -> bool {
    // `kill(0, ...)` and `kill(-pgrp, ...)` target hegn's own process group or
    // an arbitrary group, so only real positive PIDs are considered.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // Safety: kill(2) with signal 0 only probes for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Whether `pid` is alive *and* still runs the expected executable.
///
/// PID files and `session.json` outlive crashes and reboots. Without this check
/// a recycled PID could make hegn report a stale VM as running, or worse, send
/// SIGKILL to an unrelated process.
pub fn process_matches(pid: u32, binary: &Path) -> bool {
    if !process_alive(pid) {
        return false;
    }
    let Some(expected) = binary.file_name() else {
        return false;
    };
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or(&[]);
    let argv0 = String::from_utf8_lossy(argv0);
    Path::new(argv0.as_ref())
        .file_name()
        .is_some_and(|actual| actual == expected)
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
}
