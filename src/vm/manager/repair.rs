//! Automatic repair for run directories whose session metadata is unreadable.
//!
//! An unreadable `session.json` usually means an older hegn version crashed
//! before cleaning up, or the file was truncated. The previous policy was to
//! stop with a manual-repair instruction, which blocks a perfectly restartable
//! VM. We instead:
//!
//! 1. verify that every recorded process is gone (or terminate it after an
//!    identity check),
//! 2. move the stale runtime files into `run/stale-<timestamp>/` (never delete
//!    diagnostics), and
//! 3. release the network lease.
//!
//! The lifecycle lock and log files stay in place, so the caller can continue
//! starting the VM without re-acquiring a lock in a renamed directory.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::config::Config;
use crate::network;
use crate::session::relay;
use crate::vm::config::VmConfig;

use super::autosave;
use super::cleanup::indexed_pids;
use super::paths::VmPaths;
use super::process::{
    hegn_binary, process_alive, process_matches, read_pid, terminate, terminate_autosave,
    terminate_relay,
};

/// Archive and clear an unreadable run directory while the caller holds the
/// lifecycle lock.
///
/// Returns the archive path when a repair happened. Missing run state is not an
/// error: a fresh VM simply has nothing to repair.
pub(super) fn repair_unreadable_session_locked(
    config: &Config,
    vm: &VmConfig,
    paths: &VmPaths,
) -> Result<Option<PathBuf>> {
    if !paths.session.exists() {
        return Ok(None);
    }
    if fs::symlink_metadata(&paths.run_dir)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        bail!(
            "refusing to repair symlinked run directory {}",
            paths.run_dir.display()
        );
    }

    let mut pids = BTreeSet::new();
    for path in [
        &paths.vm_pid,
        &paths.relay_pid,
        &paths.profile_virtiofs_pid,
        &paths.autosave_pid,
    ] {
        if let Some(pid) = read_pid(path) {
            pids.insert(pid);
        }
    }
    pids.extend(indexed_pids(paths).into_iter().map(|(_, pid)| pid));

    for pid in pids {
        stop_recorded_process(config, vm, paths, pid)?;
    }
    if let Some(pid) = unrelated_process_using(&paths.run_dir)? {
        bail!(
            "process {pid} is still using {}; stop it before hegn repairs this VM",
            paths.run_dir.display()
        );
    }

    let archive = archive_runtime_state(paths)?;
    network::release(config, &vm.name);
    Ok(Some(archive))
}

/// Terminate a PID from a stale run directory, but only when its command line
/// proves that it belongs to this VM.
fn stop_recorded_process(config: &Config, vm: &VmConfig, paths: &VmPaths, pid: u32) -> Result<()> {
    if !process_alive(pid) {
        return Ok(());
    }
    let args = cmdline_args(pid).with_context(|| {
        format!(
            "failed to inspect process {pid} while repairing VM '{}'",
            vm.name
        )
    })?;
    let run_dir = paths.run_dir.to_string_lossy();
    let uses_run_dir = args.iter().any(|arg| arg.contains(run_dir.as_ref()));
    let autosave_for_vm = args.iter().any(|arg| arg == autosave::INTERNAL_ARG)
        && args
            .windows(2)
            .any(|pair| (pair[0] == "--vm" || pair[0] == "--instance") && pair[1] == vm.name);
    if !uses_run_dir && !autosave_for_vm {
        bail!(
            "PID file in {} points at process {pid}, which does not belong to VM '{}'; \
             refusing to repair automatically",
            paths.run_dir.display(),
            vm.name
        );
    }

    let hegn = hegn_binary()?;
    let vm_binary = config.vm_binary();
    if process_matches(pid, &vm_binary) {
        terminate(pid, &vm_binary);
    } else if process_matches(pid, &config.virtiofsd_bin) {
        terminate(pid, &config.virtiofsd_bin);
    } else if relay::process_matches_relay(pid, &hegn) {
        terminate_relay(pid, &hegn);
    } else if autosave::process_matches(pid, &hegn) {
        terminate_autosave(pid, &hegn);
    } else {
        bail!(
            "process {pid} is using {} but is not a hegn or Cloud Hypervisor process; \
             stop it manually",
            paths.run_dir.display()
        );
    }
    if process_alive(pid) {
        bail!(
            "process {pid} still runs after termination; stop it manually before starting VM '{}'",
            vm.name
        );
    }
    Ok(())
}

