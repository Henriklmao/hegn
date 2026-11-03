//! Process inspection helpers.
//!
//! PID files and session records outlive crashes and reboots. Callers therefore
//! only signal a process after its identity has been verified against the
//! expected executable.

use std::fs;
use std::path::Path;

/// Whether a positive PID is alive.
///
/// `kill(0, ...)` and `kill(-pgrp, ...)` target hegn's own process group or an
/// arbitrary group, so only real positive PIDs are considered.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // Safety: kill(2) with signal 0 only probes for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Whether `pid` is alive *and* still runs the expected executable.
///
/// An absolute `binary` is compared against `/proc/<pid>/exe`, which survives
/// argv[0] spoofing and catches a recycled PID running a different program with
/// the same file name. When the kernel does not expose `/proc/<pid>/exe` (for
/// example due to permissions), or the expected path no longer exists, the
/// function falls back to comparing the argv[0] basename.
pub fn executable_matches(pid: u32, binary: &Path) -> bool {
    if !process_alive(pid) {
        return false;
    }
    let Some(expected_name) = binary.file_name() else {
        return false;
    };

    if binary.is_absolute()
        && let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe"))
        && let (Ok(actual), Ok(expected)) = (exe.canonicalize(), binary.canonicalize())
    {
        return actual == expected;
    }

    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or(&[]);
    let argv0 = String::from_utf8_lossy(argv0);
    Path::new(argv0.as_ref())
        .file_name()
        .is_some_and(|actual| actual == expected_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn process_alive_detects_self_and_rejects_invalid_pids() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0));
        assert!(!process_alive(4_000_000_000));
        assert!(!process_alive(u32::MAX));
        assert!(!process_alive(i32::MAX as u32 + 1));
    }

    #[test]
    fn executable_matches_accepts_the_test_binary() {
        let exe = std::env::current_exe().unwrap();
        assert!(executable_matches(std::process::id(), &exe));
        assert!(!executable_matches(
            std::process::id(),
            Path::new("/definitely/not/this-binary")
        ));
        assert!(!executable_matches(0, &exe));
    }

    #[test]
    fn executable_matches_rejects_paths_without_a_file_name() {
        let exe = std::env::current_exe().unwrap();
        assert!(!executable_matches(std::process::id(), Path::new("/")));
        assert!(!executable_matches(std::process::id(), Path::new("")));
        assert!(executable_matches(std::process::id(), &exe));
    }

    #[test]
    fn executable_matches_rejects_a_recycled_pid_with_the_same_basename() {
        // Use the test binary's file name for a different executable. Basename
        // comparison alone would accept it; /proc/<pid>/exe must not.
        let expected = std::env::current_exe().unwrap();
        let name = expected.file_name().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let impostor: PathBuf = dir.path().join(name);
        let source = Path::new("/bin/sleep");
        if !source.is_file() {
            return;
        }
        fs::copy(source, &impostor).unwrap();
        let mut child = Command::new(&impostor).arg("5").spawn().unwrap();
        assert!(!executable_matches(child.id(), &expected));
        let _ = child.kill();
        let _ = child.wait();
    }
}
