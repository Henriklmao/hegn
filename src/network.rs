//! Host-enforced network guard for sandboxes.
//!
//! The guest never gets a bridge or a route into the host LAN. Each guarded
//! sandbox receives a dedicated TAP device from a pre-created pool. Host
//! nftables rules accept only outbound HTTP(S) and DNS to public resolvers;
//! every other packet, including all RFC1918/link-local traffic and all IPv6,
//! is dropped before it can be forwarded. Because the rules live on the host,
//! root inside the guest cannot remove them.
//!
//! TAP creation and nftables need `CAP_NET_ADMIN`. `hegn network setup` is the
//! explicit, one-time privileged step; `hegn sandbox start` is fail-closed and
//! refuses to boot a guarded VM unless the host marker shows that the firewall
//! is active.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::Ipv4Addr;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::sandbox::SandboxManager;
use crate::sandbox_config::SandboxStore;

/// TAP devices created by `hegn network setup`.
pub const TAP_POOL: u8 = 16;
/// Root of the isolated sandbox network. Each TAP gets a `/30` slice.
pub const SUBNET: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 0);
/// Guest netmask for every `/30` slice.
pub const NETMASK: &str = "255.255.255.252";
/// Public resolvers the guest may query. The host LAN DNS is never reachable.
pub const DNS: [Ipv4Addr; 2] = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(9, 9, 9, 9)];
/// Marker written by the privileged setup step. `/run` is cleared on reboot,
/// so a stale marker cannot outlive the nftables rules it vouches for.
pub const MARKER: &str = "/run/hegn/network.json";
const MARKER_VERSION: u32 = 1;
const NETWORK_DIR: &str = "network";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyMarker {
    pub version: u32,
    pub pool: u8,
    pub subnet: String,
    pub dns: Vec<String>,
    pub applied_at: DateTime<Utc>,
}

/// A reserved TAP device for one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lease {
    pub index: u8,
    pub sandbox: String,
    pub tap: String,
    pub mac: String,
    pub host_ip: Ipv4Addr,
    pub guest_ip: Ipv4Addr,
}

impl Lease {
    /// `--net` value for Cloud Hypervisor. The host address is configured by
    /// `hegn network setup`; passing `ip=`/`mask=` here would rewrite the host
    /// side of the TAP from inside the VMM.
    pub fn cloud_hypervisor_arg(&self) -> String {
        format!("tap={},mac={}", self.tap, self.mac)
    }

    /// NoCloud v2 network config for the guest. Matching on the MAC keeps
    /// this independent of the guest's predictable interface name.
    pub fn guest_network_config(&self) -> String {
        format!(
            "version: 2\n\
             ethernets:\n\
             \x20 hegn0:\n\
             \x20   match:\n\
             \x20     macaddress: \"{}\"\n\
             \x20   set-name: hegn0\n\
             \x20   dhcp4: false\n\
             \x20   dhcp6: false\n\
             \x20   accept-ra: false\n\
             \x20   addresses: [\"{}/30\"]\n\
             \x20   routes:\n\
             \x20     - to: default\n\
             \x20       via: {}\n\
             \x20   nameservers:\n\
             \x20     addresses: [\"{}\", \"{}\"]\n",
            self.mac, self.guest_ip, self.host_ip, DNS[0], DNS[1]
        )
    }
}

pub fn tap_name(index: u8) -> String {
    format!("hegn{index}")
}

pub fn host_ip(index: u8) -> Ipv4Addr {
    Ipv4Addr::new(
        SUBNET.octets()[0],
        SUBNET.octets()[1],
        SUBNET.octets()[2],
        4 * index + 1,
    )
}

pub fn guest_ip(index: u8) -> Ipv4Addr {
    Ipv4Addr::new(
        SUBNET.octets()[0],
        SUBNET.octets()[1],
        SUBNET.octets()[2],
        4 * index + 2,
    )
}

pub fn mac(index: u8) -> String {
    format!("52:54:00:00:00:{:02x}", index + 1)
}

fn network_dir(config: &Config) -> PathBuf {
    config.state_dir.join(NETWORK_DIR)
}

fn owner_path(config: &Config, index: u8) -> PathBuf {
    network_dir(config).join(format!("{}.owner", tap_name(index)))
}

/// One row of `hegn network status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TapStatus {
    pub index: u8,
    pub tap: String,
    pub host_ip: Ipv4Addr,
    pub exists: bool,
    pub owner: Option<String>,
}

