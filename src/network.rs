//! Host-enforced network guard for instances.
//!
//! The guest never gets a bridge or a route into the host LAN. Each guarded
//! instance receives a dedicated TAP device from a pre-created pool. Host
//! nftables rules accept only outbound HTTP(S) and DNS to public resolvers;
//! every other packet, including all RFC1918/link-local traffic and all IPv6,
//! is dropped before it can be forwarded. Because the rules live on the host,
//! root inside the guest cannot remove them.
//!
//! netfilter evaluates *every* base chain hooked at `forward` and a drop in any
//! of them wins, so hegn's own table can only ever allow a packet: a host
//! firewall such as ufw (`DEFAULT_FORWARD_POLICY="DROP"`) or firewalld would
//! still drop it. Setup therefore also exempts the TAP pool from the active
//! host firewall. That exemption is a blanket accept *in the host firewall*;
//! the HTTP(S)/DNS allow-list stays in hegn's table, which still has to accept
//! the packet.
//!
//! TAP creation and nftables need `CAP_NET_ADMIN`. `hegn network setup` is the
//! explicit, one-time privileged step; `hegn instance start` is fail-closed and
//! refuses to boot a guarded VM unless the host marker shows that the firewall
//! is active.

use std::ffi::{OsStr, OsString};
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
use crate::instance::InstanceManager;
use crate::instance_config::InstanceStore;

/// TAP devices created by `hegn network setup`.
pub const TAP_POOL: u8 = 16;
/// Root of the isolated instance network. Each TAP gets a `/30` slice.
pub const SUBNET: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 0);
/// Guest netmask for every `/30` slice.
pub const NETMASK: &str = "255.255.255.252";
/// Public resolvers the guest may query. The host LAN DNS is never reachable.
pub const DNS: [Ipv4Addr; 2] = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(9, 9, 9, 9)];
/// Marker written by the privileged setup step. `/run` is cleared on reboot,
/// so a stale marker cannot outlive the nftables rules it vouches for.
pub const MARKER: &str = "/run/hegn/network.json";
/// Bumped only for incompatible rule generations. `endpoints` defaults to an
/// empty list, so a marker written before local endpoints existed stays valid
/// for a configuration without endpoints. Version 2 added the host-firewall
/// carve-out: a version 1 guard predates it and would silently drop all guest
/// traffic on hosts whose firewall also filters `forward`.
const MARKER_VERSION: u32 = 2;
const NETWORK_DIR: &str = "network";

/// Host firewalls that filter the netfilter `forward` hook in their own base
/// chain. Recorded in the marker for `doctor`/`status` output only; the setup
/// script re-checks at apply time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostFirewall {
    /// No host firewall with a routed drop policy was detected.
    #[default]
    None,
    /// `ufw` with `Status: active`; its routed policy is usually `DROP`.
    Ufw,
    /// A running `firewalld`.
    Firewalld,
    /// Plain `iptables` with `-P FORWARD DROP`.
    Iptables,
}

impl HostFirewall {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ufw => "ufw",
            Self::Firewalld => "firewalld",
            Self::Iptables => "iptables",
        }
    }
}

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

/// A reserved TAP device for one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lease {
    pub index: u8,
    pub instance: String,
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
                .map(|(instance, _pid)| instance)
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

