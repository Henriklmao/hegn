# Networking

hegn gives each VM a network connection that is limited on purpose. The guest can
reach the public web, but not your local network or your host. This page
describes the setup, how to expose local services such as Ollama to the guest,
and how to fix common problems.

- [The guarded network](#the-guarded-network)
- [One-time setup](#one-time-setup)
- [Host firewalls](#host-firewalls)
- [Local LLM endpoints](#local-llm-endpoints)
- [Troubleshooting: the guest has no network](#troubleshooting-the-guest-has-no-network)

---

## The guarded network

With `network = "guarded"` (the default), each VM gets its own TAP device from a
pool that the host creates. The host firewall (nftables) decides what the guest
may do:

| Traffic | Allowed? |
| --- | --- |
| Outgoing HTTP and HTTPS (TCP 80 and 443) | yes |
| DNS to `1.1.1.1` and `9.9.9.9` | yes |
| Local network (RFC1918, CGNAT, link-local, multicast) | no |
| IPv6 | no |
| Guest to host | no |
| Incoming connections | no |

Packets with a forged source address are dropped as well. The guest's only route
goes through the host's `/30` gateway. Because the rules live on the host, root
inside the guest cannot switch them off.

With `network = "off"`, the guest gets no network card at all.

Presets install their tools inside the guest on first boot, so they need this
network. The first-boot provisioner waits for DNS and retries `apt`.

---

## One-time setup

The network needs root rights once:

```bash
sudo hegn network setup --user "$USER"   # TAP pool + nftables table + /run marker
hegn network status                      # marker, taps and reservations
hegn network setup --dry-run --user "$USER"  # inspect the exact script first
sudo hegn network reload                 # rules only; TAPs stay up
sudo hegn network teardown               # remove taps and rules
```

`hegn doctor --fix` runs the setup for you and asks for `sudo` or `pkexec`. It
prints the exact command before the password prompt, so you can read it first.

`hegn instance start` is fail-closed. With `network = "guarded"`, it refuses to
boot unless `/run/hegn/network.json` exists and was written by root.

The marker file is only a receipt. Setup writes it only when the live nftables
table matches the expected rules. `hegn doctor`, run as root, reads the table
directly and does not trust the JSON file. A flushed table therefore fails
safely.

---

## Host firewalls

A host firewall with its own routing rules (ufw, firewalld or plain iptables)
could silently drop the guest's traffic, even though hegn allows it. So setup
also exempts the TAP pool from that firewall:

- ufw `route` rules,
- firewalld direct rules,
- `FORWARD` accepts for plain iptables.

This exemption is a blanket accept for the TAP interfaces in the host firewall
only. The HTTP(S) and DNS allow-list stays in hegn's own table, so what the guest
can reach does not change.

Run `sudo hegn network setup` again after you install, enable or reset a host
firewall, and after upgrading from a hegn version whose marker is older.

---

## Local LLM endpoints

Tools like Ollama, LM Studio, llama.cpp and vLLM usually listen on the host's
loopback address (`127.0.0.1`). Inside the guest, `127.0.0.1` means the guest
itself. So hegn can expose selected host ports on the TAP address of each
instance:

```toml
[[local_endpoints]]
name = "ollama"   # guest env: HEGN_ENDPOINT_OLLAMA (plus OLLAMA_HOST)
port = 11434      # host target: 127.0.0.1:11434
```

`hegn network setup` adds one DNAT rule per TAP and one narrow input rule. The
guest then uses `http://10.213.0.<n>:11434`.

- Host loopback is never bridged, and other host interfaces stay unreachable.
- Ports below 1024 are rejected.
- Endpoints require `network = "guarded"`.

After you change the list, run `sudo hegn network reload`. This replaces only the
nftables rules. The TAP pool and running guests are not touched. A full
`sudo hegn network setup` also works. Until the root-owned marker matches,
`hegn instance start` fails with a hint.

A full setup recreates any TAP whose owner does not match `--user`. The guest's
link on that TAP is interrupted. This prevents a previous local user from keeping
attach rights.

With `cloud_init = true`, the URLs are exported in the guest through
`/etc/profile.d/hegn-endpoints.sh`. Each endpoint is also available as
`HEGN_ENDPOINT_<NAME>`. `hegn instance status` shows the guest URLs.

---

## Troubleshooting: the guest has no network

Error messages such as `Temporary failure in name resolution` usually mean that a
host firewall blocks the guest's traffic. This happens when the firewall was
installed or reset *after* `hegn network setup`. Its own `forward` rules then
stay in place without hegn's exemption. Netfilter runs every `forward` chain, so
the host firewall drops traffic that hegn had already allowed.

Run the setup again. It is safe to repeat:

```bash
sudo hegn network setup --user "$USER"
hegn doctor            # network guard + guest health from the newest serial logs
```

The kernel log shows which rule dropped a packet:

- `hegn-egress-drop` means hegn's own allow-list blocked it. ICMP, the local
  network, IPv6 and guest-to-host traffic are blocked by design. Test with `curl`
  or `apt`, not with `ping`.
- `[UFW BLOCK]` or `[UFW DOCKER BLOCK]` means the host firewall blocked it.
