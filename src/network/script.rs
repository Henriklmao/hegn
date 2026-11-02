//! Shell scripts that apply and remove the guarded-network configuration.

use std::ffi::{OsStr, OsString};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::TAP_POOL;
use super::addressing::{host_ip, mac, tap_name};
use super::firewall::{host_firewall_cleanup, host_firewall_coexistence, nft_rules};
use super::privilege::safe_user;

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

pub(super) fn run_script(script: &str) -> Result<()> {
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

/// Render a command line with POSIX shell quoting for the pre-sudo echo.
pub(super) fn render_command(program: &str, args: &[OsString]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rejects_unsafe_tap_owners() {
        for bad in ["root; reboot", "$(id)", "-rf", "a/b", ""] {
            assert!(setup_script(bad, &[]).is_err(), "{bad:?} must be rejected");
        }
        assert!(safe_user("_alice-1"));
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
}
