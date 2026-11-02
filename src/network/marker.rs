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

/// Marker present, every TAP in the pool exists and the endpoint DNAT rules
/// match the current configuration. Used by doctor and the automatic setup.
pub fn guard_ready(config: &Config) -> bool {
    ready_marker().is_some_and(|marker| marker.endpoints == config.endpoint_ports())
        && status(config).iter().all(|tap| tap.exists)
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
}
