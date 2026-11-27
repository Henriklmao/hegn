//! Privilege checks, escalation, fail-closed readiness and the root-only
//! apply/remove operations.

use std::ffi::OsString;
use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::firewall::{SAFE_PATH, command_exists, detect_host_firewall};
use super::marker::{
    ReadyMarker, control_taps_present, data_taps_present, marker_path, ready_marker, write_marker,
};
use super::script::{reload_script, render_command, run_script, setup_script, teardown_script};
use super::{CONTROL_SUBNET, CONTROL_TAP_POOL, DNS, MARKER_VERSION, SUBNET, TAP_POOL};
use crate::config::{Config, NetworkMode};

/// User that should own the TAP devices. `SUDO_USER` wins when hegn itself was
/// re-executed through sudo.
pub fn invoking_user() -> Option<String> {
    ["SUDO_USER", "USER", "LOGNAME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|user| safe_user(user))
}

/// Privileged helpers to try, most preferred first.
///
/// `pkexec` wins when it is installed: it authorizes through polkit and needs
/// no sudoers entry. Both helpers are optional runtime dependencies, so a
/// missing one is skipped and a direct root run never reaches this list. The
/// `bool` marks helpers that take a leading `--` before the command.
pub(crate) fn privilege_helpers() -> Vec<(&'static str, bool)> {
    helpers_from(command_exists("pkexec"), command_exists("sudo"))
}

fn helpers_from(pkexec: bool, sudo: bool) -> Vec<(&'static str, bool)> {
    let mut helpers = Vec::new();
    if pkexec {
        helpers.push(("pkexec", false));
    }
    if sudo {
        helpers.push(("sudo", true));
    }
    helpers
}

/// Apply the guard, re-executing hegn through pkexec/sudo when the current
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
    let helpers = privilege_helpers();
    if helpers.is_empty() {
        bail!(
            "guarded networking needs root, but neither pkexec nor sudo is installed; run `sudo \
             hegn network setup --user {user}` manually"
        );
    }
    let mut failures = Vec::new();
    for (helper, prefix) in helpers {
        // Resolve the helper itself through the fixed path; the caller's PATH
        // must not be able to substitute a different `pkexec`/`sudo`.
        let helper_path = super::firewall::safe_command_path(helper).with_context(|| {
            format!("failed to locate {helper} in the fixed root path {SAFE_PATH}")
        })?;
        let mut args: Vec<OsString> = Vec::new();
        if prefix {
            args.push("--".into());
        }
        args.push(exe.clone().into_os_string());
        args.push("network".into());
        args.push("setup".into());
        args.push("--user".into());
        args.push(user.into());
        // The prompt happens inside the helper. Echo the exact command first
        // so the user can inspect what will run as root before approving it.
        eprintln!(
            "hegn: requesting root privileges; about to run:\n  {}",
            render_command(helper, &args)
        );
        match Command::new(&helper_path)
            .args(&args)
            // The escalated process inherits this PATH; pin it rather than
            // passing the caller's environment into a root command.
            .env("PATH", SAFE_PATH)
            .status()
        {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => failures.push(format!("{helper} exited with {status}")),
            Err(err) => failures.push(format!("failed to run {helper}: {err}")),
        }
    }
    bail!(
        "{}; run `sudo hegn network setup --user {user}` manually",
        failures.join("; ")
    )
}

/// Fail closed when the configured networking has not been applied.
///
/// Both modes need the one-time privileged setup: guarded guests need the
/// egress guard, while air-gapped guests still need their control TAPs for SSH.
/// There is no mode that silently skips host preparation.
pub fn ensure_ready(config: &Config) -> Result<()> {
    let endpoints = config.endpoint_ports();
    ensure_ready_with(
        config.network,
        &endpoints,
        ready_marker().as_ref(),
        control_taps_present(),
        data_taps_present(),
        super::firewall::live_guard_status(&endpoints),
        is_privileged(),
    )
}

