//! Host-enforced network guard for VMs.
//!
//! The guest never gets a bridge or a route into the host LAN. Every VM gets a
//! slot with a pre-created control TAP for host-only SSH traffic and, in guarded
//! mode, a data TAP for egress. Host nftables rules accept only outbound
//! HTTP(S) and DNS to public resolvers on the data path; every other packet,
//! including all RFC1918/link-local traffic and all IPv6, is dropped before it
//! can be forwarded. Because the rules live on the host, root inside the guest
//! cannot remove them.
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
//! explicit, one-time privileged step for every network mode; `hegn vm start` is
//! fail-closed and refuses to boot any VM unless the host marker shows that the
//! setup is active.

use std::net::Ipv4Addr;

mod addressing;
mod firewall;
mod marker;
mod pool;
mod privilege;
mod script;

pub use addressing::{
    CONTROL_SUBNET, CONTROL_TAP_POOL, control_guest_ip, control_host_ip, control_mac,
    control_tap_name, guest_ip, host_ip, mac, tap_name,
};
pub use firewall::{HostFirewall, detect_host_firewall, live_guard_status, nft_rules};
pub(crate) use firewall::{SAFE_PATH, safe_command_path};
pub use marker::{ReadyMarker, guard_ready, marker_path, ready_marker};
pub use pool::{Lease, NetworkLease, TapLease, TapStatus, acquire, release, status, vm_is_running};
pub use privilege::{
    ensure_ready, invoking_user, is_privileged, reload, safe_user, setup, setup_with_privilege,
    teardown,
};
pub use script::{reload_script, setup_script, teardown_script};

/// TAP devices created by `hegn network setup`.
pub const TAP_POOL: u8 = 16;
/// Root of the isolated VM network. Each TAP gets a `/30` slice.
pub const SUBNET: Ipv4Addr = Ipv4Addr::new(10, 213, 0, 0);
/// Guest netmask for every `/30` slice.
pub const NETMASK: &str = "255.255.255.252";
/// Public resolvers the guest may query. The host LAN DNS is never reachable.
pub const DNS: [Ipv4Addr; 2] = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(9, 9, 9, 9)];
/// Marker written by the privileged setup step. `/run` is cleared on reboot,
/// so a stale marker cannot outlive the nftables rules it vouches for.
pub const MARKER: &str = "/run/hegn/network.json";
/// Bumped only for incompatible rule generations. Version 2 added the
/// host-firewall carve-out; version 3 adds the control TAP pool that every
/// session uses for SSH. A version 2 marker neither records nor vouches for the
/// control devices, so it must never be accepted for a version 3 setup.
const MARKER_VERSION: u32 = 3;
const NETWORK_DIR: &str = "network";