/// Marker present, every TAP in the pool exists and the endpoint DNAT rules
/// match the current configuration. Used by doctor and the automatic setup.
pub fn guard_ready(config: &Config) -> bool {
    ready_marker().is_some_and(|marker| marker.endpoints == config.endpoint_ports())
        && status(config).iter().all(|tap| tap.exists)
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

/// Render a command line with POSIX shell quoting for the pre-sudo echo.
fn render_command(program: &str, args: &[OsString]) -> String {
    std::iter::once(OsStr::new(program))
        .chain(args.iter().map(OsString::as_os_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &OsStr) -> String {
    let raw = value.to_string_lossy();
    if !raw.is_empty()
        && raw.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        raw.into_owned()
    } else {
        format!("'{}'", raw.replace('\'', r#"'\''"#))
    }
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Which host firewall the TAP pool has to be exempted from.
///
/// `ufw status` and `iptables -S` need root, so this is only meaningful inside
/// [`setup`]. The result is recorded in the marker for reporting; it never
/// decides whether the guard is ready, so an unprivileged reader (doctor,
/// `network status`) does not get a false negative. [`setup_script`] repeats
/// the same checks at apply time, which keeps the `--dry-run` output honest.
pub fn detect_host_firewall() -> HostFirewall {
    if command_exists("ufw")
        && command_stdout("ufw", &["status"])
            .is_some_and(|out| out.lines().any(|line| line.trim() == "Status: active"))
    {
        return HostFirewall::Ufw;
    }
    if command_exists("firewall-cmd")
        && command_stdout("firewall-cmd", &["--state"]).is_some_and(|out| out.trim() == "running")
    {
        return HostFirewall::Firewalld;
    }
    if command_exists("iptables")
        && command_stdout("iptables", &["-w", "-S", "FORWARD"])
            .is_some_and(|out| out.lines().any(|line| line.trim() == "-P FORWARD DROP"))
    {
        return HostFirewall::Iptables;
    }
    HostFirewall::None
}

/// Fail closed when guarded networking is configured but not applied.
pub fn ensure_ready(config: &Config) -> Result<()> {
    if crate::config::NetworkMode::Guarded != config.network {
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

/// Reserve a TAP slice for an instance.
///
/// The instance's recorded index is preferred so a snapshot restored later sees
/// the same device name. If the recorded owner is another running instance, or
/// the pool is exhausted, the start fails instead of silently sharing a TAP.
pub fn acquire(
    config: &Config,
    instance: &crate::instance_config::InstanceConfig,
) -> Result<Lease> {
    ensure_ready(config)?;
    fs::create_dir_all(network_dir(config))
        .with_context(|| format!("failed to create {}", network_dir(config).display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(network_dir(config), fs::Permissions::from_mode(0o700))?;
    }

    let preferred = instance.network_index;
    if let Some(index) = preferred {
        if index >= TAP_POOL {
            bail!(
                "instance '{}' records network index {index}, but the TAP pool has {TAP_POOL} slots",
                instance.name
            );
        }
        if !tap_exists(index) {
            bail!(
                "guarded TAP {} is missing; run `sudo hegn network setup --user $USER`",
                tap_name(index)
            );
        }
        return try_claim(config, index, &instance.name)?.with_context(|| {
            format!(
                "guarded TAP {} is reserved by another running instance",
                tap_name(index)
            )
        });
    }

    for index in 0..TAP_POOL {
        if !tap_exists(index) {
            continue;
        }
        if let Some(lease) = try_claim(config, index, &instance.name)? {
            return Ok(lease);
        }
    }
    bail!(
        "no free guarded-network TAP is available for instance '{}'; increase the pool or stop \
         another instance",
        instance.name
    )
}

fn try_claim(config: &Config, index: u8, instance: &str) -> Result<Option<Lease>> {
    let path = owner_path(config, index);
    let lock = lock_path(config, index);
    let _guard = FileLock::acquire(&lock)?;

    if let Some((owner, pid)) = read_owner(&path)
        && !owner.is_empty()
        && owner != instance
    {
        // A live hegn process may still be booting the owner VM, before its
        // session record exists. A live VM is equally authoritative.
        if crate::instance::process_alive(pid) || instance_is_running(config, &owner) {
            return Ok(None);
        }
        // A crashed hegn process and a stopped VM left the reservation behind.
        let _ = fs::remove_file(&path);
    }

    let lease = Lease {
        index,
        instance: instance.to_string(),
        tap: tap_name(index),
        mac: mac(index),
        host_ip: host_ip(index),
        guest_ip: guest_ip(index),
    };
    write_private(&path, &format!("{instance}\n{}\n", std::process::id()))?;
    Ok(Some(lease))
}

/// Release every reservation held by an instance, including stale ones.
pub fn release(config: &Config, instance: &str) {
    // Hold the per-TAP lock while checking the owner: another hegn process may
    // be claiming the same slice concurrently, and removing its fresh owner
    // file would let a third instance share the TAP.
    for index in 0..TAP_POOL {
        let path = owner_path(config, index);
        if !path.exists() {
            continue;
        }
        let Ok(_guard) = FileLock::acquire(&lock_path(config, index)) else {
            continue;
        };
        if read_owner(&path).is_some_and(|(owner, _)| owner == instance) {
            let _ = fs::remove_file(&path);
        }
    }
}

pub fn instance_is_running(config: &Config, instance: &str) -> bool {
    InstanceStore::new(config.clone())
        .load(instance)
        .map(|instance_config| InstanceManager::new(config.clone(), instance_config).is_running())
        .unwrap_or(false)
}

/// Owner files contain `instance\npid\n`. The pid covers the window between
/// reservation and the session record becoming visible.
fn read_owner(path: &Path) -> Option<(String, u32)> {
    let raw = fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();
    let instance = lines.next()?.trim().to_string();
    let pid = lines
        .next()
        .and_then(|line| line.trim().parse().ok())
        .unwrap_or(0);
    Some((instance, pid))
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
/// `endpoints` are the TCP ports exposed from the host loopback; ports below
/// 1024 are rejected by [`crate::config::validate_local_endpoints`] before this
/// function is reached. The script also exempts the TAP pool from the host
/// firewall's routed policy; see [`host_firewall_coexistence`].
pub fn setup_script(user: &str, endpoints: &[u16]) -> Result<String> {
    if !safe_user(user) {
        bail!("invalid user name for TAP ownership: {user:?}");
    }
    for port in endpoints {
        if *port < 1024 {
            bail!("endpoint port {port} is privileged and cannot be exposed");
        }
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
        if !endpoints.is_empty() {
            // Required for DNAT to 127.0.0.1: without it the kernel drops the
            // translated packet as a martian destination. Scoped to this TAP,
            // and the input chain still only accepts DNATed endpoint traffic.
            script.push_str(&format!(
                "sysctl -q -w net.ipv4.conf.{tap}.route_localnet=1\n"
            ));
        }
    }
    script.push_str(
        "nft delete table inet hegn 2>/dev/null || true\n\
         nft -f - <<'HEGN_NFT'\n",
    );
    script.push_str(&nft_rules(endpoints));
    script.push_str("HEGN_NFT\n");
    script.push_str(&host_firewall_coexistence());
    Ok(script)
}

/// Exempt the TAP pool from the host firewall that owns the `forward` hook.
///
/// netfilter applies every base chain at a hook, so hegn's `accept` cannot
/// overrule a foreign `policy drop`. The exemption is intentionally a blanket
/// per-interface accept: duplicating the port allow-list in every host firewall
/// would create a second place to keep in sync, and hegn's own table still has
/// to accept the packet. The checks are repeated here (instead of trusting
/// [`detect_host_firewall`]) so `hegn network setup --dry-run` prints exactly
/// what a real run would do.
fn host_firewall_coexistence() -> String {
    let mut script = String::from(
        "# A host firewall with its own `forward` policy drop wins over hegn's\n\
         # accept, so exempt the TAP pool from whichever firewall is active.\n\
         # hegn's table above stays the only allow-list; this only stops the\n\
         # host firewall from dropping what hegn already accepted.\n\
         hegn_firewall_handled=0\n\
         if command -v ufw >/dev/null 2>&1 && \\\n\
         \x20  ufw status 2>/dev/null | grep -qx 'Status: active'; then\n\
         \x20 hegn_firewall_handled=1\n",
    );
    for index in 0..TAP_POOL {
        script.push_str(&format!(
            "  ufw route allow in on {} >/dev/null\n",
            tap_name(index)
        ));
    }
    script.push_str(
        "fi\n\
         if command -v firewall-cmd >/dev/null 2>&1 && \\\n\
         \x20  firewall-cmd --state >/dev/null 2>&1; then\n\
         \x20 hegn_firewall_handled=1\n",
    );
    for index in 0..TAP_POOL {
        let tap = tap_name(index);
        // Query first so a failure means a real error instead of the normal
        // "rule already exists" case: setup must not report success while the
        // guest stays unreachable.
        script.push_str(&format!(
            "  firewall-cmd --quiet --permanent --direct --query-rule ipv4 filter FORWARD 0 \
             -i {tap} -j ACCEPT || \\\n\
             \x20   firewall-cmd --quiet --permanent --direct --add-rule ipv4 filter FORWARD 0 \
             -i {tap} -j ACCEPT\n\
             \x20 firewall-cmd --quiet --direct --query-rule ipv4 filter FORWARD 0 -i {tap} \
             -j ACCEPT || \\\n\
             \x20   firewall-cmd --quiet --direct --add-rule ipv4 filter FORWARD 0 -i {tap} \
             -j ACCEPT\n"
        ));
    }
    script.push_str(
        "fi\n\
         if [ \"$hegn_firewall_handled\" -eq 0 ] && command -v iptables >/dev/null 2>&1 && \\\n\
         \x20  iptables -w -S FORWARD 2>/dev/null | grep -qx -- '-P FORWARD DROP'; then\n",
    );
    for index in 0..TAP_POOL {
        let tap = tap_name(index);
        script.push_str(&format!(
            "  iptables -w -C FORWARD -i {tap} -j ACCEPT 2>/dev/null || \\\n\
             \x20   iptables -w -I FORWARD 1 -i {tap} -j ACCEPT\n"
        ));
    }
    script.push_str("fi\n");
    script
}

pub fn teardown_script() -> String {
    let mut script = String::from("nft delete table inet hegn 2>/dev/null || true\n");
    for index in 0..TAP_POOL {
        script.push_str(&format!(
            "ip link del dev {} 2>/dev/null || true\n",
            tap_name(index)
        ));
    }
    script.push_str(&host_firewall_cleanup());
    script.push_str("rm -f /run/hegn/network.json\n");
    script
}

/// Symmetric cleanup for [`host_firewall_coexistence`]. Missing rule sets and
/// absent tools are ignored; teardown must always reach the TAP deletion.
fn host_firewall_cleanup() -> String {
    let mut script = String::new();
    for index in 0..TAP_POOL {
        script.push_str(&format!(
            "ufw route delete allow in on {} >/dev/null 2>&1 || true\n",
            tap_name(index)
        ));
    }
    for index in 0..TAP_POOL {
        let tap = tap_name(index);
        script.push_str(&format!(
            "firewall-cmd --quiet --permanent --direct --remove-rule ipv4 filter FORWARD 0 \
             -i {tap} -j ACCEPT >/dev/null 2>&1 || true\n\
             firewall-cmd --quiet --direct --remove-rule ipv4 filter FORWARD 0 -i {tap} \
             -j ACCEPT >/dev/null 2>&1 || true\n"
        ));
    }
    for index in 0..TAP_POOL {
        script.push_str(&format!(
            "iptables -w -D FORWARD -i {} -j ACCEPT >/dev/null 2>&1 || true\n",
            tap_name(index)
        ));
    }
    script
}

/// nftables rules. Public HTTP(S) and public DNS only; everything else from a
/// instance TAP is dropped, including RFC1918, link-local, CGNAT, multicast and
/// all IPv6.
///
/// `endpoints` adds a DNAT from the instance's own TAP host address to the host
/// loopback on the same port, plus an input accept for those translated flows.
/// The rules are static, so changing the list requires re-running setup.
pub fn nft_rules(endpoints: &[u16]) -> String {
    let interfaces = (0..TAP_POOL)
        .map(|index| format!("\"{}\"", tap_name(index)))
        .collect::<Vec<_>>()
        .join(", ");
    let hosts = (0..TAP_POOL)
        .map(|index| host_ip(index).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let dns = DNS
        .iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut ports: Vec<u16> = endpoints.to_vec();
    ports.sort_unstable();
    ports.dedup();
    let endpoint_ports = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let has_endpoints = !ports.is_empty();

    let mut rules = format!(
        "table inet hegn {{\n\
         \x20 set hegn_ifaces {{\n\
         \x20   type ifname\n\
         \x20   flags constant\n\
         \x20   elements = {{ {interfaces} }}\n\
         \x20 }}\n\
         \x20 set hegn_hosts {{\n\
         \x20   type ipv4_addr\n\
         \x20   flags interval\n\
         \x20   elements = {{ {hosts} }}\n\
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
         \x20 }}\n"
    );
    if has_endpoints {
        rules.push_str(&format!(
            "\x20 set endpoint_ports {{\n\
             \x20   type inet_service\n\
             \x20   flags constant\n\
             \x20   elements = {{ {endpoint_ports} }}\n\
             \x20 }}\n\
             \x20 chain prerouting {{\n\
             \x20   type nat hook prerouting priority dstnat; policy accept;\n\
             \x20   iifname @hegn_ifaces ip daddr @hegn_hosts ip protocol tcp \\\n\
             \x20     tcp dport @endpoint_ports counter dnat ip to 127.0.0.1\n\
             \x20 }}\n"
        ));
    }
    rules.push_str(
        "\x20 chain input {\n\
         \x20   type filter hook input priority filter; policy accept;\n",
    );
    if has_endpoints {
        rules.push_str(
            "\x20   iifname @hegn_ifaces ct status dnat ip protocol tcp \\\n\
             \x20     tcp dport @endpoint_ports counter accept\n",
        );
    }
    rules.push_str(
        "\x20   iifname @hegn_ifaces counter drop\n\
         \x20 }\n\
         \x20 chain forward {\n\
         \x20   type filter hook forward priority filter; policy accept;\n\
         \x20   iifname @hegn_ifaces ip saddr != 10.213.0.0/24 counter drop\n\
         \x20   iifname @hegn_ifaces meta nfproto ipv6 counter drop\n\
         \x20   iifname @hegn_ifaces ip daddr @private_v4 counter drop\n\
         \x20   iifname @hegn_ifaces ct state established,related accept\n\
         \x20   iifname @hegn_ifaces ip protocol tcp tcp dport { 80, 443 } accept\n\
         \x20   iifname @hegn_ifaces ip protocol tcp tcp dport 53 ip daddr @web_dns accept\n\
         \x20   iifname @hegn_ifaces ip protocol udp udp dport 53 ip daddr @web_dns accept\n\
         \x20   iifname @hegn_ifaces counter limit rate 10/minute burst 20 packets \\\n\
         \x20     log prefix \"hegn-egress-drop \" level warn drop\n\
         \x20   # No unsolicited inbound connections from the LAN or internet.\n\
         \x20   # Conntrack first un-NATs replies, so established traffic is allowed.\n\
         \x20   ip daddr 10.213.0.0/24 ct state established,related accept\n\
         \x20   ip daddr 10.213.0.0/24 counter drop\n\
         \x20 }\n\
         \x20 chain postrouting {\n\
         \x20   type nat hook postrouting priority srcnat; policy accept;\n\
         \x20   ip saddr 10.213.0.0/24 masquerade\n\
         \x20 }\n\
         }\n",
    );
    rules
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

/// The marker is deliberately world-readable: the unprivileged TUI reads it to
/// decide whether the host guard is active. It is not secret (the rules are
/// already visible with `nft list` to root), and the file must stay root-owned.
fn write_marker(path: &Path, marker: &ReadyMarker) -> Result<()> {
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
        let rules = nft_rules(&[]);
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
        assert!(rules.contains("tcp dport { 80, 443 } accept"), "{rules}");
        assert!(rules.contains("udp dport 53 ip daddr @web_dns accept"));
        assert!(rules.contains("iifname @hegn_ifaces counter drop"));
        assert!(rules.contains("masquerade"));
        assert!(!rules.contains("tcp dport 22"));
    }

    #[test]
    fn endpoint_ports_are_dnated_to_the_host_loopback() {
        let rules = nft_rules(&[11434, 1234, 11434]);
        assert!(
            rules.contains("elements = { 1234, 11434 }"),
            "ports must be sorted and deduplicated:\n{rules}"
        );
        assert!(
            rules.contains("iifname @hegn_ifaces ip daddr @hegn_hosts ip protocol tcp \\"),
            "{rules}"
        );
        assert!(
            rules.contains("dnat ip to 127.0.0.1"),
            "endpoint traffic must be DNATed:\n{rules}"
        );
        assert!(
            rules.contains("ct status dnat ip protocol tcp \\"),
            "only translated endpoint flows may reach the host input chain:\n{rules}"
        );
        // The endpoint accept must precede the catch-all drop.
        let accept = rules.find("ct status dnat").unwrap();
        let drop = rules.find("iifname @hegn_ifaces counter drop").unwrap();
        assert!(
            accept < drop,
            "endpoint accept must precede the drop:\n{rules}"
        );
    }

    #[test]
    fn route_localnet_is_only_enabled_with_endpoints() {
        let plain = setup_script("alice", &[]).unwrap();
        assert!(!plain.contains("route_localnet"), "{plain}");

        let exposed = setup_script("alice", &[11434]).unwrap();
        assert!(
            exposed.contains("net.ipv4.conf.hegn0.route_localnet=1"),
            "{exposed}"
        );
        assert!(exposed.contains("tcp dport @endpoint_ports"), "{exposed}");
    }

    #[test]
    fn privileged_endpoint_ports_are_rejected() {
        assert!(setup_script("alice", &[22]).is_err());
        assert!(setup_script("alice", &[0]).is_err());
    }

    #[test]
    fn setup_script_creates_every_pool_tap() {
        let script = setup_script("alice", &[]).unwrap();
        for index in 0..TAP_POOL {
            assert!(script.contains(&format!(
                "ip tuntap add dev {} mode tap user alice",
                tap_name(index)
            )));
        }
        assert!(script.contains("nft -f -"));
    }

    #[test]
    fn setup_script_exempts_the_tap_pool_from_the_host_firewall() {
        let script = setup_script("alice", &[]).unwrap();
        // The carve-out only ever runs when the host firewall is active, so a
        // plain host without one is left untouched.
        assert!(script.contains("ufw status 2>/dev/null | grep -qx 'Status: active'"));
        assert!(script.contains("firewall-cmd --state >/dev/null 2>&1"));
        assert!(script.contains("grep -qx -- '-P FORWARD DROP'"));
        for index in 0..TAP_POOL {
            let tap = tap_name(index);
            assert!(
                script.contains(&format!("ufw route allow in on {tap} >/dev/null")),
                "missing ufw exemption for {tap}"
            );
            assert!(
                script.contains(&format!(
                    "firewall-cmd --quiet --direct --add-rule ipv4 filter FORWARD 0 -i {tap} \
                     -j ACCEPT"
                )),
                "missing firewalld exemption for {tap}"
            );
            assert!(
                script.contains(&format!("iptables -w -I FORWARD 1 -i {tap} -j ACCEPT")),
                "missing iptables exemption for {tap}"
            );
        }
        // The guard itself must stay the only allow-list.
        assert!(script.contains("tcp dport { 80, 443 } accept"), "{script}");
    }

    #[test]
    fn teardown_script_removes_the_host_firewall_exemptions() {
        let script = teardown_script();
        for index in 0..TAP_POOL {
            let tap = tap_name(index);
            assert!(script.contains(&format!("ufw route delete allow in on {tap}")));
            assert!(script.contains(&format!(
                "firewall-cmd --quiet --direct --remove-rule ipv4 filter FORWARD 0 -i {tap} \
                 -j ACCEPT"
            )));
            assert!(script.contains(&format!("iptables -w -D FORWARD -i {tap} -j ACCEPT")));
            assert!(script.contains(&format!("ip link del dev {tap}")));
        }
        assert!(script.ends_with("rm -f /run/hegn/network.json\n"));
    }

    #[test]
    fn setup_and_teardown_scripts_are_valid_shell() {
        for script in [
            setup_script("alice", &[]).unwrap(),
            setup_script("alice", &[11434]).unwrap(),
            teardown_script(),
        ] {
            let parsed = Command::new("sh")
                .args(["-n", "-c", &script])
                .status()
                .expect("sh must be available");
            assert!(parsed.success(), "shell syntax error:\n{script}");
        }
    }

    #[test]
    fn host_firewall_labels_and_serde_stay_in_sync() {
        for (firewall, label) in [
            (HostFirewall::None, "none"),
            (HostFirewall::Ufw, "ufw"),
            (HostFirewall::Firewalld, "firewalld"),
            (HostFirewall::Iptables, "iptables"),
        ] {
            assert_eq!(firewall.label(), label);
            assert_eq!(
                serde_json::to_string(&firewall).unwrap(),
                format!("\"{label}\"")
            );
            let parsed: HostFirewall = serde_json::from_str(&format!("\"{label}\"")).unwrap();
            assert_eq!(parsed, firewall);
        }
    }

    #[test]
    fn rejects_unsafe_tap_owners() {
        for bad in ["root; reboot", "$(id)", "-rf", "a/b", ""] {
            assert!(setup_script(bad, &[]).is_err(), "{bad:?} must be rejected");
        }
        assert!(safe_user("_alice-1"));
    }

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

    #[test]
    fn privileged_setup_echo_is_shell_quoted() {
        let args = vec![
            OsString::from("--"),
            OsString::from("/opt/hegn tools/hegn"),
            OsString::from("network"),
            OsString::from("setup"),
            OsString::from("--user"),
            OsString::from("demo user"),
        ];
        assert_eq!(
            render_command("sudo", &args),
            "sudo -- '/opt/hegn tools/hegn' network setup --user 'demo user'"
        );
        assert_eq!(
            render_command("pkexec", &args[1..]),
            "pkexec '/opt/hegn tools/hegn' network setup --user 'demo user'"
        );
    }

    #[test]
    fn guest_config_matches_the_reserved_slice() {
        let lease = Lease {
            index: 3,
            instance: "demo".into(),
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
