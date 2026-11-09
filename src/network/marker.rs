//! Root-written readiness marker that vouches for the active network guard.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::firewall::HostFirewall;
use super::pool::status;
use super::{MARKER, MARKER_VERSION, SUBNET, TAP_POOL};
use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyMarker {
    pub version: u32,
    pub pool: u8,
    pub subnet: String,
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
    let path = marker_path();
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
    let marker: ReadyMarker = serde_json::from_str(&raw).ok()?;
    (marker.version == MARKER_VERSION
        && marker.pool == TAP_POOL
        && marker.subnet == SUBNET.to_string())
    .then_some(marker)
}

/// Marker present, every TAP in the pool exists and — when the caller can
/// inspect the kernel — the live nftables table still matches the marker.
///
/// Unprivileged callers (TUI, `hegn network status`) cannot read nftables, so
/// they fall back to the root-owned marker and TAP presence.
pub fn guard_ready(config: &Config) -> bool {
    let endpoints = config.endpoint_ports();
    if !(ready_marker().is_some_and(|marker| marker.endpoints == endpoints)
        && status(config).iter().all(|tap| tap.exists))
    {
        return false;
    }
    // A privileged caller can always inspect nftables, so an unverifiable
    // guard must fail closed. Unprivileged callers keep the root-owned marker
    // as their only gate.
    super::firewall::guard_ready_from_status(
        super::firewall::live_guard_status(&endpoints),
        super::privilege::is_privileged(),
    )
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

    #[test]
    fn ready_marker_is_readable_by_the_unprivileged_doctor() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.json");
        let marker = ReadyMarker {
            version: MARKER_VERSION,
            pool: TAP_POOL,
            subnet: SUBNET.to_string(),
            dns: DNS.iter().map(ToString::to_string).collect(),
            endpoints: Vec::new(),
            host_firewall: HostFirewall::Ufw,
            applied_at: Utc::now(),
        };
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

    fn marker(endpoints: Vec<u16>, host_firewall: HostFirewall) -> ReadyMarker {
        ReadyMarker {
            version: MARKER_VERSION,
            pool: TAP_POOL,
            subnet: SUBNET.to_string(),
            dns: DNS.iter().map(ToString::to_string).collect(),
            endpoints,
            host_firewall,
            applied_at: Utc::now(),
        }
    }

    #[test]
    fn marker_roundtrip_preserves_endpoints_and_firewall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.json");
        let expected = marker(vec![1234, 11434], HostFirewall::Firewalld);

        write_marker(&path, &expected).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let parsed: ReadyMarker = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, expected);
        assert!(raw.contains("\"endpoints\": [\n"), "{raw}");
        assert!(raw.contains("11434"), "{raw}");
        assert!(raw.contains("\"host_firewall\": \"firewalld\""), "{raw}");
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
    fn legacy_marker_without_optional_fields_still_deserializes() {
        // A marker written before local endpoints existed has neither
        // `endpoints` nor `host_firewall`; both default rather than failing.
        let legacy = r#"{
            "version": 1,
            "pool": 16,
            "subnet": "10.213.0.0",
            "dns": ["1.1.1.1", "9.9.9.9"],
            "applied_at": "2024-01-01T00:00:00Z"
        }"#;
        let parsed: ReadyMarker = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.version, 1);
        assert!(parsed.endpoints.is_empty());
        assert_eq!(parsed.host_firewall, HostFirewall::None);
        // The version gate is what makes a v1 marker unusable for v2 rules.
        assert_ne!(
            parsed.version, MARKER_VERSION,
            "an old marker must never vouch for the current rules"
        );
    }

    #[test]
    fn malformed_or_incomplete_markers_are_rejected() {
        for raw in [
            "",
            "not json",
            "{}",
            r#"{"version": 2}"#,
            r#"{"version": "2", "pool": 16, "subnet": "10.213.0.0", "dns": [], "applied_at": "2024-01-01T00:00:00Z"}"#,
            r#"{"version": 2, "pool": 16, "subnet": "10.213.0.0", "dns": [], "applied_at": "yesterday"}"#,
        ] {
            assert!(
                serde_json::from_str::<ReadyMarker>(raw).is_err(),
                "malformed marker must be rejected: {raw:?}"
            );
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
    fn marker_lives_on_the_volatile_run_filesystem() {
        assert_eq!(marker_path(), Path::new("/run/hegn/network.json"));
        assert!(marker_path().is_absolute());
    }

    #[test]
    fn guard_ready_requires_a_valid_marker_matching_endpoints_and_all_taps() {
        // Read-only: never writes to /run. On an unguarded host there is no
        // marker and readiness must be false; on a guarded host readiness is
        // exactly "endpoints match && every pool TAP exists".
        let config = Config::default();
        let marker = ready_marker();
        let all_taps_exist = status(&config).iter().all(|tap| tap.exists);
        let endpoints_match = marker
            .as_ref()
            .is_some_and(|marker| marker.endpoints == config.endpoint_ports());

        let expected = endpoints_match
            && all_taps_exist
            && super::super::firewall::guard_ready_from_status(
                super::super::firewall::live_guard_status(&config.endpoint_ports()),
                super::super::privilege::is_privileged(),
            );
        assert_eq!(guard_ready(&config), expected);
        if marker.is_none() {
            assert!(!guard_ready(&config), "no marker means not ready");
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
