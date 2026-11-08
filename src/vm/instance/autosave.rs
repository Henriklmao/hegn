//! Detached periodic snapshot worker.
//!
//! A running VM is checkpointed every `snapshot_interval_secs` so a later
//! `start` can restore the newest checkpoint instead of cold booting. The
//! worker is a small `hegn __autosnapshot --instance <name>` process: like the
//! console relay it outlives the CLI/TUI that spawned it and stops as soon as
//! the VM is gone.
//!
//! Only snapshots tagged [`AUTOSAVE_NOTE`] are eligible for retention pruning
//! and automatic restore. Manual checkpoints are never touched.
//!
//! Snapshot restore rewinds guest RAM/CPU but not the writable rootfs, so an
//! automatic checkpoint is a *best-effort* fast-resume aid, not a crash-
//! consistent disk backup.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::session::snapshot::{AUTOSAVE_NOTE, SnapshotStore};
use crate::shared::process::executable_matches;
use crate::vm::hypervisor::VmState;
use crate::vm::instance_config::InstanceConfig;

use super::paths::InstancePaths;
use super::process::read_pid;
use super::registry::InstanceRegistry;
use super::util::{append_event_log, write_private};

/// Hidden first argument used to start the worker from [`spawn`].
pub const INTERNAL_ARG: &str = "__autosnapshot";

/// Prefix of every automatic snapshot name.
const NAME_PREFIX: &str = "autosave";

/// Is this process the internal automatic snapshot worker?
pub fn requested() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == INTERNAL_ARG)
}

/// Entry point for the detached worker, called from `main`.
pub fn run_from_current_args() -> Result<()> {
    let instance =
        parse_args(std::env::args_os().skip(2)).context("invalid automatic snapshot arguments")?;
    let config = Config::load()?;
    run_worker(&config, &instance)
}

fn parse_args(args: impl Iterator<Item = std::ffi::OsString>) -> Result<String> {
    let mut instance = None;
    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--instance") => {
                instance = Some(
                    args.next()
                        .context("missing value for --instance")?
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("instance name is not valid UTF-8"))?,
                );
            }
            _ => bail!("unknown autosnapshot argument {flag:?}"),
        }
    }
    let instance = instance.context("missing --instance")?;
    if instance.is_empty() {
        bail!("instance name must not be empty");
    }
    Ok(instance)
}

/// Does `pid` still run the automatic snapshot worker?
pub fn process_matches(pid: u32, binary: &Path) -> bool {
    if !executable_matches(pid, binary) {
        return false;
    }
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    cmdline
        .split(|byte| *byte == 0)
        .any(|arg| arg == INTERNAL_ARG.as_bytes())
}

/// Start the worker for a VM that is already running.
///
/// Returns the existing pid when a matching worker is still alive. A stale pid
/// file is replaced. The child is left detached; a reaper thread avoids a
/// zombie while the parent (usually the TUI) is alive.
pub(crate) fn spawn(
    config: &Config,
    instance: &InstanceConfig,
    paths: &InstancePaths,
) -> Result<u32> {
    if !config.periodic_snapshots {
        return Ok(0);
    }
    let exe = std::env::current_exe().context("failed to locate the hegn executable")?;
    if let Some(pid) = read_pid(&paths.autosave_pid)
        && process_matches(pid, &exe)
    {
        return Ok(pid);
    }
    let stderr = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&paths.autosave_log)
        .with_context(|| format!("failed to open {}", paths.autosave_log.display()))?;
    let mut child = Command::new(&exe)
        .arg(INTERNAL_ARG)
        .arg("--instance")
        .arg(&instance.name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed to spawn the automatic snapshot worker")?;
    let pid = child.id();
    if let Err(err) = write_private(&paths.autosave_pid, pid.to_string()) {
        // Without the pid file the worker cannot be stopped or replaced by
        // identity, so do not leak it.
        let _ = child.kill();
        let _ = child.wait();
        return Err(err).context("failed to record the automatic snapshot pid");
    }
    reaper(child);
    Ok(pid)
}

fn reaper(mut child: Child) {
    let _ = std::thread::Builder::new()
        .name("hegn-autosave-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// Loop until the VM disappears, checkpointing it every interval.
fn run_worker(config: &Config, instance: &str) -> Result<()> {
    let interval = Duration::from_secs(config.snapshot_interval_secs.max(30));
    let keep = config.snapshot_keep.max(1);
    let registry = InstanceRegistry::new(config.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build the autosnapshot runtime")?;

    loop {
        std::thread::sleep(interval);
        let manager = match registry.manager_by_name(instance) {
            Ok(manager) => manager,
            // The instance was deleted while the VM was running.
            Err(_) => return Ok(()),
        };
        if !manager.is_running() {
            return Ok(());
        }
        // Never turn a deliberate pause into a resume. Only a running guest is
        // a suitable checkpoint.
        match runtime.block_on(manager.instance_info()) {
            Ok(info) if info.state == VmState::Running => {}
            Ok(_) => continue,
            Err(_) => return Ok(()),
        }
        let name = suggested_name();
        match runtime.block_on(manager.create_snapshot_tagged(&name, Some(AUTOSAVE_NOTE))) {
            Ok(_) => {
                if let Err(err) =
                    SnapshotStore::new(&config.snapshots_dir).prune_autosaves(instance, keep)
                {
                    append_event_log(
                        &manager.paths().event_log,
                        &format!("autosnapshot: could not prune old checkpoints: {err:#}"),
                    );
                }
            }
            Err(err) => {
                append_event_log(
                    &manager.paths().event_log,
                    &format!("autosnapshot: snapshot failed: {err:#}"),
                );
                // A stopped VMM will never recover on its own; leave instead
                // of retrying against a dead socket forever.
                if !manager.is_running() {
                    return Ok(());
                }
            }
        }
    }
}

/// Name for a fresh automatic snapshot. The timestamp also sorts correctly.
pub(crate) fn suggested_name() -> String {
    SnapshotStore::suggested_name(NAME_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_from(values: &[&str]) -> Result<String> {
        parse_args(values.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn parses_the_instance_argument() {
        assert_eq!(args_from(&["--instance", "demo"]).unwrap(), "demo");
        assert!(args_from(&[]).is_err());
        assert!(args_from(&["--instance"]).is_err());
        assert!(args_from(&["--instance", ""]).is_err());
        assert!(args_from(&["--unknown", "demo"]).is_err());
    }

    #[test]
    fn process_match_requires_the_internal_argument() {
        let exe = std::env::current_exe().unwrap();
        assert!(!process_matches(std::process::id(), &exe));
        assert!(!process_matches(4_000_000_000, &exe));
    }

    #[test]
    fn names_are_sanitized_and_prefixed() {
        let name = suggested_name();
        assert!(name.starts_with("autosave-"), "{name}");
        crate::session::snapshot::sanitize_name(&name).unwrap();
    }

    #[test]
    fn disabled_periodic_snapshots_do_not_spawn_a_worker() {
        use super::super::test_support::{config_with_assets, instance_for};

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.periodic_snapshots = false;
        let instance = instance_for(&dir.path().join("project"));
        let paths = InstancePaths::from_instance(&config, &instance);
        assert_eq!(spawn(&config, &instance, &paths).unwrap(), 0);
        assert!(!paths.autosave_pid.exists());
    }
}
