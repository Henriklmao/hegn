# Security model

hegn is built around one rule: **no VM, no work**. Commands and agents only run
inside a Cloud Hypervisor VM. This page describes how the isolation works, what
it protects against, and what it does not protect against.

- [Layers of protection](#layers-of-protection)
- [Command guardrails](#command-guardrails)
- [Input that bypasses the guardrails](#input-that-bypasses-the-guardrails)
- [Persistent data](#persistent-data)
- [Harness profiles and skills](#harness-profiles-and-skills)
- [VM images](#vm-images)
- [Limits](#limits)

---

## Layers of protection

1. **Isolation.** Commands run inside a Cloud Hypervisor VM, not on the host. The
   project directory is shared through a per-project `virtiofsd`. That process is
   sandboxed and resolves the shared directory to its real path, so a symlink
   cannot widen what is exported. If no VM can start, hegn refuses to start work.
2. **VMM hardening.** Cloud Hypervisor runs with seccomp filters and Landlock
   file-system sandboxing. The VMM can access only the kernel, the disks, the
   serial socket and the virtio-fs sockets, plus a staging directory for
   snapshots. `virtiofsd` runs with `--sandbox=namespace` (or `chroot`) and
   `--cache=never`. All control and console sockets are in a `0700` directory. The
   relay socket is `0600`. A finished snapshot is moved out of the VMM's staging
   directory and its files are set to `0600`. The VMM can therefore not write to
   completed checkpoints.
3. **Console access.** The official cloud image is started with a private NoCloud
   seed. It enables automatic root login on the serial console. Set
   `guest_credential_hardening = true` for a non-root console. The serial and
   relay sockets are reachable only by the user who owns them.
4. **Network guard.** With `network = "guarded"`, each VM gets its own TAP
   device. Host nftables rules allow only outgoing TCP 80 and 443 and DNS to
   `1.1.1.1` and `9.9.9.9`. Private, CGNAT, link-local and multicast ranges, forged
   sources and all IPv6 are dropped. Guest-to-host traffic is dropped too. Details
   in [Networking](networking.md).
5. **Command interception.** In the project terminal, input is collected into a
   command line. Before any bytes reach the guest serial port, the line is checked
   by `tirith-core` and by hegn's own workspace guardrails.
6. **Policy.** The `security` setting decides what happens with findings:
   - `enforce` blocks high and critical findings and asks about medium ones,
   - `warn` reports everything,
   - `off` disables `tirith-core` but keeps hegn's checks for destructive workspace
     operations.
7. **Recovery.** A flagged command that you allow can be preceded by a git
   checkpoint. VM snapshots also allow an instant rollback beyond git.

---

## Command guardrails

`tirith-core` plus hegn's own checks look for:

- mixed-script homoglyphs (look-alike characters),
- secret exfiltration: `.env` files, SSH keys and cloud credentials,
- encoded or obfuscated payloads,
- destructive operations in the workspace.

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

**The guest root filesystem** is writable and is reused between sessions. Changes
you make inside the guest stay. The workspace is the only project directory that is
shared. If you need a truly ephemeral root, point `rootfs_image` at a per-session
copy or an overlay image that you create and remove yourself.

---

## Harness profiles and skills

Profiles are written by you, but they are applied as root inside the guest. So
hegn checks them before writing the apply script:

- package names,
- environment variable names and values,
- file modes,
- guest target paths.

The profile is exported from a private per-instance directory through its own
sandboxed `virtiofsd`. Skills are copied into that directory. Your real skill
directories on the host are never mounted. A compromised guest can change only
the throw-away copy.

`hegn skills sync-back` is an explicit copy operation. It skips symlinks and
special files.

---

## VM images

Images are upstream Cloud Hypervisor artifacts. They are pinned by HTTPS URL and
SHA-256. `hegn images download` refuses an empty checksum unless you pass
`--allow-unverified`. It checks the data before the atomic rename. It never
replaces an existing image without `--force`. See [VM images](vm-images.md).

Project import only creates a symlink. It refuses targets that would expose `/`,
`$HOME`, an ancestor of `projects_root` or hegn's state to the guest.

---

## Limits

- The network guard limits the guest's traffic. It does not check what the agent
  sends over HTTPS.
- Automatic checkpoints restore the VM's memory, not the disk. They are not backups.
- The workspace is writable by the agent unless you enable
  [read-only mode](projects-and-instances.md#read-only-workspace).

For the technical details, the threat notes and the invariants that the code must
keep, see [Architecture](architecture.md#6-security-model-and-invariants).
