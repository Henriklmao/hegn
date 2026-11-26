//! Root-written readiness marker that vouches for the active network guard.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::addressing::{control_tap_name, tap_name};
use super::firewall::HostFirewall;
use super::{CONTROL_SUBNET, CONTROL_TAP_POOL, MARKER, MARKER_VERSION, SUBNET, TAP_POOL};
use crate::config::{Config, NetworkMode};

/// Where the kernel exposes network interfaces.
const SYSFS_NET: &str = "/sys/class/net";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyMarker {
    pub version: u32,
    pub pool: u8,
    pub subnet: String,
    /// Number of control TAP slots. Every VM needs one for SSH, independent of
    /// its data-network mode.
    pub control_pool: u8,
    /// Root of the host-only control network (`10.214.0.0`).
    pub control_subnet: String,
    pub dns: Vec<String>,
    /// TCP ports DNATed to the host loopback for the guest.
    #[serde(default)]
    pub endpoints: Vec<u16>,
    /// Host firewall the TAP pool was carved out of. Informational: readiness
    /// does not depend on it, because detection needs root.
    #[serde(default)]
    pub host_firewall: HostFirewall,
    pub applied_at: DateTime<Utc>,
}

/// Path of the marker. Exposed for doctor/status output and tests.
pub fn marker_path() -> &'static Path {
    Path::new(MARKER)
}

/// Read and validate the host firewall marker.
pub fn ready_marker() -> Option<ReadyMarker> {
    ready_marker_at(marker_path())
}

/// Read and validate a marker from `path`.
///
/// Kept separate from [`marker_path`] so tests can exercise the file-level
/// safety checks (symlinks, ownership) without touching `/run`.
fn ready_marker_at(path: &Path) -> Option<ReadyMarker> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Only root may create the marker. A user-writable marker would let an
        // unprivileged process claim that the firewall is active.
        if meta.uid() != 0 {
            return None;
        }
    }
    let raw = fs::read_to_string(path).ok()?;
    parse_marker(&raw)
}

/// Parse and validate the marker payload.
///
/// The version gate is what makes an old marker unusable for new rules. The
/// control fields are required (not serde defaults), so a version 2 marker
/// cannot be reinterpreted as a version 3 receipt even if someone edits its
/// version number.
fn parse_marker(raw: &str) -> Option<ReadyMarker> {
    let marker: ReadyMarker = serde_json::from_str(raw).ok()?;
    (marker.version == MARKER_VERSION
        && marker.pool == TAP_POOL
        && marker.subnet == SUBNET.to_string()
        && marker.control_pool == CONTROL_TAP_POOL
        && marker.control_subnet == CONTROL_SUBNET.to_string())
    .then_some(marker)
}

/// All slots of a TAP family exist under `root`.
fn taps_present(root: &Path, pool: u8, name: impl Fn(u8) -> String) -> bool {
    (0..pool).all(|index| root.join(name(index)).exists())
}

/// Whether every control TAP (`hegnc0..hegnc15`) currently exists.
pub(super) fn control_taps_present() -> bool {
    taps_present(Path::new(SYSFS_NET), CONTROL_TAP_POOL, control_tap_name)
}

/// Whether every guarded data TAP (`hegn0..hegn15`) currently exists.
pub(super) fn data_taps_present() -> bool {
    taps_present(Path::new(SYSFS_NET), TAP_POOL, tap_name)
}

/// Marker present, the control TAP pool exists and — when the caller can
/// inspect the kernel — the live nftables rules still match the marker.
///
/// Air-gapped (`off`) guests still need their control TAPs for SSH, so there is
/// no mode that skips host preparation. Guarded guests additionally need the
/// data TAP pool.
///
/// Unprivileged callers (TUI, `hegn network status`) cannot read nftables, so
/// they fall back to the root-owned marker and TAP presence.
pub fn guard_ready(config: &Config) -> bool {
    let endpoints = config.endpoint_ports();
    guard_ready_with(
        config.network,
        &endpoints,
        ready_marker().as_ref(),
        control_taps_present(),
        data_taps_present(),
        super::firewall::live_guard_status(&endpoints),
        super::privilege::is_privileged(),
    )
}

