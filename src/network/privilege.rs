//! Privilege checks, escalation, fail-closed readiness and the root-only
//! apply/remove operations.

use std::ffi::OsString;
use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::firewall::{command_exists, detect_host_firewall};
use super::marker::{ReadyMarker, marker_path, ready_marker, write_marker};
use super::script::{render_command, run_script, setup_script, teardown_script};
use super::{DNS, MARKER_VERSION, SUBNET, TAP_POOL};
use crate::config::{Config, NetworkMode};

/// User that should own the TAP devices. `SUDO_USER` wins when hegn itself was
/// re-executed through sudo.
pub fn invoking_user() -> Option<String> {
    std::env::var("SUDO_USER")
        .ok()
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())
        .filter(|user| safe_user(user))
}

/// Apply the guard, re-executing hegn through sudo/pkexec when the current
/// process is not root. The caller is responsible for restoring any TUI
/// terminal first so the privilege helper can prompt.
pub fn setup_with_privilege(user: &str, endpoints: &[u16]) -> Result<()> {
    if !safe_user(user) {
        bail!("invalid user name for TAP ownership: {user:?}");
    }
    if is_privileged() {
        return setup(user, endpoints);
    }
    let exe = std::env::current_exe().context("failed to locate the hegn executable")?;
    let (helper, prefix) = if command_exists("sudo") {
        ("sudo", true)
    } else if command_exists("pkexec") {
        ("pkexec", false)
    } else {
        bail!(
            "guarded networking needs root, but neither sudo nor pkexec is installed; run `sudo \
             hegn network setup --user {user}` manually"
        );
    };
    let mut args: Vec<OsString> = Vec::new();
    if prefix {
        args.push("--".into());
    }
    args.push(exe.into_os_string());
    args.push("network".into());
    args.push("setup".into());
    args.push("--user".into());
    args.push(user.into());
    // The password prompt happens inside sudo/pkexec. Echo the exact command
    // first so the user can inspect what will run as root before typing it.
    eprintln!(
        "hegn: requesting root privileges; about to run:\n  {}",
        render_command(helper, &args)
    );
    let status = Command::new(helper)
        .args(&args)
        .status()
        .with_context(|| format!("failed to run {helper}"))?;
    if !status.success() {
        bail!("{helper} exited with {status}");
    }
    Ok(())
}

/// Fail closed when guarded networking is configured but not applied.
pub fn ensure_ready(config: &Config) -> Result<()> {
    if NetworkMode::Guarded != config.network {
        return Ok(());
    }
    let Some(marker) = ready_marker() else {
        bail!(
            "guarded networking is enabled but the host firewall is not active; run `sudo hegn \
             network setup` once (or set `network = \"off\"` for an air-gapped VM)"
        );
    };
    let expected = config.endpoint_ports();
    if marker.endpoints != expected {
        bail!(
            "local LLM endpoints changed (configured {:?}, firewall has {:?}); re-run `sudo hegn \
             network setup`",
            expected,
            marker.endpoints
        );
    }
    Ok(())
}

/// `true` when the process has the privileges required for setup/teardown.
pub fn is_privileged() -> bool {
    // Safety: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

pub fn safe_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 32
        && user
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Apply the host guard. Requires root; callers surface a clear error.
pub fn setup(user: &str, endpoints: &[u16]) -> Result<()> {
    if !is_privileged() {
        bail!("`hegn network setup` must run as root (try `sudo hegn network setup --user $USER`)");
    }
    let mut endpoints = endpoints.to_vec();
    endpoints.sort_unstable();
    endpoints.dedup();
    let script = setup_script(user, &endpoints)?;
    // The script replaces the nftables table. Drop the marker first so a
    // failed re-run cannot leave a "ready" marker vouching for a guard that no
    // longer exists; new instance starts then fail closed until setup succeeds.
    match fs::remove_file(marker_path()) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to clear {}", marker_path().display()));
        }
    }
    run_script(&script).context("failed to configure the guarded network")?;
    let marker = ReadyMarker {
        version: MARKER_VERSION,
        pool: TAP_POOL,
        subnet: SUBNET.to_string(),
        dns: DNS.iter().map(ToString::to_string).collect(),
        endpoints,
        // Detected after the script ran: it reports what the guard had to
        // coexist with, not what it hoped to find.
        host_firewall: detect_host_firewall(),
        applied_at: Utc::now(),
    };
    write_marker(marker_path(), &marker)
}

pub fn teardown() -> Result<()> {
    if !is_privileged() {
        bail!("`hegn network teardown` must run as root");
    }
    run_script(&teardown_script()).context("failed to tear down the guarded network")
}
