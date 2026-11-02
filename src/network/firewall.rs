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

    #[test]
    fn every_non_public_ipv4_range_is_dropped() {
        let rules = nft_rules(&[]);
        for range in [
            "0.0.0.0/8",
            "10.0.0.0/8",
            "100.64.0.0/10",
            "127.0.0.0/8",
            "169.254.0.0/16",
            "172.16.0.0/12",
            "192.0.0.0/24",
            "192.0.2.0/24",
            "192.168.0.0/16",
            "198.18.0.0/15",
            "198.51.100.0/24",
            "203.0.113.0/24",
            "224.0.0.0/4",
            "240.0.0.0/4",
        ] {
            assert!(
                rules.contains(range),
                "missing private range {range}:\n{rules}"
            );
        }
        assert!(rules.contains("set private_v4 {"), "{rules}");
    }

    #[test]
    fn guest_egress_can_only_reach_public_web_and_public_dns() {
        let rules = nft_rules(&[]);
        let accepts = rules
            .lines()
            .filter(|line| line.contains("iifname") && line.contains(" accept"))
            .collect::<Vec<_>>();
        assert_eq!(
            accepts.len(),
            4,
            "the forward path may only accept conntrack, 80/443 and public DNS:\n{}",
            accepts.join("\n")
        );

        assert!(
            rules.contains("ct state established,related accept"),
            "{rules}"
        );
        assert!(rules.contains("tcp dport { 80, 443 } accept"), "{rules}");
        assert!(
            rules.contains("tcp dport 53 ip daddr @web_dns accept"),
            "{rules}"
        );
        assert!(
            rules.contains("udp dport 53 ip daddr @web_dns accept"),
            "{rules}"
        );

        // The DNS allow-list is the two public resolvers, not arbitrary DNS.
        assert!(rules.contains("elements = { 1.1.1.1, 9.9.9.9 }"), "{rules}");
        for forbidden in ["8.8.8.8", "tcp dport 22", "udp dport 123", "dport 25"] {
            assert!(
                !rules.contains(forbidden),
                "must not allow {forbidden}:\n{rules}"
            );
        }
    }

    #[test]
    fn ipv6_and_private_destinations_are_dropped_before_any_accept() {
        let rules = nft_rules(&[]);
        let ipv6 = rules.find("meta nfproto ipv6 counter drop").unwrap();
        let private = rules.find("ip daddr @private_v4 counter drop").unwrap();
        let established = rules.find("ct state established,related accept").unwrap();
        let web = rules.find("tcp dport { 80, 443 } accept").unwrap();
        assert!(ipv6 < established, "{rules}");
        assert!(private < established, "{rules}");
        assert!(private < web, "{rules}");
    }

    #[test]
    fn guests_cannot_initiate_connections_to_the_host_or_lan() {
        let rules = nft_rules(&[]);
        // Anything entering the host from a TAP that is not a DNATed endpoint
        // flow hits the input chain's catch-all drop.
        assert!(
            rules.contains("iifname @hegn_ifaces counter drop"),
            "{rules}"
        );
        // The TAP host addresses themselves are inside RFC1918 and therefore
        // unreachable from the guest's forward path.
        assert!(rules.contains("iifname @hegn_ifaces ip daddr @private_v4 counter drop"));
        for index in 0..TAP_POOL {
            assert!(rules.contains(&host_ip(index).to_string()), "{rules}");
        }
        // Unsolicited LAN/internet traffic to the guest subnet is dropped, while
        // replies to guest-initiated flows stay allowed.
        let inbound_accept = rules
            .find("ip daddr 10.213.0.0/24 ct state established,related accept")
            .unwrap();
        let inbound_drop = rules.find("ip daddr 10.213.0.0/24 counter drop").unwrap();
        assert!(inbound_accept < inbound_drop, "{rules}");
    }

    #[test]
    fn no_endpoints_means_no_dnat_or_host_input_accept() {
        let rules = nft_rules(&[]);
        assert!(!rules.contains("endpoint_ports"), "{rules}");
        assert!(!rules.contains("dnat"), "{rules}");
        assert!(!rules.contains("prerouting"), "{rules}");
        assert!(!rules.contains("ct status dnat"), "{rules}");
    }

    /// Rules wrap with `\` line continuations, so multi-token assertions have
    /// to look at the logical line rather than a physical one.
    fn one_line(rules: &str) -> String {
        rules
            .replace("\\\n", " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn dnat_is_scoped_to_pool_host_addresses_and_endpoint_ports() {
        let rules = nft_rules(&[11434, 4321, 11434]);
        let flat = one_line(&rules);
        assert_eq!(
            flat.matches("dnat ip to 127.0.0.1").count(),
            1,
            "one DNAT rule:\n{rules}"
        );
        let (before, _) = flat.split_once("dnat ip to").unwrap();
        let dnat_rule = before.rsplit("iifname").next().unwrap();
        assert!(dnat_rule.contains("ip daddr @hegn_hosts"), "{dnat_rule}");
        assert!(dnat_rule.contains("ip protocol tcp"), "{dnat_rule}");
        assert!(
            dnat_rule.contains("tcp dport @endpoint_ports"),
            "{dnat_rule}"
        );
        // It must not rewrite arbitrary destinations to the loopback.
        assert!(!dnat_rule.contains("0.0.0.0"), "{dnat_rule}");
        assert!(!dnat_rule.contains("10.0.0.0"), "{dnat_rule}");

        let prerouting = rules.find("chain prerouting").unwrap();
        let dnat = rules.find("dnat ip to").unwrap();
        let input = rules.find("chain input").unwrap();
        assert!(prerouting < dnat && dnat < input, "{rules}");
        assert!(rules.contains("elements = { 4321, 11434 }"), "{rules}");
    }

    #[test]
    fn endpoint_input_accept_requires_translated_flows_and_precedes_the_drop() {
        let rules = nft_rules(&[11434]);
        let flat = one_line(&rules);
        let expected = "iifname @hegn_ifaces ct status dnat ip protocol tcp tcp dport \
                        @endpoint_ports counter accept";
        assert!(
            flat.contains(expected),
            "missing conntrack-gated endpoint accept:\n{rules}"
        );

        let accept_pos = rules.find("ct status dnat").unwrap();
        let drop_pos = rules.find("iifname @hegn_ifaces counter drop").unwrap();
        assert!(accept_pos < drop_pos, "{rules}");
    }

    #[test]
    fn pool_sets_cover_every_tap_and_host_address() {
        let rules = nft_rules(&[]);
        for index in 0..TAP_POOL {
            assert!(
                rules.contains(&format!("\"{}\"", tap_name(index))),
                "missing {}:\n{rules}",
                tap_name(index)
            );
            assert!(
                rules.contains(host_ip(index).to_string().as_str()),
                "missing host address for {}:\n{rules}",
                tap_name(index)
            );
        }
    }

    #[test]
    fn masquerade_is_scoped_to_the_guest_subnet() {
        let rules = nft_rules(&[]);
        assert!(
            rules.contains("ip saddr 10.213.0.0/24 masquerade"),
            "{rules}"
        );
        assert_eq!(rules.matches("masquerade").count(), 1, "{rules}");
        assert!(!rules.contains("oifname"), "{rules}");
    }

    #[test]
    fn egress_drops_are_rate_limited_and_logged() {
        let rules = nft_rules(&[]);
        let log = rules
            .find("log prefix \"hegn-egress-drop \" level warn drop")
            .unwrap();
        let web = rules.find("tcp dport { 80, 443 } accept").unwrap();
        assert!(
            web < log,
            "the drop must come after the allow-list:\n{rules}"
        );
        assert!(
            rules.contains("counter limit rate 10/minute burst 20 packets"),
            "{rules}"
        );
    }

    #[test]
    fn host_firewall_carve_out_only_runs_for_active_firewalls() {
        let script = host_firewall_coexistence();
        assert!(script.contains("ufw status 2>/dev/null | grep -qx 'Status: active'"));
        assert!(script.contains("firewall-cmd --state >/dev/null 2>&1"));
        assert!(script.contains("grep -qx -- '-P FORWARD DROP'"));
        // The plain-iptables branch must not run when ufw/firewalld was handled.
        assert!(script.contains("hegn_firewall_handled=0"));
        assert!(script.contains("[ \"$hegn_firewall_handled\" -eq 0 ]"));

        assert_eq!(
            script.matches("ufw route allow in on hegn").count(),
            TAP_POOL as usize
        );
        assert_eq!(
            script
                .matches("--quiet --direct --add-rule ipv4 filter FORWARD")
                .count(),
            TAP_POOL as usize
        );
        assert_eq!(
            script
                .matches("--quiet --permanent --direct --add-rule ipv4 filter FORWARD")
                .count(),
            TAP_POOL as usize
        );
        assert_eq!(
            script.matches("iptables -w -I FORWARD 1 -i hegn").count(),
            TAP_POOL as usize
        );
        // Never insert an iptables rule without checking for it first.
        for line in script.lines().filter(|line| line.contains("-I FORWARD 1")) {
            let check = line.trim().replace("-I FORWARD 1", "-C FORWARD");
            assert!(
                script.contains(check.as_str()),
                "missing idempotence check for {line}"
            );
        }
    }

    #[test]
    fn host_firewall_cleanup_is_symmetric_and_cannot_abort_teardown() {
        let script = host_firewall_cleanup();
        assert_eq!(
            script.matches("ufw route delete allow in on hegn").count(),
            TAP_POOL as usize
        );
        assert_eq!(
            script
                .matches("--direct --remove-rule ipv4 filter FORWARD")
                .count(),
            TAP_POOL as usize * 2
        );
        assert_eq!(
            script.matches("iptables -w -D FORWARD -i hegn").count(),
            TAP_POOL as usize
        );
        assert!(!script.contains("--add-rule"), "{script}");
        for line in script.lines().filter(|line| !line.trim().is_empty()) {
            assert!(
                line.ends_with("|| true"),
                "cleanup could abort teardown: {line}"
            );
        }
    }

    #[test]
    fn default_and_unknown_host_firewalls_are_none() {
        assert_eq!(HostFirewall::default(), HostFirewall::None);
        assert!(serde_json::from_str::<HostFirewall>("\"nftables\"").is_err());
        assert!(serde_json::from_str::<HostFirewall>("\"firewalld \"").is_err());
    }

    #[test]
    fn nft_rules_are_deterministic() {
        assert_eq!(nft_rules(&[11434]), nft_rules(&[11434]));
    }
}
