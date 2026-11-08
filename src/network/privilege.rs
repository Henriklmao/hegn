//! Privilege checks, escalation, fail-closed readiness and the root-only
//! apply/remove operations.

use std::ffi::OsString;
use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::firewall::{command_exists, detect_host_firewall};
use super::marker::{ReadyMarker, marker_path, ready_marker, write_marker};
use super::script::{reload_script, render_command, run_script, setup_script, teardown_script};
use super::{DNS, MARKER_VERSION, SUBNET, TAP_POOL};
use crate::config::{Config, NetworkMode};

/// User that should own the TAP devices. `SUDO_USER` wins when hegn itself was
/// re-executed through sudo.
pub fn invoking_user() -> Option<String> {
    ["SUDO_USER", "USER", "LOGNAME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|user| safe_user(user))
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
    // A root-owned receipt can outlive its rules; verify the kernel state when
    // this process is allowed to read it (root-only).
    if super::firewall::live_guard_status(&expected) == Some(false) {
        bail!(
            "the nftables guard is missing or incomplete; re-run `sudo hegn network setup --user \
             $USER`"
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
    clear_marker()?;
    run_script(&script).context("failed to configure the guarded network")?;
    // Do not write a readiness receipt unless the rules actually landed. A
    // host firewall or a concurrent `nft flush` must not leave a marker that
    // vouches for a table that does not exist.
    ensure_guard_live(&endpoints, "setup")?;
    record_ready(endpoints)
}

/// Reinstall the nftables guard without touching the TAP pool or the host
/// firewall exemptions. Used when only the endpoint list changed.
pub fn reload(endpoints: &[u16]) -> Result<()> {
    if !is_privileged() {
        bail!("`hegn network reload` must run as root (try `sudo hegn network reload`)");
    }
    if ready_marker().is_none() {
        bail!("guarded networking is not set up; run `sudo hegn network setup --user $USER` first");
    }
    let mut endpoints = endpoints.to_vec();
    endpoints.sort_unstable();
    endpoints.dedup();
    let script = reload_script(&endpoints)?;
    clear_marker()?;
    run_script(&script).context("failed to reload the guarded network")?;
    ensure_guard_live(&endpoints, "reload")?;
    record_ready(endpoints)
}

fn clear_marker() -> Result<()> {
    match fs::remove_file(marker_path()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("failed to clear {}", marker_path().display()))
        }
    }
}

fn ensure_guard_live(endpoints: &[u16], phase: &str) -> Result<()> {
    if super::firewall::live_guard_status(endpoints) == Some(false) {
        bail!(
            "the nftables guard is missing after {phase}; refusing to write {}",
            marker_path().display()
        );
    }
    Ok(())
}

fn record_ready(endpoints: Vec<u16>) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LocalEndpoint, NetworkMode};

    #[test]
    fn safe_user_accepts_normal_unix_account_names() {
        for user in [
            "alice",
            "Alice",
            "a",
            "_alice",
            "_",
            "alice-1",
            "alice.b",
            "systemd-network",
            "user_name",
            "a1._-",
        ] {
            assert!(safe_user(user), "{user:?} must be accepted");
        }
        let longest = "a".repeat(32);
        assert!(safe_user(&longest), "32 bytes is the documented maximum");
    }

    #[test]
    fn safe_user_rejects_empty_overlong_and_leading_characters() {
        for user in [
            "", "1alice", "-alice", "--force", ".alice", "..", " alice", "alice ", "\talice",
        ] {
            assert!(!safe_user(user), "{user:?} must be rejected");
        }
        let overlong = "a".repeat(33);
        assert!(!safe_user(&overlong), "33 bytes exceeds the limit");
    }

    #[test]
    fn safe_user_rejects_every_shell_metacharacter() {
        // `safe_user` is the only gate before a name is embedded verbatim in a
        // root shell script, so this list is deliberately hostile.
        for user in [
            "root; reboot",
            "root;reboot",
            "$(id)",
            "`id`",
            "${HOME}",
            "a b",
            "a\tb",
            "a\nb",
            "a&&b",
            "a|b",
            "a||b",
            "a>b",
            "a<b",
            "a'b",
            "a\"b",
            "a\\b",
            "a/b",
            "a*b",
            "a?b",
            "a[b]",
            "a{b}",
            "a(b)",
            "a~b",
            "a#b",
            "a!b",
            "a%b",
            "a@b",
            "a=b",
            "a:b",
            "a,b",
            "a+b",
            "a^b",
            "a$b",
            "a&b",
            "a;b",
            "üser",
            "aliceé",
        ] {
            assert!(!safe_user(user), "{user:?} must be rejected");
        }
    }

    #[test]
    fn is_privileged_matches_the_effective_uid() {
        // Safety: geteuid has no preconditions.
        let root = unsafe { libc::geteuid() == 0 };
        assert_eq!(is_privileged(), root);
    }

    #[test]
    fn invoking_user_never_returns_an_unsafe_name() {
        // Whatever SUDO_USER/USER/LOGNAME contains, the result must always pass
        // the same filter that guards the setup script.
        if let Some(user) = invoking_user() {
            assert!(!user.is_empty());
            assert!(safe_user(&user), "invoking_user returned {user:?}");
        }
    }

    #[test]
    fn ensure_ready_allows_air_gapped_networks_without_a_marker() {
        let config = Config {
            network: NetworkMode::Off,
            ..Config::default()
        };
        assert!(ensure_ready(&config).is_ok());
    }

    #[test]
    fn ensure_ready_fails_closed_for_guarded_networking() {
        let config = Config::default();
        assert_eq!(config.network, NetworkMode::Guarded);
        match ready_marker() {
            None => {
                let err = ensure_ready(&config).unwrap_err();
                let message = format!("{err:#}");
                assert!(
                    message.contains("host firewall is not active")
                        && message.contains("network setup"),
                    "unexpected error: {message}"
                );
            }
            Some(marker) => {
                // A marker exists on this host. A changed endpoint list must
                // still make readiness fail closed.
                let Some(port) = [65535u16, 65000, 64512]
                    .into_iter()
                    .find(|port| !marker.endpoints.contains(port))
                else {
                    return;
                };
                let mut config = config;
                config.local_endpoints = vec![LocalEndpoint {
                    name: "probe".to_string(),
                    port,
                }];
                let err = ensure_ready(&config).unwrap_err();
                assert!(
                    format!("{err:#}").contains("endpoints changed"),
                    "unexpected error: {err:#}"
                );
            }
        }
    }

    #[test]
    fn ensure_ready_accepts_the_endpoints_the_marker_vouched_for() {
        let Some(marker) = ready_marker() else {
            return;
        };
        if marker.endpoints.iter().any(|port| *port < 1024) {
            return;
        }
        let config = Config {
            local_endpoints: marker
                .endpoints
                .iter()
                .map(|port| LocalEndpoint {
                    name: format!("probe{port}"),
                    port: *port,
                })
                .collect(),
            ..Config::default()
        };
        assert!(ensure_ready(&config).is_ok());
    }

    #[test]
    fn setup_and_teardown_refuse_to_run_unprivileged() {
        if is_privileged() {
            // Never execute the apply/remove scripts from a test.
            return;
        }
        let setup_err = setup("alice", &[]).unwrap_err();
        assert!(
            format!("{setup_err:#}").contains("must run as root"),
            "{setup_err:#}"
        );
        let teardown_err = teardown().unwrap_err();
        assert!(
            format!("{teardown_err:#}").contains("must run as root"),
            "{teardown_err:#}"
        );
        let reload_err = reload(&[]).unwrap_err();
        assert!(
            format!("{reload_err:#}").contains("must run as root"),
            "{reload_err:#}"
        );
    }

    #[test]
    fn setup_with_privilege_rejects_unsafe_users_before_escalating() {
        for user in ["root; reboot", "$(id)", "", "-rf"] {
            let err = setup_with_privilege(user, &[]).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("invalid user name"), "{user:?}: {message}");
        }
    }
}
