# Migrating to v2

hegn v2 is a hard cut. There is no automatic migration from the v1
one-project-per-VM model.

- [Upgrading an existing v2 installation](#upgrading-an-existing-v2-installation)
- [What v2 reads](#what-v2-reads)
- [What happens to v1 state](#what-happens-to-v1-state)
- [Recreate a v1 instance](#recreate-a-v1-instance)
- [Command mapping](#command-mapping)
- [Limitations carried into v2](#limitations-carried-into-v2)

---

## Upgrading an existing v2 installation

The SSH/control-TAP release keeps the v2 config and state format, but it changes
the network setup, the guest seed and the snapshot fingerprint. Upgrade an
existing v2 host in this order:

1. **Host tools.** Install the OpenSSH client if it is missing. `ssh` and
   `ssh-keygen` must be on `PATH`; `hegn doctor` now checks both.
2. **Network setup.** Run `sudo hegn network setup --user "$USER"` again. The
   marker moves to version 3 and the 16 control TAPs (`hegnc0`–`hegnc15`) plus
   the control nftables rules are created. This is required even for
   `network = "off"`, because every VM now gets a control TAP for SSH.
3. **Seed and guest.** `hegn vm apply <vm>` regenerates the seed with version 13
   and restarts the VM. The next boot re-runs cloud-init once, configures the
   control netplan and sshd, generates fresh guest host keys and authorizes the
   per-VM public key. For a stopped VM, `hegn vm start <vm>` does the same on
   its next boot; a reboot inside the guest does not rewrite the seed.
4. **Snapshots.** Snapshots from before seed version 13 or the
   `hegn-snapshot-boot-v3` fingerprint domain are not resumed. The next start
   cold boots and logs why. Manual restore refuses them; take a fresh snapshot
   after `hegn vm apply` if you need a restorable checkpoint.
5. **Old session records.** A VM whose `session.json` predates `guest_ip` and
   `ssh_user` refuses the default SSH attach. Use
   `hegn attach <vm> --serial` until `hegn vm apply <vm>` has refreshed the
   record, or run `hegn vm apply` first and attach over SSH afterwards.

`hegn doctor` reports an outdated network marker, missing control TAPs, missing
SSH tools and running VMs without `guest_ip`/`ssh_user`/key. There is no
automatic serial fallback; `--serial` is the explicit recovery path.

---

## What v2 reads

| Item | v2 behaviour |
| --- | --- |
| `vms_dir` (default `~/.local/share/hegn/vms`) | Active VM directory |
| `<state_dir>/projects.toml` | Active private project registry (version 2) |
| `<vms_dir>/<vm>/vm.toml` | Active per-VM config (version 2) |
| Project `hegn.toml` | Only `launch`, `integration`, `auto_start` |
| `instances_dir` in `config.toml` | Deprecated alias for `vms_dir`; warning |
| `sandboxes_dir` in `config.toml` | Deprecated alias for `vms_dir`; warning |
| `shared_tag` in `config.toml` | Deprecated alias for `share_tag_prefix`; warning |
| `guest_mount` in `config.toml` | Deprecated alias; `/workspace` migrates to `/projects` with a specific warning |
| v1 instance directories | Ignored, reported by `hegn doctor`, never deleted |
| v1 snapshots without a fingerprint | Listed as legacy; never restored or deleted |
| v1 session logs | Still readable with `hegn logs` |

A v1 `config.toml` keeps working for the renamed global keys, so startup does not
fail. The old values are read; rewrite them to the new names when convenient.
Unknown keys are still rejected.

The old v1 default `guest_mount = "/workspace"` is migrated to
`/projects` with this exact warning:

```text
warning: legacy guest_mount = "/workspace" is reserved for the primary /workspace symlink; using guest_projects_base = "/projects" instead. Set guest_projects_base explicitly to silence this warning.
```

Set `guest_projects_base` explicitly to remove the warning. A new
`guest_projects_base = "/workspace"` is rejected because `/workspace` and paths
under it are reserved for the primary-project symlink.

Per-VM `security`, `periodic_snapshots`, `snapshot_interval_secs`,
`snapshot_keep` and `auto_resume` in `vm.toml` are effective and are seeded from
the global values when the VM is created. Change them in `vm.toml`; use
`hegn vm apply <vm>` to restart a running VM with the new settings.

A v1 project `hegn.toml` may contain `harness`, `packages` or `template`. Those
keys are ignored for control in v2. Harness and packages belong to `vm.toml`.

---

## What happens to v1 state

- Running v1 VMs are not adopted. Stop them before upgrading.
- v1 instance directories under `~/.local/share/hegn/instances` are not scanned,
  started, restored or deleted. They are protected paths, so they cannot be
  imported as projects either.
- v1 snapshots remain in `snapshots_dir`, visible in `hegn snapshot list` with
  format `legacy`. `hegn snapshot restore` refuses them with a readable error.
  They are never auto-selected for resume and never pruned automatically.
- `hegn doctor` reports legacy instance directories, fingerprint-less snapshots
  and legacy repo keys, with a one-line remediation for each. Nothing is
  removed.
- Logs are diagnostics, not state. They stay readable.

---

## Recreate a v1 instance

Create a VM with the old tool profile, then assign the project:

```bash
hegn vm create my-app --harness opencode --vcpus 4 --memory 8192
hegn project import ~/code/my-app --vm my-app
hegn vm start my-app
```

To keep several v1 instances of the same repository, either assign them as
separate projects to one VM or fork the project:

```bash
hegn project fork my-app --name my-app-review
hegn project fork my-app --mode worktree --name my-app-experiment
```

Clones are independent and are the default. Worktrees are opt-in and share git
state with the parent.

Guest tools are not migrated. A fresh VM rootfs installs the harness on first
boot. Anything you installed manually in the old guest must be installed again,
or baked into a custom `rootfs_image`.

---

## Command mapping

| v1 | v2 |
| --- | --- |
| `hegn instance list` | `hegn vm list` |
| `hegn instance status <name>` | `hegn vm show <name>` |
| `hegn instance create <project>` | `hegn vm create <vm>` + `hegn project assign <project> --vm <vm>` |
| `hegn instance start <name>` | `hegn vm start <vm>` (or `hegn open <project>`) |
| `hegn instance stop <name>` | `hegn vm stop <vm>` |
| `hegn instance pause` / `resume` | no pause command; `hegn vm restart <vm>` |
| `hegn instance config <name>` | `hegn vm config <vm>` |
| `hegn instance delete <name>` | `hegn vm delete <vm>` |
| `hegn instance clone <project>` | `hegn project fork <project> --name <name>` |

`hegn instance` remains as a hidden command that prints this mapping and exits
non-zero, so stale scripts fail with a readable message instead of an
unknown-command error.

`hegn project create` no longer accepts `--harness` or `--package`; those are VM
settings on `hegn vm create`. `hegn project create/import` now takes `--vm` or
`--solo`. Without either, an interactive run asks whether to create a one-member
VM named after the project.

---

## Limitations carried into v2

- **SSH sessions, no vsock broker.** Several SSH sessions per VM run in
  parallel over the control TAP, each with its own guest PTY. The embedded TUI
  terminal remains the serial console; there are no TUI tabs, VT emulator or
  hegn-side session multiplexer. Guest `tmux` is optional persistence, not the
  transport.
- **Membership changes require a restart.** There is no live virtio-fs hot-plug;
  `hegn vm apply` restarts the VM and kills running agents.
- **Tool changes are not reconciled.** The provisioner installs the baseline and
  runs again when the seed identity changes (for example after `hegn vm apply`).
  Later changes are manual in the guest or `hegn vm rebuild <vm>` plus a fresh
  first boot.
- **No per-project snapshots.** A snapshot belongs to the VM and covers every
  member.
- **No isolation between projects in one VM.** Every member is readable at
  `/projects/<slug>`. Group only projects that may read each other's files and
  secrets; use separate VMs for a hard boundary.
