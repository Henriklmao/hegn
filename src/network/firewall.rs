//! Host-firewall detection and generation of the nftables guard rules.

use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use super::addressing::{host_ip, tap_name};
use super::{DNS, TAP_POOL};

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

/// Which host firewall the TAP pool has to be exempted from.
///
/// `ufw status` and `iptables -S` need root, so this is only meaningful inside
/// [`crate::network::setup`]. The result is recorded in the marker for
/// reporting; it never decides whether the guard is ready, so an unprivileged
/// reader (doctor, `network status`) does not get a false negative.
/// [`crate::network::setup_script`] repeats the same checks at apply time, which
/// keeps the `--dry-run` output honest.
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

pub(super) fn command_exists(name: &str) -> bool {
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

/// Exempt the TAP pool from the host firewall that owns the `forward` hook.
///
/// netfilter applies every base chain at a hook, so hegn's `accept` cannot
/// overrule a foreign `policy drop`. The exemption is intentionally a blanket
/// per-interface accept: duplicating the port allow-list in every host firewall
/// would create a second place to keep in sync, and hegn's own table still has
/// to accept the packet. The checks are repeated here (instead of trusting
/// [`detect_host_firewall`]) so `hegn network setup --dry-run` prints exactly
/// what a real run would do.
pub(super) fn host_firewall_coexistence() -> String {
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

/// Symmetric cleanup for [`host_firewall_coexistence`]. Missing rule sets and
/// absent tools are ignored; teardown must always reach the TAP deletion.
pub(super) fn host_firewall_cleanup() -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