/// Pure readiness gate, so each mode/TAP/live combination is testable without a
/// real `/run` marker or `/sys/class/net`.
fn ensure_ready_with(
    mode: NetworkMode,
    endpoints: &[u16],
    marker: Option<&ReadyMarker>,
    control_taps: bool,
    data_taps: bool,
    live: Option<bool>,
    privileged: bool,
) -> Result<()> {
    let Some(marker) = marker else {
        bail!(
            "networking is not set up for mode '{}'; run `sudo hegn network setup --user $USER`",
            mode.label()
        );
    };
    if marker.endpoints != endpoints {
        bail!(
            "local LLM endpoints changed (configured {:?}, firewall has {:?}); re-run `sudo hegn \
             network setup`",
            endpoints,
            marker.endpoints
        );
    }
    if !control_taps {
        bail!("the control TAP pool is incomplete; run `sudo hegn network setup --user $USER`");
    }
    if mode == NetworkMode::Guarded && !data_taps {
        bail!(
            "the guarded-network data TAP pool is incomplete; run `sudo hegn network setup \
             --user $USER`"
        );
    }
    // A root-owned receipt can outlive its rules; verify the kernel state when
    // this process is allowed to read it (root-only). A privileged caller can
    // always run nft, so an unverifiable guard is not ready; unprivileged
    // callers keep the root-owned marker as their gate.
    if !super::firewall::guard_ready_from_status(live, privileged) {
        bail!(
            "the nftables guard is missing, incomplete, or could not be verified; re-run `sudo \
             hegn network setup --user $USER`"
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
    // longer exists; new VM starts then fail closed until setup succeeds.
    clear_marker()?;
    run_script(&script).context("failed to configure the guarded network")?;
    // Do not write a readiness receipt unless the rules and both TAP families
    // actually landed. A host firewall or a concurrent `nft flush` must not
    // leave a marker that vouches for a guard that does not exist.
    ensure_guard_live(&endpoints, "setup")?;
    ensure_tap_pools("setup")?;
    record_ready(endpoints)
}

/// Reinstall the nftables guard without touching the TAP pool or the host
/// firewall exemptions. Used when only the endpoint list changed.
pub fn reload(endpoints: &[u16]) -> Result<()> {
    if !is_privileged() {
        bail!("`hegn network reload` must run as root (try `sudo hegn network reload`)");
    }
    if ready_marker().is_none() {
        bail!("networking is not set up; run `sudo hegn network setup --user $USER` first");
    }
    let mut endpoints = endpoints.to_vec();
    endpoints.sort_unstable();
    endpoints.dedup();
    let script = reload_script(&endpoints)?;
    // Keep the marker in place: a failed reload with unchanged endpoints leaves
    // the previous guard installed, and an endpoint change already fails
    // closed because the marker records the old list.
    run_script(&script).context("failed to reload the guarded network")?;
    ensure_guard_live(&endpoints, "reload")?;
    ensure_tap_pools("reload")?;
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
    // setup/reload are root-only, so an unverified guard must never be turned
    // into a readiness receipt.
    if !super::firewall::guard_ready_from_status(
        super::firewall::live_guard_status(endpoints),
        is_privileged(),
    ) {
        bail!(
            "the nftables guard is missing or unverifiable after {phase}; refusing to write {}",
            marker_path().display()
        );
    }
    Ok(())
}

/// A marker may only be written when the devices it names actually exist. The
/// control pool is mandatory for every mode; the data pool backs guarded
/// egress.
fn ensure_tap_pools(phase: &str) -> Result<()> {
    if !control_taps_present() {
        bail!(
            "the control TAP pool is missing after {phase}; refusing to write {}",
            marker_path().display()
        );
    }
    if !data_taps_present() {
        bail!(
            "the guarded-network TAP pool is missing after {phase}; refusing to write {}",
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
        control_pool: CONTROL_TAP_POOL,
        control_subnet: CONTROL_SUBNET.to_string(),
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
    let torn_down =
        run_script(&teardown_script()).context("failed to tear down the guarded network");
    // The marker goes last, even when the script failed: a missing marker fails
    // closed, while a stale one could vouch for devices that are already gone.
    clear_marker()?;
    torn_down
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkMode;

    #[test]
    fn pkexec_is_preferred_over_sudo() {
        assert_eq!(
            helpers_from(true, true),
            vec![("pkexec", false), ("sudo", true)]
        );
        assert_eq!(helpers_from(true, false), vec![("pkexec", false)]);
        assert_eq!(helpers_from(false, true), vec![("sudo", true)]);
        assert!(helpers_from(false, false).is_empty());
    }

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

    fn ready_marker_fixture(endpoints: Vec<u16>) -> ReadyMarker {
        ReadyMarker {
            version: MARKER_VERSION,
            pool: TAP_POOL,
            subnet: SUBNET.to_string(),
            control_pool: CONTROL_TAP_POOL,
            control_subnet: CONTROL_SUBNET.to_string(),
            dns: DNS.iter().map(ToString::to_string).collect(),
            endpoints,
            host_firewall: crate::network::HostFirewall::None,
            applied_at: Utc::now(),
        }
    }

    #[test]
    fn ensure_ready_without_a_marker_fails_closed_for_every_mode() {
        for mode in NetworkMode::ALL {
            let err = ensure_ready_with(mode, &[], None, true, true, None, false).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(&format!("mode '{}'", mode.label()))
                    && message.contains("sudo hegn network setup"),
                "unexpected error: {message}"
            );
        }
    }

    #[test]
    fn air_gapped_ensure_ready_still_requires_a_marker_and_control_taps() {
        let marker = ready_marker_fixture(Vec::new());
        // Off needs no data pool, but the control pool and marker are mandatory.
        assert!(
            ensure_ready_with(
                NetworkMode::Off,
                &[],
                Some(&marker),
                true,
                false,
                None,
                false
            )
            .is_ok()
        );
        let err = ensure_ready_with(
            NetworkMode::Off,
            &[],
            Some(&marker),
            false,
            false,
            Some(true),
            true,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("control TAP pool") && message.contains("sudo hegn network setup"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn guarded_ensure_ready_also_requires_the_data_pool() {
        let marker = ready_marker_fixture(Vec::new());
        assert!(
            ensure_ready_with(
                NetworkMode::Guarded,
                &[],
                Some(&marker),
                true,
                true,
                Some(true),
                true
            )
            .is_ok()
        );
        let err = ensure_ready_with(
            NetworkMode::Guarded,
            &[],
            Some(&marker),
            true,
            false,
            Some(true),
            true,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("data TAP pool") && message.contains("sudo hegn network setup"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn ensure_ready_rejects_changed_endpoints_in_every_mode() {
        let marker = ready_marker_fixture(vec![11434]);
        for mode in NetworkMode::ALL {
            let err = ensure_ready_with(mode, &[4321], Some(&marker), true, true, Some(true), true)
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("endpoints changed"),
                "{mode:?}: {err:#}"
            );
        }
    }

    #[test]
    fn privileged_ensure_ready_fails_closed_when_the_live_guard_is_unverifiable() {
        let marker = ready_marker_fixture(Vec::new());
        for live in [None, Some(false)] {
            let err = ensure_ready_with(
                NetworkMode::Off,
                &[],
                Some(&marker),
                true,
                false,
                live,
                true,
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("could not be verified"),
                "live={live:?}: {err:#}"
            );
        }
        // Unprivileged callers cannot run nft; the marker is their gate.
        assert!(
            ensure_ready_with(
                NetworkMode::Off,
                &[],
                Some(&marker),
                true,
                false,
                None,
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn ensure_ready_on_this_host_fails_closed_without_a_marker() {
        // Read-only: never writes to /run. On a host with a v3 marker this
        // test has nothing to prove about the missing-marker path.
        let marker = ready_marker();
        for mode in NetworkMode::ALL {
            let config = Config {
                network: mode,
                ..Config::default()
            };
            if marker.is_none() {
                let err = ensure_ready(&config).unwrap_err();
                let message = format!("{err:#}");
                assert!(
                    message.contains("networking is not set up")
                        && message.contains("sudo hegn network setup"),
                    "{mode:?}: {message}"
                );
            }
        }
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
