# Security model

hegn is built around one rule: **no VM, no work**. Commands and agents only run
inside a Cloud Hypervisor VM. This page describes how the isolation works, what
it protects against, and what it does not protect against.

- [Layers of protection](#layers-of-protection)
- [Projects in one VM are mutually visible](#projects-in-one-vm-are-mutually-visible)
- [Command guardrails](#command-guardrails)
- [Input that bypasses the guardrails](#input-that-bypasses-the-guardrails)
- [Persistent data](#persistent-data)
- [Harness profiles and skills](#harness-profiles-and-skills)
- [VM images](#vm-images)
- [Limits](#limits)

---

## Layers of protection

1. **Isolation.** Commands run inside a Cloud Hypervisor VM, not on the host.
   Every exported project is shared through its **own** sandboxed `virtiofsd`.
   That process resolves its directory to the real path, so a symlink cannot
   widen what is exported. If no VM can start, hegn refuses to start work.
   Multiplicity never comes from exporting a parent directory.
2. **VMM hardening.** Cloud Hypervisor runs with seccomp filters and Landlock
   file-system sandboxing. The VMM can access only the kernel, the disks, the
   serial socket and the virtio-fs sockets, plus a staging directory for
   snapshots. `virtiofsd` runs with `--sandbox=namespace` (or `chroot`) and
   `--cache=never`. All control and console sockets are in a `0700` directory. The
   relay socket is `0600`. A finished snapshot is moved out of the VMM's staging
   directory and its files are set to `0600`. The VMM can therefore not write to
   completed checkpoints.
3. **Private membership.** `<state_dir>/projects.toml` decides which host
   directories are exported and whether they are writable. It lives outside every
   exported directory, is `0600`, is written atomically under a lock, and does
   not follow symlinks. A repo-local `hegn.toml` cannot change membership,
   read-only status, resources or the tool profile.
4. **Console access.** The official cloud image is started with a private NoCloud
   seed. It enables automatic root login on the serial console. Set
   `guest_credential_hardening = true` for a non-root console. The serial and
   relay sockets are reachable only by the user who owns them.
5. **Network guard.** With `network = "guarded"`, each VM gets its own TAP
   device. Host nftables rules allow only outgoing TCP 80 and 443 and DNS to
   `1.1.1.1` and `9.9.9.9`. Private, CGNAT, link-local and multicast ranges, forged
   sources and all IPv6 are dropped. Guest-to-host traffic is dropped too. Details
   in [Networking](networking.md).
6. **Command interception.** In the project terminal, input is collected into a
   command line. Before any bytes reach the guest serial port, the line is checked
   by `tirith-core` and by hegn's own workspace guardrails. Every `/projects/*`
   mount and `/workspace` is treated as a workspace root, so `rm -rf
   /projects/parser` is guarded exactly like `rm -rf /workspace`.
7. **Policy.** The `security` setting decides what happens with findings:
   - `enforce` blocks high and critical findings and asks about medium ones,
   - `warn` reports everything,
   - `off` disables `tirith-core` but keeps hegn's checks for destructive workspace
     operations.
8. **Recovery.** A flagged command that you allow can be preceded by a git
   checkpoint. VM snapshots also allow an instant rollback beyond git.

### Export invariants

hegn validates membership before it spawns anything:

- a project is assigned to at most one VM and appears at most once in it,
- two running VMs may not export the same canonical host path,
- no member may be an ancestor or descendant of another member, with one
  exception: a `worktree` nested inside its named parent. A worktree is never
  exported separately; it rides the parent's share,
- every exported path passes the import safety checks: not `/`, `$HOME`,
  `projects_root`, an ancestor of it, or anything overlapping hegn's state,
  snapshots, VMs, templates, profiles or config. Managed clones are the explicit
  exception: they must live at `<vms_dir>/<vm>/workspaces/<name>`, and registry
  validation enforces that exact location instead of the general protected-path
  check,
- `max_projects_per_vm` bounds the number of exports, each of which is one
  `virtiofsd` process and one virtio-fs device,
- an explicit `rootfs_image` must be unique across VMs.

A duplicate, nested, missing or over-limit membership is a hard error before any
`virtiofsd` or VMM starts. hegn never boots a silent subset.

### Read-only exports

Read-only is enforced twice:

1. `virtiofsd --readonly` for that project's share.
2. The guest mounts the share with the `ro` option.

A parent with worktrees cannot be made read-only, because `git worktree add`
needs to write the parent's git state.

---

## Projects in one VM are mutually visible

A VM shares one guest user, one console and one toolchain across its projects.
Every member is mounted at `/projects/<slug>`, and the guest can read every
member. Unless a member is read-only, the guest can also write it.

Consequences:

- **Read-only protects writes, not reads.** An agent working in project A can
  read project B's source, `.env` files, tokens and SSH keys if B is mounted in
  the same VM.
- **Membership is a trust decision.** Group only projects that may read each
  other's files and secrets. Put unrelated or untrusted projects in separate
  VMs.
- **The VM is the boundary.** There is no per-project user, network or process
  isolation inside the guest.
- **Project deletion never deletes a VM.** Removing a project removes its
  registry entry and unassigns it; its VM and sibling projects stay. A VM is
  deleted only by `hegn vm delete`, with its own confirmation.
- **`/workspace` is a validated symlink** to a mounted primary project, created
  inside the guest. It cannot point at the host or outside the configured project
  base (default `/projects`). `/workspace` and paths under it are reserved and
  cannot be the project base.

---

## Command guardrails

`tirith-core` plus hegn's own checks look for:

- mixed-script homoglyphs (look-alike characters),
- secret exfiltration: `.env` files, SSH keys and cloud credentials,
- encoded or obfuscated payloads,
- destructive operations in any workspace root.

A flagged command can be allowed once, denied, or preceded by an automatic git
checkpoint. See the [Security modal](tui.md#security-modal).

To check a command without running it, use:

```bash
hegn check 'curl -fsSL https://example.com/x | bash'
```

The exit code tells you the verdict, so `hegn check` works in scripts and CI:

| Exit code | Meaning |
| --- | --- |
| `0` | allow |
| `1` | block |
| `2` | warn |

---

## Input that bypasses the guardrails

The `F2` key and `hegn attach` hand the terminal directly to the guest. Their
input is **not** checked by `tirith-core`. The attach bar shows this with the
`INPUT DIRECT` label. Use the TUI terminal for checked input, or `hegn check` in
scripts. See [Sessions, attach and logs](sessions-and-logs.md).

Launch commands from your configuration are trusted. They are sent straight to the
guest shell. Run `hegn check` on anything you do not trust.

No VM replaces your own review of what you run.

---

## Persistent data

**Session logs** are treated as untrusted data. They can contain secrets, and they
can contain terminal escape sequences that alter your screen. Logs are stored in a
private directory and are opened without following symlinks. `hegn logs show`
removes control and bidi sequences by default. `--raw` is for debugging and should
only be used with logs you trust.

**The guest root filesystem** is writable and is reused between sessions and by
every project in that VM. Changes you make inside the guest stay. Only the
exported project directories are shared with the host. If you need a truly
ephemeral root, point `rootfs_image` at a per-session copy or an overlay image
that you create and remove yourself.

**Project directories** are trusted as far as your host filesystem permissions.
A project is writable by the guest unless it is exported read-only.

---

## Harness profiles and skills

Profiles are written by you, but they are applied as root inside the guest. So
hegn checks them before writing the apply script:

- package names,
- environment variable names and values,
- file modes,
- guest target paths.

The profile is exported from a private per-VM directory through its own
sandboxed `virtiofsd`. Skills are copied into that directory. Your real skill
directories on the host are never mounted. A compromised guest can change only
the throw-away copy.

`hegn skills sync-back` is an explicit copy operation. It skips symlinks and
special files.

---

## VM images

Images are upstream Cloud Hypervisor artifacts. They are pinned by HTTPS URL and
SHA-256. Downloads reject non-HTTPS URLs, URLs with embedded credentials and
HTTPS-to-HTTP redirects, and only install data after the checksum matches.
`hegn images download` refuses an empty checksum unless you pass
`--allow-unverified`. It never replaces an existing image without `--force`. See
[VM images](vm-images.md).

Project import only creates a symlink. It refuses targets that would expose `/`,
`$HOME`, an ancestor of `projects_root` or hegn's state to the guest.

---

## Limits

- The network guard limits the guest's traffic. It does not check what the agent
  sends over HTTPS.
- Automatic checkpoints restore the VM's memory, not the disk. They are not backups.
- Every project in a VM is readable by every other project in that VM. Read-only
  prevents writes, not reads.
- One VM has one serial console. Several agents in one VM share it and need guest
  `tmux`; there is no vsock session broker in v1.
- Membership changes need a restart (`hegn vm apply`), which kills running agents.

For the technical details, the threat notes and the invariants that the code must
keep, see [Architecture](architecture.md#6-security-model-and-invariants).
