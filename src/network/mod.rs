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

use std::net::Ipv4Addr;

mod addressing;
mod firewall;
mod marker;
mod pool;
mod privilege;
mod script;

pub use addressing::{guest_ip, host_ip, mac, tap_name};
pub use firewall::{HostFirewall, detect_host_firewall, live_guard_status, nft_rules};
pub use marker::{ReadyMarker, guard_ready, marker_path, ready_marker};
pub use pool::{Lease, TapStatus, acquire, instance_is_running, release, status};
pub use privilege::{
    ensure_ready, invoking_user, is_privileged, reload, safe_user, setup, setup_with_privilege,
    teardown,
};
pub use script::{reload_script, setup_script, teardown_script};

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
