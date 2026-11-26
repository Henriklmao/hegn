# Networking

hegn gives each VM two kinds of network link. A **control link** is always
present and carries only SSH between the host and the guest. A **guarded data
link** optionally provides public web and DNS access. Neither link reaches your
local network, and the control link cannot leave the host at all. This page
describes the setup, how to expose local services such as Ollama to the guest,
and how to fix common problems.

- [The control network](#the-control-network)
- [The guarded network](#the-guarded-network)
- [One-time setup](#one-time-setup)
- [nftables rules](#nftables-rules)
- [Host firewalls](#host-firewalls)
- [Local LLM endpoints](#local-llm-endpoints)
- [Troubleshooting](#troubleshooting)

---

## The control network

Every VM gets one control TAP for its slot, independent of `network`:

| Item | Value |
| --- | --- |
| TAP (host) | `hegnc<i>` |
| Subnet | `10.214.0.0/30` per slot |
| Host address | `10.214.0.<4i+1>` |
| Guest address | `10.214.0.<4i+2>` |
| MAC | `52:54:00:00:01:<i+1>` (last byte `01`–`10`, hex) |
| Guest interface | `hegnc0`, matched by MAC in netplan |

`hegn attach` and `hegn open` run OpenSSH against the guest address on port 22.
The guest interface is static, `optional: true`, has **no default route and no
nameservers**, and the host side is never forwarded or masqueraded. nftables
accepts only established replies to host-initiated SSH and drops everything else
arriving on a control TAP. The control link is therefore a management channel,
not an egress path.

`network = "off"` keeps this control TAP, so SSH still works, but removes the
data NIC. The guest has no default route, no DNS and no internet access.
`network_index` is still assigned and persisted because it selects the control
TAP slot.

---

## The guarded network

With `network = "guarded"` (the default), each VM additionally gets a data TAP
from a pool that the host creates:

| Item | Value |
| --- | --- |
| TAP (host) | `hegn<i>` |
| Subnet | `10.213.0.0/30` per slot |
| Host address | `10.213.0.<4i+1>` |
| Guest address | `10.213.0.<4i+2>` |
| MAC | `52:54:00:00:00:<i+1>` (last byte `01`–`10`, hex) |
| Guest interface | `hegn0`, matched by MAC in netplan |

The host firewall (nftables) decides what the guest may do:

| Traffic | Allowed? |
| --- | --- |
| Outgoing HTTP and HTTPS (TCP 80 and 443) on the data TAP | yes |
| DNS to `1.1.1.1` and `9.9.9.9` on the data TAP | yes |
| Local network (RFC1918, CGNAT, link-local, multicast) | no |
| IPv6 | no |
| Guest to host | no |
| Incoming connections | no |
| SSH over the control TAP | yes, independent of the data allow-list |

Packets with a forged source address are dropped as well. The guest's only
default route goes through the data TAP's `/30` gateway. Because the rules live
on the host, root inside the guest cannot switch them off. The control TAP never
has a default route and is never masqueraded, so SSH cannot be used to reach the
host, the local network or the internet.

Presets install their tools inside the guest on first boot, so they need the
guarded data network. The first-boot provisioner waits for DNS and retries
`apt`. SSH provisioning and recovery still work with `network = "off"`.

---

## One-time setup

The network needs root rights once:

```bash
sudo hegn network setup --user "$USER"   # 16 data + 16 control TAPs, nftables, marker
hegn network status                      # marker, taps and reservations
hegn network setup --dry-run --user "$USER"  # inspect the exact script first
sudo hegn network reload                 # rules only; TAPs stay up
sudo hegn network teardown               # remove both TAP families and the rules
```

The setup creates both TAP families: `hegn0`–`hegn15` for guarded data and
`hegnc0`–`hegnc15` for control. It writes `/run/hegn/network.json` with **marker
version 3**. Re-run the setup after upgrading from a version with an older
marker; an old receipt cannot vouch for the control TAPs or the new nftables
rules.

`network = "off"` needs this setup too, because every VM still gets a control
TAP for SSH. A host without the marker cannot start any VM.

`hegn doctor --fix` runs the setup for you and asks for `sudo` or `pkexec`. It
prints the exact command before the password prompt, so you can read it first.

`hegn vm start` is fail-closed. It refuses to boot unless
`/run/hegn/network.json` exists, was written by root and carries the current
marker version.

The marker file is only a receipt. Setup writes it only when the live nftables
table matches the expected rules. `hegn doctor`, run as root, reads the table
directly and does not trust the JSON file. A flushed table therefore fails
safely.

---

## nftables rules

The privileged setup installs the `inet hegn` table. In addition to the data
allow-list it creates control sets (`hegn_control_ifaces`, `hegn_control_hosts`
and an optional `hegn_control_guests` set) and applies these rules:

Input chain, in order:

1. For each control TAP, a source check:
   `iifname "hegnc<i>" ip saddr != <guest>/32 drop`.
2. `iifname @hegn_control_ifaces ip daddr @hegn_control_hosts ct state
   established,related accept`.
3. `iifname @hegn_control_ifaces counter drop`.
4. The existing data rules, unchanged.

Forward chain:

- an early `iifname @hegn_control_ifaces counter drop`,
- no masquerade for `10.214.0.0/24`; only the guarded data network is NATed.

The established-accept rule must come before the control drop, otherwise SSH
replies from the guest would be discarded. `hegn network status` and a root-run
`hegn doctor` parse the live table and refuse to trust the marker when the
control sets, source drops, established-accept, forward drop or the absence of
control masquerade do not match.

---

## Host firewalls

A host firewall with its own routing rules (ufw, firewalld or plain iptables)
could silently drop the guarded data traffic, even though hegn allows it. So
setup also exempts the **data** TAP pool from that firewall:

- ufw `route` rules,
- firewalld direct rules,
- `FORWARD` accepts for plain iptables.

This exemption is a blanket accept for the data TAP interfaces in the host
firewall only. The HTTP(S) and DNS allow-list stays in hegn's own table, so what
the guest can reach does not change. Control TAPs get **no** host-firewall
forward exemption: their forward path is dropped, and their input path is
accepted by hegn's established/related rule.

Run `sudo hegn network setup` again after you install, enable or reset a host
firewall, and after upgrading from a hegn version whose marker is older.
Marker version 3 is required for both the control and the data rules.

---

## Local LLM endpoints

Tools like Ollama, LM Studio, llama.cpp and vLLM usually listen on the host's
loopback address (`127.0.0.1`). Inside the guest, `127.0.0.1` means the guest
itself. So hegn can expose selected host ports on the TAP address of each VM:

```toml
[[local_endpoints]]
name = "ollama"   # guest env: HEGN_ENDPOINT_OLLAMA (plus OLLAMA_HOST)
port = 11434      # host target: 127.0.0.1:11434
```

`hegn network setup` adds one DNAT rule per **data** TAP and one narrow input
rule. The guest then uses `http://10.213.0.<n>:11434`.

- Host loopback is never bridged, and other host interfaces stay unreachable.
- Ports below 1024 are rejected.
- Endpoints require `network = "guarded"`; control TAPs never get endpoint DNAT.
- Control TAPs keep their no-egress/no-forwarding rules.

After you change the list, run `sudo hegn network reload`. This replaces only the
nftables rules. The TAP pool and running guests are not touched. A full
`sudo hegn network setup` also works. Until the root-owned marker matches,
`hegn vm start` fails with a hint.

A full setup recreates any TAP whose owner does not match `--user`. The guest's
link on that TAP is interrupted. This prevents a previous local user from keeping
attach rights.

With `cloud_init = true`, the URLs are exported in the guest through
`/etc/profile.d/hegn-endpoints.sh`. Each endpoint is also available as
`HEGN_ENDPOINT_<NAME>`. `hegn vm start`, `restart` and `apply` print the guest
endpoint URLs in their start report.

---

## Troubleshooting

### The guest has no network

Error messages such as `Temporary failure in name resolution` usually mean that a
host firewall blocks the guest's traffic. This happens when the firewall was
installed or reset *after* `hegn network setup`. Its own `forward` rules then
stay in place without hegn's data exemption. Netfilter runs every `forward`
chain, so the host firewall drops traffic that hegn had already allowed.

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

`network = "off"` has no data network by design; use SSH for management.

### SSH cannot connect

`hegn attach` uses the control TAP. Check these in order:

1. `hegn doctor` reports the `ssh`/`ssh-keygen` tools, marker version 3, the
   control TAPs and the control nftables rules. Fix what it names.
2. `hegn network status` shows the control TAPs and the setup marker. If the
   marker is older than version 3, run `sudo hegn network setup --user "$USER"`.
3. `Connection refused` usually means the VM was booted before SSH support or
   sshd is not ready. Wait for the boot/provisioning to finish, or run
   `hegn vm apply <vm>` and start the VM again. The VM keeps running.
4. `Permission denied (publickey)` means the seed does not authorize the
   current key for `ssh_user`. `hegn vm apply <vm>` regenerates the seed and
   rotates the guest host keys; `hegn vm rebuild <vm>` also creates a new
   client key.
5. `Host key verification failed` means the guest host key changed. The per-VM
   `known_hosts` is not overwritten automatically. `hegn vm apply` clears it and
   rotates the host keys on the next boot.
6. `guest_ip`/`ssh_user` missing in `session.json` means the VM predates this
   release. Use `hegn attach <vm> --serial` until `hegn vm apply <vm>` has
   refreshed the record.
7. A host firewall should not block established SSH replies; hegn accepts them
   in its own control input rule. If SSH still fails while the data network
   works, check `sudo nft list table inet hegn` for the control sets and drops.

There is no automatic serial fallback. `hegn attach <vm> --serial` is always the
explicit recovery path.
