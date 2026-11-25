# Troubleshooting

Start with `hegn doctor`. It checks the host and tells you what is missing. Most
problems are solved by `hegn doctor --fix`.

- [A VM does not start](#a-vm-does-not-start)
- [A project is not mounted](#a-project-is-not-mounted)
- [Landlock is not active](#landlock-is-not-active)
- [The guest has no network](#the-guest-has-no-network)
- [Legacy v1 checks](#legacy-v1-checks)
- [Where to find logs](#where-to-find-logs)

---

## A VM does not start

1. Run `hegn doctor` and fix what it reports.
2. Read the start log:

   ```bash
   cat ~/.local/share/hegn/vms/<vm>/run/hegn.log
   ```

3. Check the VMM log and the relay log. See
   [Start diagnostics](sessions-and-logs.md#start-diagnostics).

The TUI doctor opens automatically after a start failure. That way the other host
checks and the image state are visible too.

Common membership errors:

| Message | Fix |
| --- | --- |
| `VM '<vm>' has no projects` | `hegn project assign <project> --vm <vm>` |
| `member directory ... missing` | Restore the directory or `hegn project unassign <project>` |
| `duplicate project path` / `nested inside` | Assign only one of the overlapping projects |
| `already exported by running VM` | Stop the other VM first |
| `above max_projects_per_vm` | Unassign projects or raise the limit (hard ceiling 32) |
| `primary project ... is not an exported member` | `hegn vm primary <vm> <project>` |

If you changed membership while the VM was running, run `hegn vm apply <vm>`.
That restarts the VM and kills running agents.

---

## A project is not mounted

A VM boots with the membership recorded at start. Assigning or unassigning a
project while it runs only changes the registry; attach prints a warning.

```bash
hegn vm projects <vm>
hegn vm apply <vm>
```

Inside the guest, check `/projects` and `/workspace`. A missing mount count in
the latest `hegn-health` line (`mounts=<ok>/<total>`) means a share failed to
mount; the boot log names the member.

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

## Legacy v1 checks

`hegn doctor` has four optional legacy findings:

| Check | What it reports |
| --- | --- |
| `legacy instance dirs` | v1 instance directories that v2 ignores |
| `legacy snapshots` | snapshots without a boot fingerprint; not restorable |
| `legacy project keys` | `harness`, `packages` or `template` in a repo `hegn.toml` |
| `deprecated config keys` | `instances_dir`, `shared_tag`, `guest_mount` and old aliases |

These checks are marked `!` when they find something. They never make
`hegn doctor` fail and never delete anything. Read the remediation line, verify
the old state is unused, then remove it or rewrite the keys yourself. `hegn vm
list` shows only active v2 VMs; `hegn snapshot list` marks legacy entries as
`legacy`.

---

## Where to find logs

| What | Command or path |
| --- | --- |
| Lifecycle and start errors | `run/hegn.log` in the VM directory |
| Cloud Hypervisor (VMM) log | `run/cloud-hypervisor.log` |
| Console relay log | `run/console-relay.log` |
| Automatic checkpoint errors | `run/autosave.log` |
| Serial output of all sessions | `hegn logs list`, then `hegn logs show <name>` |
| Serial output for one VM | `hegn logs list --vm <vm>` |
| Guest provisioning | the serial console and the journal. Look for `hegn: preset ready`. |

The VM directory defaults to `~/.local/share/hegn/vms/<vm>/`. See
[Sessions, attach and logs](sessions-and-logs.md#session-logs).

Legacy v1 instance directories under `~/.local/share/hegn/instances` are not
used by v2. `hegn doctor` reports them. Their files are never deleted
automatically; remove them yourself after you no longer need the diagnostics.
