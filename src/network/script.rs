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

    #[test]
    fn shell_quoting_neutralizes_injection_metacharacters() {
        for (raw, quoted) in [
            ("a b", "'a b'"),
            ("it's", r"'it'\''s'"),
            ("$(id)", "'$(id)'"),
            ("`id`", "'`id`'"),
            ("a;b", "'a;b'"),
            ("a&&b", "'a&&b'"),
            ("a||b", "'a||b'"),
            ("a|b", "'a|b'"),
            ("a>b", "'a>b'"),
            ("a<b", "'a<b'"),
            ("a*b", "'a*b'"),
            ("a\nb", "'a\nb'"),
            ("$HOME", "'$HOME'"),
            ("~root", "'~root'"),
            ("a\\b", "'a\\b'"),
            // A leading dash is not a shell metacharacter; quoting is not
            // required for it to stay a single literal argument.
            ("--force", "--force"),
            ("", "''"),
            ("safe-._@%+=:,/", "safe-._@%+=:,/"),
        ] {
            assert_eq!(
                shell_quote(OsStr::new(raw)),
                quoted,
                "unexpected quoting for {raw:?}"
            );
        }
    }

    #[test]
    fn quoted_values_survive_a_roundtrip_through_the_shell() {
        // Execute only `printf` in a throwaway shell; no host state is touched.
        for raw in [
            "a b", "it's", "$(id)", "`id`", "a;b", "a&&b", "a|b", "a>b", "a*b", "a\nb", "--force",
            "", "100%", "$HOME", "~root", "a\\b",
        ] {
            let script = format!("printf %s {}", shell_quote(OsStr::new(raw)));
            let output = Command::new("sh")
                .args(["-c", &script])
                .output()
                .expect("sh must be available");
            assert!(output.status.success(), "{raw:?} failed to parse: {script}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                raw,
                "{raw:?} was not passed through as a single literal word"
            );
        }
    }

    #[test]
    fn render_command_quotes_only_what_needs_it() {
        assert_eq!(render_command("sudo", &[]), "sudo");
        assert_eq!(
            render_command("sudo", &[OsString::from("alice")]),
            "sudo alice"
        );
        assert_eq!(
            render_command("sudo", &[OsString::from("a b"), OsString::from("it's")]),
            r"sudo 'a b' 'it'\''s'"
        );
        assert_eq!(
            render_command("/opt/hegn tools/hegn", &[OsString::from("--user")]),
            "'/opt/hegn tools/hegn' --user"
        );
    }

    #[test]
    fn setup_script_embeds_only_the_validated_user_name() {
        let user = "_alice-1.2";
        let script = setup_script(user, &[]).unwrap();
        assert_eq!(
            script.matches(user).count(),
            TAP_POOL as usize,
            "the user name belongs only on the tuntap lines"
        );
        for index in 0..TAP_POOL {
            assert!(script.contains(&format!(
                "ip tuntap add dev {} mode tap user {user}",
                tap_name(index)
            )));
            // Tap ownership must not be granted to root implicitly.
            assert!(!script.contains(&format!(
                "ip tuntap add dev {} mode tap user root",
                tap_name(index)
            )));
        }
    }

    #[test]
    fn endpoint_ports_are_sorted_deduplicated_and_only_safe_ports_are_accepted() {
        let script = setup_script("alice", &[11434, 1234, 11434]).unwrap();
        assert!(
            script.contains("elements = { 1234, 11434 }"),
            "endpoint ports must be sorted and deduplicated:\n{script}"
        );
        assert_eq!(
            script.matches("route_localnet=1").count(),
            TAP_POOL as usize,
            "route_localnet must be enabled for every TAP"
        );
        for index in 0..TAP_POOL {
            assert!(script.contains(&format!(
                "net.ipv4.conf.{}.route_localnet=1",
                tap_name(index)
            )));
        }
        assert!(
            !script.contains("net.ipv4.route_localnet"),
            "route_localnet must never be enabled globally"
        );

        for port in [0u16, 1, 22, 53, 443, 1023] {
            assert!(
                setup_script("alice", &[port]).is_err(),
                "privileged endpoint port {port} must be rejected"
            );
        }
        assert!(setup_script("alice", &[1024]).is_ok());
    }

    #[test]
    fn setup_script_installs_the_guard_after_removing_the_old_table() {
        let script = setup_script("alice", &[]).unwrap();
        assert!(script.starts_with("set -eu\n"), "{script}");
        assert!(script.contains("sysctl -q -w net.ipv4.ip_forward=1"));
        let delete = script.find("nft delete table inet hegn").unwrap();
        let heredoc = script.find("nft -f - <<'HEGN_NFT'").unwrap();
        let close = script.find("\nHEGN_NFT\n").unwrap();
        assert!(
            delete < heredoc && heredoc < close,
            "rules must be replaced atomically through one heredoc:\n{script}"
        );
        assert_eq!(script.matches("HEGN_NFT").count(), 2);
        assert_eq!(script.matches("nft -f -").count(), 1);
    }

    #[test]
    fn teardown_script_only_deletes_known_paths_and_never_fails() {
        let script = teardown_script();
        assert_eq!(
            script.matches("rm -f /run/hegn/network.json").count(),
            1,
            "{script}"
        );
        assert!(
            script.ends_with("rm -f /run/hegn/network.json\n"),
            "{script}"
        );
        assert!(!script.contains("rm -r"), "{script}");
        assert!(!script.contains("rm -rf"), "{script}");
        assert!(!script.contains('*'), "no wildcards in teardown:\n{script}");
        assert_eq!(
            script.matches("network.json").count(),
            1,
            "the marker must be the only JSON path in teardown:\n{script}"
        );
        assert_eq!(
            script.matches("ip link del dev hegn").count(),
            TAP_POOL as usize
        );
        for line in script
            .lines()
            .filter(|line| line.starts_with("ip link del dev hegn"))
        {
            assert!(line.ends_with("2>/dev/null || true"), "{line}");
        }
        assert!(
            script.find("nft delete table inet hegn").unwrap()
                < script.find("ip link del dev hegn0").unwrap(),
            "remove the rules before the devices:\n{script}"
        );
    }

    #[test]
    fn scripts_with_unusual_but_valid_users_are_valid_shell() {
        for user in ["_alice-1.2", "A", "a_b-c.d"] {
            for script in [
                setup_script(user, &[]).unwrap(),
                setup_script(user, &[11434]).unwrap(),
                teardown_script(),
            ] {
                let parsed = Command::new("sh")
                    .args(["-n", "-c", &script])
                    .status()
                    .expect("sh must be available");
                assert!(parsed.success(), "shell syntax error for {user:?}");
            }
        }
    }

    #[test]
    fn run_script_surfaces_stderr_on_failure() {
        assert!(run_script("true").is_ok());
        let err = run_script("echo boom >&2; exit 7").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("network script failed"), "{message}");
        assert!(message.contains("boom"), "{message}");
    }
}