/// Find a process that still references the run directory but was not recorded
/// in a pid file. We do not kill unknown processes automatically.
fn unrelated_process_using(run_dir: &Path) -> Result<Option<u32>> {
    let needle = run_dir.to_string_lossy();
    let self_pid = std::process::id();
    let entries = fs::read_dir("/proc").context("failed to scan /proc")?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(args) = cmdline_args(pid) else {
            continue;
        };
        if args.iter().any(|arg| arg.contains(needle.as_ref())) {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

fn cmdline_args(pid: u32) -> Result<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline"))
        .with_context(|| format!("failed to read /proc/{pid}/cmdline"))?;
    Ok(raw
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect())
}

/// Move runtime state into a private archive subdirectory. Logs and the
/// lifecycle lock stay in place.
fn archive_runtime_state(paths: &VmPaths) -> Result<PathBuf> {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let mut archive = paths.run_dir.join(format!("stale-{stamp}"));
    let mut suffix = 1;
    while archive.exists() {
        archive = paths.run_dir.join(format!("stale-{stamp}-{suffix}"));
        suffix += 1;
    }
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(&archive)
        .with_context(|| format!("failed to create {}", archive.display()))?;

    let entries = fs::read_dir(&paths.run_dir)
        .with_context(|| format!("failed to read {}", paths.run_dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "hegn.lock" || name_str.ends_with(".log") || name_str.starts_with("stale-") {
            continue;
        }
        let from = entry.path();
        let to = archive.join(&name);
        fs::rename(&from, &to)
            .with_context(|| format!("failed to archive {} to {}", from.display(), to.display()))?;
    }
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::manager::VmManager;
    use crate::vm::manager::test_support::{config_with_assets, manager};

    fn fixture() -> (tempfile::TempDir, VmManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        manager.paths().ensure().unwrap();
        (dir, manager)
    }

    #[test]
    fn archives_unreadable_state_and_keeps_logs_and_lock() {
        let (_dir, manager) = fixture();
        let paths = manager.paths();
        fs::write(&paths.session, b"{ not json").unwrap();
        fs::write(&paths.membership, b"stale").unwrap();
        fs::write(&paths.vm_pid, b"999999\n").unwrap();
        fs::write(&paths.log, b"vmm log\n").unwrap();
        fs::write(&paths.lock, b"lock\n").unwrap();

        let archive = repair_unreadable_session_locked(manager.config(), manager.vm(), &paths)
            .unwrap()
            .expect("repair");

        assert!(!paths.session.exists());
        assert!(!paths.membership.exists());
        assert_eq!(fs::read(&paths.log).unwrap(), b"vmm log\n");
        assert!(paths.lock.exists());
        assert!(archive.is_dir());
        assert_eq!(
            fs::read(archive.join("session.json")).unwrap(),
            b"{ not json"
        );
    }

    #[test]
    fn refuses_to_kill_an_unrelated_live_pid() {
        let (_dir, manager) = fixture();
        let paths = manager.paths();
        fs::write(&paths.session, b"{ not json").unwrap();
        fs::write(&paths.vm_pid, format!("{}\n", std::process::id())).unwrap();

        let err = repair_unreadable_session_locked(manager.config(), manager.vm(), &paths)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not belong"), "{err}");
        assert!(paths.session.exists());
    }

    #[test]
    fn missing_session_is_not_a_repair() {
        let (_dir, manager) = fixture();
        assert!(
            repair_unreadable_session_locked(manager.config(), manager.vm(), &manager.paths(),)
                .unwrap()
                .is_none()
        );
    }
}