/// Pure readiness decision, so every mode/TAP/live combination is testable
/// without a real `/run` marker or `/sys/class/net`.
fn guard_ready_with(
    mode: NetworkMode,
    endpoints: &[u16],
    marker: Option<&ReadyMarker>,
    control_taps: bool,
    data_taps: bool,
    live: Option<bool>,
    privileged: bool,
) -> bool {
    let Some(marker) = marker else {
        return false;
    };
    if marker.endpoints != endpoints || !control_taps {
        return false;
    }
    if mode == NetworkMode::Guarded && !data_taps {
        return false;
    }
    // A privileged caller can always inspect nftables, so an unverifiable
    // guard must fail closed. Unprivileged callers keep the root-owned marker
    // as their only gate.
    super::firewall::guard_ready_from_status(live, privileged)
}

/// The marker is deliberately world-readable: the unprivileged TUI reads it to
/// decide whether the host guard is active. It is not secret (the rules are
/// already visible with `nft list` to root), and the file must stay root-owned.
pub(super) fn write_marker(path: &Path, marker: &ReadyMarker) -> Result<()> {
    let raw = serde_json::to_vec_pretty(marker)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(&raw)?;
    // The process umask may be 077; set the mode on the open descriptor so the
    // unprivileged doctor can always read the marker.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .with_context(|| format!("failed to set marker permissions on {}", path.display()))?;
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::DNS;

    fn marker(endpoints: Vec<u16>, host_firewall: HostFirewall) -> ReadyMarker {
        ReadyMarker {
            version: MARKER_VERSION,
            pool: TAP_POOL,
            subnet: SUBNET.to_string(),
            control_pool: CONTROL_TAP_POOL,
            control_subnet: CONTROL_SUBNET.to_string(),
            dns: DNS.iter().map(ToString::to_string).collect(),
            endpoints,
            host_firewall,
            applied_at: Utc::now(),
        }
    }

    #[test]
    fn ready_marker_is_readable_by_the_unprivileged_doctor() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.json");
        let marker = marker(Vec::new(), HostFirewall::Ufw);
        write_marker(&path, &marker).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ReadyMarker = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.pool, TAP_POOL);
        assert_eq!(parsed.host_firewall, HostFirewall::Ufw);
        assert!(raw.contains("\"host_firewall\": \"ufw\""), "{raw}");
    }

    #[test]
    fn marker_roundtrip_preserves_endpoints_control_fields_and_firewall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.json");
        let expected = marker(vec![1234, 11434], HostFirewall::Firewalld);

        write_marker(&path, &expected).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let parsed: ReadyMarker = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, expected);
        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.control_pool, CONTROL_TAP_POOL);
        assert_eq!(parsed.control_subnet, "10.214.0.0");
        assert!(raw.contains("\"control_pool\": 16"), "{raw}");
        assert!(raw.contains("\"control_subnet\": \"10.214.0.0\""), "{raw}");
        assert!(raw.contains("\"endpoints\": [\n"), "{raw}");
        assert!(raw.contains("11434"), "{raw}");
        assert!(raw.contains("\"host_firewall\": \"firewalld\""), "{raw}");
        assert_eq!(parse_marker(&raw), Some(expected));
    }

    #[test]
    fn version_two_markers_never_vouch_for_version_three_rules() {
        // A plain v2 marker has no control fields and must not deserialize.
        let legacy = r#"{
            "version": 2,
            "pool": 16,
            "subnet": "10.213.0.0",
            "dns": ["1.1.1.1", "9.9.9.9"],
            "endpoints": [],
            "host_firewall": "none",
            "applied_at": "2024-01-01T00:00:00Z"
        }"#;
        assert!(serde_json::from_str::<ReadyMarker>(legacy).is_err());
        assert_eq!(parse_marker(legacy), None);

        // Even a v2 marker that carries the new fields is rejected by version.
        let upgraded = r#"{
            "version": 2,
            "pool": 16,
            "subnet": "10.213.0.0",
            "control_pool": 16,
            "control_subnet": "10.214.0.0",
            "dns": ["1.1.1.1", "9.9.9.9"],
            "endpoints": [],
            "host_firewall": "none",
            "applied_at": "2024-01-01T00:00:00Z"
        }"#;
        assert_eq!(parse_marker(upgraded), None);
    }

    #[test]
    fn markers_without_control_fields_are_rejected() {
        let raw = r#"{
            "version": 3,
            "pool": 16,
            "subnet": "10.213.0.0",
            "dns": ["1.1.1.1", "9.9.9.9"],
            "endpoints": [],
            "host_firewall": "none",
            "applied_at": "2024-01-01T00:00:00Z"
        }"#;
        assert!(
            serde_json::from_str::<ReadyMarker>(raw).is_err(),
            "control fields must be required, not defaulted"
        );
        assert_eq!(parse_marker(raw), None);
    }

    #[test]
    fn marker_with_wrong_control_pool_or_subnet_is_rejected() {
        for (pool, subnet) in [
            (CONTROL_TAP_POOL - 1, "10.214.0.0"),
            (CONTROL_TAP_POOL, "10.215.0.0"),
            (CONTROL_TAP_POOL, "10.213.0.0"),
        ] {
            let mut marker = marker(Vec::new(), HostFirewall::None);
            marker.control_pool = pool;
            marker.control_subnet = subnet.to_string();
            let raw = serde_json::to_string(&marker).unwrap();
            assert_eq!(parse_marker(&raw), None, "{pool}/{subnet} must be rejected");
        }
    }

    #[test]
    fn marker_permissions_are_world_readable_but_never_group_or_world_writable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.json");
        write_marker(&path, &marker(Vec::new(), HostFirewall::None)).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        // The unprivileged TUI/doctor must be able to read readiness, but an
        // unprivileged process must not be able to forge readiness.
        assert_eq!(mode & 0o444, 0o444, "marker must be readable: {mode:o}");
        assert_eq!(mode & 0o022, 0, "marker must not be writable: {mode:o}");
    }

    #[test]
    fn a_marker_owned_by_a_non_root_user_is_rejected() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.json");
        write_marker(&path, &marker(Vec::new(), HostFirewall::None)).unwrap();

        // The test process normally runs unprivileged; under root this check
        // cannot be exercised without changing file ownership.
        if fs::metadata(&path).unwrap().uid() != 0 {
            assert_eq!(ready_marker_at(&path), None);
        }
    }

    #[test]
    fn unknown_firewall_names_are_rejected() {
        assert!(serde_json::from_str::<HostFirewall>("\"nftables\"").is_err());
        assert!(serde_json::from_str::<HostFirewall>("\"\"").is_err());
    }

    #[test]
    fn writing_a_marker_truncates_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.json");
        let long = marker(vec![1234, 4321, 9999], HostFirewall::Iptables);
        write_marker(&path, &long).unwrap();

        let short = marker(Vec::new(), HostFirewall::None);
        write_marker(&path, &short).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let parsed: ReadyMarker = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, short);
        assert!(!raw.contains("9999"), "old contents must be gone: {raw}");
    }

    #[test]
    fn write_marker_refuses_to_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "root owned").unwrap();
        let link = dir.path().join("network.json");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        assert!(
            write_marker(&link, &marker(Vec::new(), HostFirewall::None)).is_err(),
            "O_NOFOLLOW must reject a symlinked marker"
        );
        assert_eq!(fs::read_to_string(&victim).unwrap(), "root owned");
    }

    #[test]
    fn reading_a_symlinked_marker_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        write_marker(&target, &marker(Vec::new(), HostFirewall::None)).unwrap();
        let link = dir.path().join("network.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(
            ready_marker_at(&link),
            None,
            "a symlink must never be accepted as the readiness marker"
        );
    }

    #[test]
    fn marker_lives_on_the_volatile_run_filesystem() {
        assert_eq!(marker_path(), Path::new("/run/hegn/network.json"));
        assert!(marker_path().is_absolute());
    }

    #[test]
    fn tap_presence_requires_every_slot_of_the_family() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        assert!(!taps_present(root, CONTROL_TAP_POOL, control_tap_name));
        assert!(!taps_present(root, TAP_POOL, tap_name));

        for index in 0..TAP_POOL {
            fs::write(root.join(control_tap_name(index)), "").unwrap();
            fs::write(root.join(tap_name(index)), "").unwrap();
        }
        assert!(taps_present(root, CONTROL_TAP_POOL, control_tap_name));
        assert!(taps_present(root, TAP_POOL, tap_name));

        fs::remove_file(root.join(control_tap_name(15))).unwrap();
        fs::remove_file(root.join(tap_name(0))).unwrap();
        assert!(!taps_present(root, CONTROL_TAP_POOL, control_tap_name));
        assert!(!taps_present(root, TAP_POOL, tap_name));
    }

    #[test]
    fn guard_ready_requires_control_taps_in_every_mode() {
        let marker = marker(Vec::new(), HostFirewall::None);
        for mode in NetworkMode::ALL {
            assert!(
                !guard_ready_with(mode, &[], Some(&marker), false, true, Some(true), true),
                "{mode:?} must not be ready without the control pool"
            );
        }
    }

    #[test]
    fn guarded_guard_ready_requires_the_data_pool_too() {
        let marker = marker(Vec::new(), HostFirewall::None);
        assert!(!guard_ready_with(
            NetworkMode::Guarded,
            &[],
            Some(&marker),
            true,
            false,
            Some(true),
            true
        ));
        assert!(guard_ready_with(
            NetworkMode::Guarded,
            &[],
            Some(&marker),
            true,
            true,
            Some(true),
            true
        ));
    }

    #[test]
    fn off_guard_ready_needs_a_marker_and_control_taps_but_no_data_pool() {
        let marker = marker(Vec::new(), HostFirewall::None);
        assert!(guard_ready_with(
            NetworkMode::Off,
            &[],
            Some(&marker),
            true,
            false,
            Some(true),
            true
        ));
        assert!(!guard_ready_with(
            NetworkMode::Off,
            &[],
            None,
            true,
            false,
            Some(true),
            true
        ));
        assert!(!guard_ready_with(
            NetworkMode::Off,
            &[],
            Some(&marker),
            false,
            true,
            Some(true),
            true
        ));
    }

    #[test]
    fn guard_ready_rejects_changed_endpoints() {
        let marker = marker(vec![11434], HostFirewall::None);
        assert!(!guard_ready_with(
            NetworkMode::Off,
            &[4321],
            Some(&marker),
            true,
            false,
            Some(true),
            true
        ));
        assert!(guard_ready_with(
            NetworkMode::Off,
            &[11434],
            Some(&marker),
            true,
            false,
            Some(true),
            true
        ));
    }

    #[test]
    fn privileged_guard_ready_fails_closed_when_the_live_guard_is_unverifiable() {
        let marker = marker(Vec::new(), HostFirewall::None);
        assert!(!guard_ready_with(
            NetworkMode::Off,
            &[],
            Some(&marker),
            true,
            false,
            None,
            true
        ));
        assert!(!guard_ready_with(
            NetworkMode::Off,
            &[],
            Some(&marker),
            true,
            false,
            Some(false),
            true
        ));
        // Unprivileged callers cannot run nft, so the root-owned marker stays
        // their gate.
        assert!(guard_ready_with(
            NetworkMode::Off,
            &[],
            Some(&marker),
            true,
            false,
            None,
            false
        ));
        assert!(!guard_ready_with(
            NetworkMode::Off,
            &[],
            Some(&marker),
            true,
            false,
            Some(false),
            false
        ));
    }

    #[test]
    fn guard_ready_never_vouches_from_the_host_state_without_a_marker() {
        // Read-only: never writes to /run. On a host without a v3 marker both
        // modes must report "not ready".
        for mode in NetworkMode::ALL {
            let config = Config {
                network: mode,
                ..Config::default()
            };
            if ready_marker().is_none() {
                assert!(!guard_ready(&config), "{mode:?} without a marker");
            }
        }
    }

    #[test]
    fn privileged_callers_fail_closed_when_the_live_guard_cannot_be_verified() {
        // `guard_ready_from_status` owns this policy; keep the marker-side
        // behavior explicit so a future refactor cannot silently fail open.
        assert!(!super::super::firewall::guard_ready_from_status(None, true));
        assert!(super::super::firewall::guard_ready_from_status(None, false));
    }
}
