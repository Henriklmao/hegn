# Troubleshooting

Start with `hegn doctor`. It checks the host and tells you what is missing. Most
problems are solved by `hegn doctor --fix`.

- [A VM does not start](#a-vm-does-not-start)
- [Landlock is not active](#landlock-is-not-active)
- [The guest has no network](#the-guest-has-no-network)
- [Where to find logs](#where-to-find-logs)

---

## A VM does not start

1. Run `hegn doctor` and fix what it reports.
2. Read the start log:

   ```bash
   cat ~/.local/share/hegn/instances/<name>/run/hegn.log
   ```

3. Check the VMM log and the relay log. See
   [Start diagnostics](sessions-and-logs.md#start-diagnostics).

The TUI doctor opens automatically after a start failure. That way the other host
checks and the image state are visible too.

---

## Landlock is not active

hegn always enables Cloud Hypervisor's Landlock sandbox. If the host kernel was
built without Landlock, or securityfs is not available, the VMM refuses to start.

Run `hegn doctor`. It reports the `landlock` check. Install a kernel with Landlock
enabled. There is no configuration switch to run without it.

---

## The guest has no network

Messages such as `Temporary failure in name resolution` usually come from a host
firewall that was set up after hegn's network. Run the network setup again:

```bash
sudo hegn network setup --user "$USER"
```

The full explanation is in [Networking](networking.md#troubleshooting-the-guest-has-no-network).

---

## Where to find logs

| What | Command or path |
| --- | --- |
| Lifecycle and start errors | `run/hegn.log` in the instance directory |
| Cloud Hypervisor (VMM) log | `run/cloud-hypervisor.log` |
| Console relay log | `run/console-relay.log` |
| Serial output of all sessions | `hegn logs list`, then `hegn logs show <name>` |
| Guest provisioning | the serial console and the journal. Look for `hegn: preset ready`. |

The instance directory defaults to `~/.local/share/hegn/instances/<name>/`. See
[Sessions, attach and logs](sessions-and-logs.md#session-logs).