pub fn status(config: &Config) -> Vec<TapStatus> {
    (0..TAP_POOL)
        .map(|index| TapStatus {
            index,
            tap: tap_name(index),
            host_ip: host_ip(index),
            exists: tap_exists(index),
            owner: read_owner(&owner_path(config, index))
                .map(|(sandbox, _pid)| sandbox)
                .filter(|owner| !owner.is_empty()),
        })
        .collect()
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

/// Marker present and every TAP in the pool exists. Used by doctor and by the
/// automatic setup action.
pub fn guard_ready(config: &Config) -> bool {
    ready_marker().is_some() && status(config).iter().all(|tap| tap.exists)
}

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
pub fn setup_with_privilege(user: &str) -> Result<()> {
    if !safe_user(user) {
        bail!("invalid user name for TAP ownership: {user:?}");
    }
    if is_privileged() {
        return setup(user);
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
    let mut command = Command::new(helper);
    if prefix {
        command.arg("--");
    }
    let status = command
        .arg(&exe)
        .arg("network")
        .arg("setup")
        .arg("--user")
        .arg(user)
        .status()
        .with_context(|| format!("failed to run {helper}"))?;
    if !status.success() {
        bail!("{helper} exited with {status}");
    }
    Ok(())
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

/// Fail closed when guarded networking is configured but not applied.
pub fn ensure_ready(config: &Config) -> Result<()> {
    if crate::config::NetworkMode::Guarded != config.network {
        return Ok(());
    }
    if ready_marker().is_none() {
        bail!(
            "guarded networking is enabled but the host firewall is not active; run `sudo hegn \
             network setup` once (or set `network = \"off\"` for an air-gapped VM)"
        );
    }
    Ok(())
}

/// Reserve a TAP slice for a sandbox.
///
/// The sandbox's recorded index is preferred so a snapshot restored later sees
/// the same device name. If the recorded owner is another running sandbox, or
/// the pool is exhausted, the start fails instead of silently sharing a TAP.
pub fn acquire(config: &Config, sandbox: &crate::sandbox_config::SandboxConfig) -> Result<Lease> {
    ensure_ready(config)?;
    fs::create_dir_all(network_dir(config))
        .with_context(|| format!("failed to create {}", network_dir(config).display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(network_dir(config), fs::Permissions::from_mode(0o700))?;
    }

    let preferred = sandbox.network_index;
    if let Some(index) = preferred {
        if index >= TAP_POOL {
            bail!(
                "sandbox '{}' records network index {index}, but the TAP pool has {TAP_POOL} slots",
                sandbox.name
            );
        }
        if !tap_exists(index) {
            bail!(
                "guarded TAP {} is missing; run `sudo hegn network setup --user $USER`",
                tap_name(index)
            );
        }
        return try_claim(config, index, &sandbox.name)?.with_context(|| {
            format!(
                "guarded TAP {} is reserved by another running sandbox",
                tap_name(index)
            )
        });
    }

    for index in 0..TAP_POOL {
        if !tap_exists(index) {
            continue;
        }
        if let Some(lease) = try_claim(config, index, &sandbox.name)? {
            return Ok(lease);
        }
    }
    bail!(
        "no free guarded-network TAP is available for sandbox '{}'; increase the pool or stop \
         another sandbox",
        sandbox.name
    )
}

fn try_claim(config: &Config, index: u8, sandbox: &str) -> Result<Option<Lease>> {
    let path = owner_path(config, index);
    let lock = lock_path(config, index);
    let _guard = FileLock::acquire(&lock)?;

    if let Some((owner, pid)) = read_owner(&path)
        && !owner.is_empty()
        && owner != sandbox
    {
        // A live hegn process may still be booting the owner VM, before its
        // session record exists. A live VM is equally authoritative.
        if crate::sandbox::process_alive(pid) || sandbox_is_running(config, &owner) {
            return Ok(None);
        }
        // A crashed hegn process and a stopped VM left the reservation behind.
        let _ = fs::remove_file(&path);
    }

    let lease = Lease {
        index,
        sandbox: sandbox.to_string(),
        tap: tap_name(index),
        mac: mac(index),
        host_ip: host_ip(index),
        guest_ip: guest_ip(index),
    };
    write_private(&path, &format!("{sandbox}\n{}\n", std::process::id()))?;
    Ok(Some(lease))
}

/// Release every reservation held by a sandbox, including stale ones.
pub fn release(config: &Config, sandbox: &str) {
    // Hold the per-TAP lock while checking the owner: another hegn process may
    // be claiming the same slice concurrently, and removing its fresh owner
    // file would let a third sandbox share the TAP.
    for index in 0..TAP_POOL {
        let path = owner_path(config, index);
        if !path.exists() {
            continue;
        }
        let Ok(_guard) = FileLock::acquire(&lock_path(config, index)) else {
            continue;
        };
        if read_owner(&path).is_some_and(|(owner, _)| owner == sandbox) {
            let _ = fs::remove_file(&path);
        }
    }
}

pub fn sandbox_is_running(config: &Config, sandbox: &str) -> bool {
    SandboxStore::new(config.clone())
        .load(sandbox)
        .map(|sandbox_config| SandboxManager::new(config.clone(), sandbox_config).is_running())
        .unwrap_or(false)
}

/// Owner files contain `sandbox\npid\n`. The pid covers the window between
/// reservation and the session record becoming visible.
fn read_owner(path: &Path) -> Option<(String, u32)> {
    let raw = fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();
    let sandbox = lines.next()?.trim().to_string();
    let pid = lines
        .next()
        .and_then(|line| line.trim().parse().ok())
        .unwrap_or(0);
    Some((sandbox, pid))
}

fn tap_exists(index: u8) -> bool {
    Path::new("/sys/class/net").join(tap_name(index)).exists()
}

fn lock_path(config: &Config, index: u8) -> PathBuf {
    network_dir(config).join(format!("{}.lock", tap_name(index)))
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

struct FileLock {
    _file: fs::File,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        // Safety: `file` is a valid open descriptor for a private lock file.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if locked != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

/// `true` when the process has the privileges required for setup/teardown.
pub fn is_privileged() -> bool {
    // Safety: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Shell script that creates the TAP pool and installs the nftables table.
/// `user` must pass [`safe_user`]; it is embedded in the script deliberately.
pub fn setup_script(user: &str) -> Result<String> {
    if !safe_user(user) {
        bail!("invalid user name for TAP ownership: {user:?}");
    }
    let mut script = String::from(
        "set -eu\n\
         sysctl -q -w net.ipv4.ip_forward=1\n\
         install -d -m 0755 /run/hegn\n",
    );
    for index in 0..TAP_POOL {
        let tap = tap_name(index);
        let addr = host_ip(index);
        let tap_mac = mac(index);
        script.push_str(&format!(
            "if ! ip link show dev {tap} >/dev/null 2>&1; then\n\
               ip tuntap add dev {tap} mode tap user {user}\n\
             fi\n\
             ip link set dev {tap} address {tap_mac}\n\
             ip addr replace {addr}/30 dev {tap}\n\
             ip link set {tap} up\n"
        ));
    }
    script.push_str(
        "nft delete table inet hegn 2>/dev/null || true\n\
         nft -f - <<'HEGN_NFT'\n",
    );
    script.push_str(&nft_rules());
    script.push_str("HEGN_NFT\n");
    Ok(script)
}

pub fn teardown_script() -> String {
    let mut script = String::from("nft delete table inet hegn 2>/dev/null || true\n");
    for index in 0..TAP_POOL {
        script.push_str(&format!(
            "ip link del dev {} 2>/dev/null || true\n",
            tap_name(index)
        ));
    }
    script.push_str("rm -f /run/hegn/network.json\n");
    script
}

/// nftables rules. Public HTTP(S) and public DNS only; everything else from a
/// sandbox TAP is dropped, including RFC1918, link-local, CGNAT, multicast and
/// all IPv6.
pub fn nft_rules() -> String {
    let interfaces = (0..TAP_POOL)
        .map(|index| format!("\"{}\"", tap_name(index)))
        .collect::<Vec<_>>()
        .join(", ");
    let dns = DNS
        .iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "table inet hegn {{\n\
         \x20 set hegn_ifaces {{\n\
         \x20   type ifname\n\
         \x20   flags constant\n\
         \x20   elements = {{ {interfaces} }}\n\
         \x20 }}\n\
         \x20 set private_v4 {{\n\
         \x20   type ipv4_addr\n\
         \x20   flags interval\n\
         \x20   elements = {{\n\
         \x20     0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8,\n\
         \x20     169.254.0.0/16, 172.16.0.0/12, 192.0.0.0/24, 192.0.2.0/24,\n\
         \x20     192.168.0.0/16, 198.18.0.0/15, 198.51.100.0/24,\n\
         \x20     203.0.113.0/24, 224.0.0.0/4, 240.0.0.0/4\n\
         \x20   }}\n\
         \x20 }}\n\
         \x20 set web_dns {{\n\
         \x20   type ipv4_addr\n\
         \x20   flags interval\n\
         \x20   elements = {{ {dns} }}\n\
         \x20 }}\n\
         \x20 chain input {{\n\
         \x20   type filter hook input priority filter; policy accept;\n\
         \x20   iifname @hegn_ifaces counter drop\n\
         \x20 }}\n\
         \x20 chain forward {{\n\
         \x20   type filter hook forward priority filter; policy accept;\n\
         \x20   iifname @hegn_ifaces ip saddr != 10.213.0.0/24 counter drop\n\
         \x20   iifname @hegn_ifaces meta nfproto ipv6 counter drop\n\
         \x20   iifname @hegn_ifaces ip daddr @private_v4 counter drop\n\
         \x20   iifname @hegn_ifaces ct state established,related accept\n\
         \x20   iifname @hegn_ifaces ip protocol tcp tcp dport {{ 80, 443 }} accept\n\
         \x20   iifname @hegn_ifaces ip protocol tcp tcp dport 53 ip daddr @web_dns accept\n\
         \x20   iifname @hegn_ifaces ip protocol udp udp dport 53 ip daddr @web_dns accept\n\
         \x20   iifname @hegn_ifaces counter limit rate 10/minute burst 20 packets \\\n\
         \x20     log prefix \"hegn-egress-drop \" level warn drop\n\
         \x20   # No unsolicited inbound connections from the LAN or internet.\n\
         \x20   # Conntrack first un-NATs replies, so established traffic is allowed.\n\
         \x20   ip daddr 10.213.0.0/24 ct state established,related accept\n\
         \x20   ip daddr 10.213.0.0/24 counter drop\n\
         \x20 }}\n\
         \x20 chain postrouting {{\n\
         \x20   type nat hook postrouting priority srcnat; policy accept;\n\
         \x20   ip saddr 10.213.0.0/24 masquerade\n\
         \x20 }}\n\
         }}\n"
    )
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
pub fn setup(user: &str) -> Result<()> {
    if !is_privileged() {
        bail!("`hegn network setup` must run as root (try `sudo hegn network setup --user $USER`)");
    }
    let script = setup_script(user)?;
    // The script replaces the nftables table. Drop the marker first so a
    // failed re-run cannot leave a "ready" marker vouching for a guard that no
    // longer exists; new sandbox starts then fail closed until setup succeeds.
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
        applied_at: Utc::now(),
    };
    let raw = serde_json::to_vec_pretty(&marker)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(marker_path())
        .with_context(|| format!("failed to write {}", marker_path().display()))?;
    file.write_all(&raw)?;
    file.sync_all()?;
    Ok(())
}

pub fn teardown() -> Result<()> {
    if !is_privileged() {
        bail!("`hegn network teardown` must run as root");
    }
    run_script(&teardown_script()).context("failed to tear down the guarded network")
}

fn run_script(script: &str) -> Result<()> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .context("failed to run the network setup script")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("network script failed: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_math_is_a_30_slice_per_pool_entry() {
        assert_eq!(host_ip(0), Ipv4Addr::new(10, 213, 0, 1));
        assert_eq!(guest_ip(0), Ipv4Addr::new(10, 213, 0, 2));
        assert_eq!(host_ip(1), Ipv4Addr::new(10, 213, 0, 5));
        assert_eq!(guest_ip(15), Ipv4Addr::new(10, 213, 0, 62));
        assert_eq!(mac(0), "52:54:00:00:00:01");
        assert_eq!(mac(15), "52:54:00:00:00:10");
    }

    #[test]
    fn nft_rules_block_private_ranges_and_allow_web_only() {
        let rules = nft_rules();
        for private in [
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "100.64.0.0/10",
            "224.0.0.0/4",
            "meta nfproto ipv6 counter drop",
        ] {
            assert!(rules.contains(private), "missing {private}:\n{rules}");
        }
        assert!(rules.contains("tcp dport { 80, 443 } accept"));
        assert!(rules.contains("udp dport 53 ip daddr @web_dns accept"));
        assert!(rules.contains("iifname @hegn_ifaces counter drop"));
        assert!(rules.contains("masquerade"));
        assert!(!rules.contains("tcp dport 22"));
    }

    #[test]
    fn setup_script_creates_every_pool_tap() {
        let script = setup_script("alice").unwrap();
        for index in 0..TAP_POOL {
            assert!(script.contains(&format!(
                "ip tuntap add dev {} mode tap user alice",
                tap_name(index)
            )));
        }
        assert!(script.contains("nft -f -"));
    }

    #[test]
    fn rejects_unsafe_tap_owners() {
        for bad in ["root; reboot", "$(id)", "-rf", "a/b", ""] {
            assert!(setup_script(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(safe_user("_alice-1"));
    }

    #[test]
    fn guest_config_matches_the_reserved_slice() {
        let lease = Lease {
            index: 3,
            sandbox: "demo".into(),
            tap: tap_name(3),
            mac: mac(3),
            host_ip: host_ip(3),
            guest_ip: guest_ip(3),
        };
        let config = lease.guest_network_config();
        assert!(config.contains("10.213.0.14/30"), "{config}");
        assert!(config.contains("via: 10.213.0.13"), "{config}");
        assert!(config.contains("1.1.1.1"));
        assert!(config.contains("52:54:00:00:00:04"));
        assert_eq!(
            lease.cloud_hypervisor_arg(),
            "tap=hegn3,mac=52:54:00:00:00:04"
        );
    }
}
