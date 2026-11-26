# Multi-project VMs: project-first design

**Status:** accepted, implemented (v2)
**Supersedes:** the one-project-per-VM model described in
[Projects, VMs and workspaces](../projects-and-vms.md) and
[Architecture §2–§6](../architecture.md).
**ADRs:** [ADR-006 … ADR-011](../architecture.md#13-decisions-and-trade-offs).

---

## 1. Summary

Today a project *is* a VM: the v1 instance config stores one `project`, one
`workspace`, one `shared_tag`, and one `guest_mount`; boot spawns one
`virtiofsd` and one `--fs` device. Tools (Node, Rust, Python, the harness CLI)
are installed into that VM's rootfs, so ten projects mean ten rootfs copies and
ten first-boot installations.

The new model separates three things that are currently fused:

| Concept | Role | Cardinality |
| --- | --- | --- |
| **Project** | A directory the user works in; the primary navigation object | many |
| **VM** | A named tool profile and runtime: harness, packages, CPU/RAM, rootfs, network, snapshots, SSH access | many |
| **Workspace** | A concrete checkout exported to a VM: the project itself, a clone, or a git worktree | one per project membership |

A **project belongs to at most one VM**. A **VM hosts many projects**. The
guest gets one virtio-fs share per project, so tools and package installs are
paid for once per VM while every project stays a separate host-side export.

That last point has a deliberate consequence: **projects in one VM can see each
other**. The VM is now the trust boundary, not the project. Only group projects
that may read each other's files and secrets; a read-only project prevents
writes, not reads. This is the price of shared tools and is called out again in
§9 and §13.

Example:

```bash
hegn vm create rust --harness opencode --vcpus 8 --memory 16384 --package rustup
hegn project assign ~/code/parser --vm rust
hegn project assign ~/code/cli    --vm rust
hegn open ~/code/parser            # boots the rust VM with both projects mounted
```

Inside the guest:

```text
/projects/parser   -> host ~/code/parser
/projects/cli      -> host ~/code/cli
/workspace         -> symlink to /projects/parser   (the VM's primary project)
```

`rustup`, the harness CLI and any manual `apt install` live in one rootfs and
are shared by both projects.

---

## 2. Goals and non-goals

### Goals

1. Share one toolchain, harness install and rootfs across many projects.
2. Keep the security property that **each exported directory is its own
   sandboxed `virtiofsd`**. Never export a parent directory to get multiplicity.
3. Keep the project picker as the home screen. The user navigates projects; the
   VM is an implementation detail they can name and tune.
4. Make clones cheap: a clone is another workspace member of the same VM, not a
   second VM with a second rootfs.
5. Keep every TUI action available as a non-interactive CLI command.
6. Fail closed. Missing project, duplicate membership, or a mismatched snapshot
   is an error, never a partial boot.
7. Be explicit that VM membership is a **trust decision**. Projects in one VM
   share a guest user, a toolchain, an SSH account and each other's mounted
   files.
8. Give every interactive session its own guest PTY and resize. SSH over a
   control TAP is the transport; the serial relay remains boot/recovery. No
   vsock broker (ADR-011).

### Non-goals (v1 of this model)

- TUI tabs, an embedded VT emulator or a hegn-side session multiplexer. SSH
  sessions are external; the embedded TUI terminal stays on the serial relay.
- Live adding/removing of shares on a running VM. Membership changes require a
  restart (`hegn vm apply`).
- Automatic migration of v1 instances. See §11.
- Host-side package management inside the guest beyond the existing first-boot
  provisioner.

---

## 3. Decisions at a glance

| # | Decision | ADR |
| --- | --- | --- |
| 1 | Multiple projects per VM, one sandboxed `virtiofsd` per project | [ADR-006](../architecture.md#adr-006-multiple-projects-per-vm) |
| 2 | Project-first control plane; VM is a named tool profile; membership lives in private state | [ADR-007](../architecture.md#adr-007-project-first-membership-in-private-state) |
| 3 | Parallel workspaces: self-contained clones by default, nested worktrees opt-in | [ADR-008](../architecture.md#adr-008-clones-by-default-worktrees-opt-in) |
| 4 | Hard cut to config/state v2; v1 instances are not migrated | [ADR-009](../architecture.md#adr-009-hard-cut-to-v2) |
| 5 | Snapshot boot fingerprint; membership or resource changes invalidate resume | [ADR-010](../architecture.md#adr-010-snapshot-boot-fingerprint) |
| 6 | SSH over a per-VM control TAP; serial stays the recovery channel; no vsock broker | [ADR-011](../architecture.md#adr-011-ssh-over-the-control-tap-instead-of-a-vsock-broker) |

---

## 4. Domain model

### 4.1 Project (primary)

A project is a directory under `projects_root` (default `~/Projects/hegn`),
including import symlinks. It has a stable picker `name` and a canonical
`path`.

Control data lives in a **private registry** outside every exported directory:

```text
<state_dir>/projects.toml        # 0600, atomic tmp+rename, flock for writers
```

```toml
version = 2

[[project]]
name = "parser"
path = "/home/me/code/parser"        # canonicalized
vm = "rust"                          # absent = unassigned
read_only = false
kind = "external"                    # external | clone | worktree
managed = false                      # true only for hegn-created clone dirs
hidden = false
created_at = "2026-11-24T10:00:00Z"

[[project]]
name = "parser-fix-tests"
path = "/home/me/.local/share/hegn/vms/rust/workspaces/parser-fix-tests"
vm = "rust"
kind = "clone"
parent = "parser"
managed = true
```

Why private state: the registry decides **which host directories are exported
and whether they are writable**. A file inside the project is writable by the
agent through `virtiofsd`; it must never be able to widen its own export or
mark itself read-only/read-write. This is the same reason nftables rules and
TAP reservations are host-owned.

A repo-local `hegn.toml` may still exist for **presentation-only** preferences
(`launch`, `integration`, `auto_start`). It is validated exactly as today and
cannot change membership, read-only status, resources or the tool profile.
v1 project files that contain `harness`, `packages` or `template` are ignored
with a warning.

### 4.2 VM (tool profile + runtime)

```text
<vms_dir>/<name>/vm.toml         # 0600
```

```toml
version = 2
name = "rust"
harness = "opencode"             # shared tool profile
vcpus = 8
memory_mib = 16384
extra_packages = ["rustup"]
rootfs_image = "/abs/optional.qcow2"   # None = private copy of the base image
primary = "parser"               # picker name; drives /workspace
security = "enforce"
periodic_snapshots = true
snapshot_interval_secs = 300
snapshot_keep = 2
auto_resume = true
network_index = 3                # TAP slot; set in every mode, kept across restores
ssh_user = "root"                # root | hegn | ubuntu; default from hardening
created_at = "2026-11-24T10:00:00Z"
```

The VM config deliberately does **not** list its projects. Membership is
derived from the project registry, which keeps the project as the primary
object and avoids a second source of truth. Boot resolves the VM name to
members by scanning the registry.

`primary` is the only project-specific VM setting. It must name a current
member; changing it requires a restart because `/workspace` is a boot-time
symlink. When the first project is assigned to an empty VM it becomes primary
automatically.

### 4.3 Workspace kinds

| `kind` | Path | Exported separately? | Owned by hegn? |
| --- | --- | --- | --- |
| `external` | any safe directory | yes, one `virtiofsd` | no |
| `clone` | `<vms_dir>/<vm>/workspaces/<name>` | yes, one `virtiofsd` | yes |
| `worktree` | inside its parent project | **no**, lives in the parent's share | no |

See §8 for the clone/worktree rules.

### 4.4 Invariants

1. A project has zero or one `vm`. A VM has zero or more projects.
2. A project path appears at most once in one VM. Two VMs may never export the
   same canonical path at the same time.
3. No member may be an ancestor or descendant of another member, with one
   exception: a `worktree` member is intentionally nested inside its `parent`
   and is **not** exported separately.
4. Every exported path must pass the existing import safety checks: not `/`,
   `$HOME`, `projects_root`, an ancestor of it, or anything overlapping hegn's
   state, snapshots, VMs, templates, profiles or config.
5. Read-only is enforced twice: `virtiofsd --readonly` and a guest `ro` mount.
6. A VM with no members cannot start. An empty VM may exist for setup.
7. A member that disappears from disk is a hard boot error. Never boot a
   subset silently. The error lists the missing projects and the detach command.
8. `max_projects_per_vm` (global config) defaults to 16 and is validated
   (hard ceiling 32), because each member is one virtio-fs device and one
   `virtiofsd` process.
9. Interactive access uses SSH over the VM's control TAP. The control TAP has
   no egress; the guarded data TAP remains the only route to the public web. A
   missing `ssh_user`/`guest_ip` is a hard attach error, never a serial
   fallback.

---

## 5. Guest layout and boot

### 5.1 Shares and tags

Members are sorted by canonical path for deterministic ordering. For member
index `i`:

```text
tag        = "hegn-prj-<i>"                 # configurable prefix, <= 36 bytes
socket     = "<run>/virtiofs-<i>.sock"
guest path = "/projects/<slug>"
```

`<slug>` is the sanitized picker name (ASCII letters/digits, `-`, `_`, `.`;
lowercased; max 24 bytes), deduplicated with `-2`, `-3`. Tags are index-based,
so renaming a project's picker name changes a mount path but not the device
ordering. The profile share keeps `hegn-profile` at `/run/hegn/profile`.

`/workspace` is a guest-side symlink to `/projects/<primary-slug>`. It is not
a separate export and cannot point outside `/projects`.

### 5.2 Cloud-init

The seed renders one `mounts:` entry and one `runcmd` mount per member:

```yaml
mounts:
  - [ 'hegn-prj-0', '/projects/parser', virtiofs, "defaults,nofail", "0", "0" ]
  - [ 'hegn-prj-1', '/projects/cli',    virtiofs, "defaults,nofail,ro", "0", "0" ]

runcmd:
  - mkdir -p '/projects' || true
  - mkdir -p '/projects/parser' || true
  - mountpoint -q '/projects/parser' || mount -t virtiofs 'hegn-prj-0' '/projects/parser' || true
  - mkdir -p '/projects/cli' || true
  - mountpoint -q '/projects/cli' || mount -t virtiofs -o ro 'hegn-prj-1' '/projects/cli' || true
  - ln -sfn '/projects/parser' '/workspace'
```

Health output gains a mount count, for example:

```text
hegn-health v3 apt=0 checked=... reason=ok provision=ok mounts=2/2
```

A missing share makes `mounts` fail and the host's `hegn doctor` reports it.
`/etc/gitconfig` gets a `safe.directory` entry for every mount and for
`/workspace`.

The login shell starts in `/workspace` when it exists, so the primary project
is the default working directory. Other projects are reached with
`cd /projects/<slug>`.

### 5.3 Boot flow

1. Resolve selector: project → `vm` name; or an explicit `--vm`.
2. Load `vm.toml`; load the registry; build the ordered membership; validate
   §4.4 (including path nesting, duplicates and `primary`).
3. Take `run/hegn.lock`.
4. Preflight every binary, image **and every member directory** before
   spawning anything.
5. Try auto-resume only when the newest autosave's boot fingerprint matches
   (`§7`).
6. Ensure the per-VM SSH key, reserve the slot's control TAP and, with guarded
   networking, its data TAP; materialize the VM profile.
7. Write the NoCloud seed with all mounts, the `/workspace` symlink, the
   control netplan, sshd configuration and the authorized public key.
8. Spawn one sandboxed `virtiofsd` per member on its indexed socket, plus the
   optional profile daemon.
9. Spawn Cloud Hypervisor with one `--fs` per member, then the profile device,
   and `--net` for the control TAP (data first when guarded); add one Landlock
   sysfs rule per TAP.
10. Start the relay, wait for a running seccomp-confined VMM, write
    `run/membership.json` and `session.json` (with `guest_ip`/`ssh_user`), start
    the autosave worker.

Any failure kills **all** daemons started so far and removes run files; the
serial log is kept, exactly as today.

### 5.4 Attach and start semantics

- Starting any member starts the whole VM. `hegn attach` opens SSH to the
  guest's control address; every invocation gets its own PTY, so several
  sessions (terminals, tmux panes, full-screen TUIs) can run in parallel.
  `--serial` attaches the single serial console instead.
- The VM writes `run/membership.json` at boot. `hegn open` compares it with the
  registry. If they differ (a project was assigned while the VM ran), attach
  succeeds with a warning; changing mounts requires
  `hegn vm apply <vm>` (restart). hegn never kills a running agent
  automatically.
- `hegn attach <vm>` is raw and unguarded, like the serial path. `hegn open
  <project>` waits for provisioning/SSH readiness, attaches and shows the
  project's guest path. `--command` is executed through SSH in that guest
  directory. `hegn vm shell <vm> --project <name>` starts the shell/harness
  over SSH.
- The embedded TUI terminal remains the serial console; `F2` and the external
  attach path open an SSH session. There are no TUI tabs and no VT emulator
  (see §13).

### 5.5 Stop

Stop terminates every SSH session, the VMM, relay, every member `virtiofsd`
(from `membership.json`), the profile daemon and the autosave worker, then
releases the control TAP and, when present, the data TAP. It never deletes a
workspace.

---

## 6. Rootfs, tools and provisioning

- One rootfs per VM, created from the base image on first start (reflink or
  copy). Projects never get their own rootfs.
- The harness preset and `extra_packages` are installed once by the existing
  first-boot provisioner. Later manual installs inside the guest persist in
  that rootfs, which is the point of a shared tool profile.
- Changing `harness` or `extra_packages` does not rewrite an existing rootfs.
  v1 reconciles nothing automatically: users either install inside the guest
  or rebuild (`hegn vm rebuild <vm>`, which creates a new private rootfs and
  requires confirmation). A future guest-side manifest reconciler can close
  this gap without changing the model.
- An explicit `rootfs_image` must be unique across VMs; boot refuses two VMs
  pointing at the same writable disk. This closes a gap in the current model.

---

## 7. Snapshots

Snapshots are per VM and cover its device tree, memory and rootfs.

`SnapshotMeta` v2 records:

```text
vm: "rust"
boot_fingerprint: "sha256:..."
membership: [ { tag, path, slug, read_only, kind } ... ]
```

`membership` lists only the members that are actually exported. Worktrees are
not listed: they ride their parent's share and do not change the device tree,
so adding one does not invalidate a snapshot.

`boot_fingerprint` hashes everything that changes the VMM device tree:

- ordered membership (tag, canonical path, mount slug, read-only, kind),
- `vcpus`, `memory_mib`, rootfs path and format,
- `network_index`, profile-share presence,
- `seed_version` and `network_mode` (control-only vs control+data).

The fingerprint domain is `hegn-snapshot-boot-v3`.

Semantics:

- **Automatic resume:** skip a checkpoint whose fingerprint or domain differs,
  log why, and cold boot. Never restore a VM whose embedded mounts no longer
  exist or whose device tree predates the control NIC.
- **Manual restore:** refuse a fingerprint mismatch with a message naming the
  changed fields and suggesting `hegn vm apply`. There is no `--force`; a
  restored VM cannot be reconciled with added or removed shares or a changed
  network mode after the fact.
- **Restore flow:** reserve the control TAP and, when recorded, the data TAP;
  re-spawn one `virtiofsd` per recorded member on the same indexed sockets;
  regenerate the seed from the recorded membership and network mode; restore the
  VMM with the Landlock rules for both TAPs; and rewrite the session record.
- Snapshots are listed and restored from the VM, not from a project. The
  project screen shows its VM's snapshots.
- v1 snapshot directories have no fingerprint and are listed as legacy,
  non-restorable. They are never deleted automatically.

---

## 8. Parallel workspaces: clones and worktrees

Both modes exist; **clone is the default**. A fork is a project member, not a
new VM, so it inherits the VM's tools and rootfs.

### 8.1 Clone (default, isolated)

```bash
hegn project fork parser --name parser-fix-tests
```

- `git clone` into `<vms_dir>/<vm>/workspaces/<name>`.
- Independent `.git`, refs, hooks and object store; one agent cannot corrupt
  the source repository.
- Its own sandboxed `virtiofsd` and mount, so it can be read-only, detached or
  deleted independently.
- `kind = "clone"`, `managed = true`, `parent = "parser"`.
- Use `git clone --local`/reflink when source and destination share a
  filesystem to keep it cheap; fall back to a normal clone.
- Deleting the clone deletes the managed workspace only after confirmation;
  the parent project is untouched.

Why not a worktree by default: a worktree's `.git` is a file pointing at
`<main>/.git/worktrees/<name>`, outside the single exported directory. The
guest cannot resolve it, and exporting the main repository to fix that lets
one agent mutate the other's refs and hooks.

### 8.2 Worktree (opt-in, nested, shared git state)

```bash
hegn project fork parser --mode worktree --name parser-experiment
```

- The worktree **must live inside the parent project's exported tree**, for
  example `<parent>/.hegn/worktrees/<name>`, so its gitdir resolves through
  the parent share.
- It is **not** a separate export: `kind = "worktree"`, `parent = "parser"`,
  and it is skipped when building the virtio-fs device list.
- Its `vm` is inherited from the parent and cannot be assigned independently;
  detaching the parent detaches its worktrees.
- It must be a real worktree of the parent (`git worktree list` check). The
  parent must be a member of the same VM and writable; a read-only parent
  cannot host worktrees.
- hegn never deletes a worktree directory implicitly. Removal uses
  `git worktree remove` after explicit confirmation. Deleting the parent's
  files also deletes nested worktrees, so that confirmation names them.
- Documented risk: worktrees share refs, hooks and the object store. A
  destructive git command in one worktree can affect the parent. This is the
  opt-in tradeoff for near-zero disk and instant creation.

Adding an existing host worktree as a project is supported only when its
gitdir resolves inside the exported tree. Otherwise hegn refuses with the
clone alternative.

---

## 9. Security model changes

The existing invariants stay; the following are new or strengthened.

1. **One sandboxed daemon per export, still.** Multiplicity is achieved with N
   processes, not a wider export. Each daemon canonicalizes its own directory
   and uses `--sandbox namespace|chroot` and `--cache never`.
2. **Agent cannot change membership.** The registry is private (`0600`, no
   symlink following, atomic writes, flock). Repo-local `hegn.toml` cannot
   affect exports.
3. **Nesting and overlap are rejected** at boot, including two members where
   one contains the other. The only nested member is a worktree, which has no
   share of its own.
4. **Duplicate export is rejected.** A project assigned to one VM is checked
   again at boot, and a cross-VM scan refuses two running VMs exporting the
   same canonical path.
5. **Dynamic guardrail roots.** `tirith-core` destructive and exfiltration
   checks currently hardcode `/workspace`. The engine receives the VM's
   guest roots (`/projects/*` plus `/workspace`) and treats all of them as
   workspace. `rm -rf /projects/parser` is guarded exactly like
   `rm -rf /workspace`.
6. **Projects in one VM are mutually visible.** The guest user can read and
   (unless read-only) write every member. Read-only protects writes, not
   reads. Membership is the trust boundary; unrelated or untrusted projects
   belong in separate VMs. Secrets in one project's `.env` are readable by an
   agent working in another project of the same VM.
7. **Project deletion no longer deletes a VM.** Removing a project unassigns
   it. Deleting files never touches sibling projects, other members or a VM.
   A VM is deleted only by `hegn vm delete`, with its own confirmation.
8. **`/workspace` is a validated symlink** to a mounted primary project,
   created inside the guest. It cannot point at the host or outside
   `/projects`.
9. **Explicit rootfs images are unique per VM** and checked before boot.
10. **SSH is a management channel.** Each VM's client key and `known_hosts`
    live in its private `ssh/` directory. SSH terminates on the control TAP,
    which has no route, DNS, forwarding or masquerade; the guarded data TAP is
    still the only egress path. sshd is public-key only for `ssh_user`, and
    agent/TCP forwarding are disabled. A missing key or stale session record is
    a hard error; hegn never falls back to serial.

---

## 10. CLI and TUI surface

### 10.1 CLI (replaces `hegn instance`)

```text
hegn vm list
hegn vm create <name> [--harness H] [--vcpus N] [--memory MiB] [--package P]...
hegn vm show <name> [--json]
hegn vm config <name> [--path|--edit]
hegn vm start <name>
hegn vm stop <name>
hegn vm restart <name>
hegn vm apply <name>              # restart when membership/config changed
hegn vm rebuild <name>            # new private rootfs from the base image
hegn vm primary <name> <project>
hegn vm projects <name>
hegn vm shell <name> [--project <project>] [--serial]
hegn vm delete <name> [--keep-workspaces|--delete-workspaces] [--yes]

hegn project create <name> [--vm <name>] ...
hegn project import <path> [--vm <name>] [--name <picker-name>]
hegn project assign <project> --vm <name>
hegn project unassign <project>
hegn project fork <project> [--mode clone|worktree] [--name <name>] [--vm <name>]
hegn project delete <project> [--keep-files|--delete-files]
hegn projects [query]             # now shows the VM column

hegn open <project> [--vm <name>] [--serial] [--command C]
hegn attach <vm|project> [--project <name>] [--serial] [--no-bar] [--command C]
hegn launch <project> [--command C] [--integration I] [--serial]
hegn snapshot create|restore|list|delete --vm <name> ...
hegn logs ...                     # per VM, filterable by project
```

- Omitting `--vm` on `project create/import` asks user if he likes to creat a solo VM named after the
  project (similar to old behavior, explicitly represented as a one-member VM).
- `hegn instance` remains only as a hidden command that prints the new mapping
  and exits non-zero, so scripts fail with a readable message.
- Every TUI action keeps a CLI counterpart.

### 10.2 TUI

The project picker stays the home screen.

- **Dashboard:** each project row shows its VM badge (or `unassigned`) next to
  git status. The preview panel shows the project's VM: harness, member count,
  VM state, newest snapshot, and the project's guest path.
- **Project screen:** four sections become
  `1 VM` (state, tool profile, mount path, sibling projects),
  `2 Snapshots`, `3 Health`, `4 Git`.
  The old instance list is replaced by the VM and its member list.
- **VM screen (`v`):** a reusable VM view with sections
  `1 Projects` (assign/detach/set primary), `2 Snapshots`, `3 Tools`
  (harness, packages, resources, provisioning state), `4 Health`.
  Start/stop/apply/shell live here.
- **Attach:** `Enter` on a project attaches its VM. External attachments
  (`F2`, external/tmux/herdr integrations) open an SSH session in the current
  terminal; the host title is `hegn: <vm> · <project>`. The embedded terminal
  remains the serial relay, with no tabs or VT emulator. Missing
  `guest_ip`/`ssh_user` produces a TUI error that points at `--serial`.
- **Settings:** `F4` global, `F5` VM (`vm.toml`), `F6` project presentation
  preferences. The project settings modal no longer contains `harness`.
- **Modals:** create VM, edit VM, assign project to VM, fork workspace
  (clone/worktree), detach project, delete VM.
- **State:** `App.instance` becomes `App.vm`; `Screen` gains `Vm`; `Effect`
  gains `StartVm`, `AttachVm`, `ApplyVm`, `AssignProject`, `UnassignProject`,
  `ForkWorkspace`, `DeleteVm`. Attachment is keyed by VM name, not project.

---

## 11. Migration: hard cut to v2

No automatic migration.

- v2 reads `vms_dir` and the private project registry. The legacy
  `instances_dir` is ignored.
- `hegn doctor` reports legacy instance directories, legacy snapshots without
  a fingerprint, and legacy repo `hegn.toml` keys, with a one-line
  remediation for each. Nothing is deleted.
- Existing logs remain readable; they are diagnostics, not state.
- Global `config.toml` accepts the old `instances_dir`, `shared_tag` and
  `guest_mount` keys as deprecated aliases with warnings, so an upgrade does
  not brick startup. The new keys are `vms_dir` (default
  `~/.local/share/hegn/vms`), `share_tag_prefix` (default `hegn-prj`) and
  `guest_projects_base` (default `/projects`).
- Legacy snapshots and instances are never selected for restore.

The SSH attach release upgrades an existing v2 installation in place:

1. Run `sudo hegn network setup --user "$USER"` again; the marker moves to
   version 3 and the control TAP family is created.
2. Install the host OpenSSH client (`ssh`, `ssh-keygen`); `hegn doctor` checks
   both.
3. `hegn vm apply <vm>` regenerates the seed (version 13) and restarts the VM,
   which re-runs cloud-init, configures sshd/control netplan and rotates the
   guest host keys.
4. Snapshots from before seed version 13 or the v3 fingerprint domain are not
   resumed; the next start cold boots.
5. A VM whose `session.json` predates `guest_ip`/`ssh_user` refuses SSH attach;
   use `hegn attach <vm> --serial` until `hegn vm apply` has refreshed it.

v1 projects are obviously importable via:

```bash
hegn vm create rust --harness opencode
hegn project import ~/code/parser --vm rust
```

although I'm the only user...

---

## 12. Failure handling

| Failure | Behaviour |
| --- | --- |
| VM has no members | Hard error before spawn; `hegn project assign` hint |
| Member path missing | Hard error listing every missing member; no partial boot |
| Duplicate or nested members | Validation error before spawn |
| Two VMs export the same path | Boot fails; names both VMs |
| More than `max_projects_per_vm` | Validation error before spawn |
| One `virtiofsd` fails | Kill all member daemons, VMM and relay; keep log |
| Membership changed while running | Attach warns; `hegn vm apply` restarts |
| Auto-resume fingerprint mismatch | Skip checkpoint, cold boot, log reason |
| Manual restore fingerprint mismatch | Refuse and name the changed fields |
| Legacy v1 instance/snapshot | Ignored, reported by doctor, never deleted |
| Snapshot without the control NIC | Rejected; cold boot instead of resume |
| SSH refused or authentication fails | Hard error naming `--serial`/`hegn vm apply`; VM keeps running |
| Session record lacks `guest_ip`/`ssh_user` | SSH attach refused with a `--serial` hint |
| Shared explicit rootfs | Boot fails for the second VM |
| Read-only project | virtiofsd `--readonly` plus guest `ro` mount |

---

## 13. Known limitations

- **External SSH sessions only.** Every `hegn attach` gets its own guest PTY,
  so several agents can run in parallel without sharing the serial line. There
  is no hegn-side multiplexer, TUI tab bar, VT emulator or SSH status bar, and
  no vsock broker (ADR-011). The embedded TUI terminal still shows the serial
  console. Guest `tmux` remains the way to survive a disconnect.
- **Membership changes need a restart.** There is no live hot-plug of
  virtio-fs devices; `vm apply` is explicit because it kills running agents.
- **Tool changes are not reconciled.** The provisioner installs the baseline
  once; later changes are manual in the guest or a rootfs rebuild.
- **Worktrees share mutable git state** with their parent. That is documented
  and opt-in; clones remain the isolation-preserving default.
- **N daemons cost N processes.** Shared-tool VMs reduce rootfs and provision
  cost but increase per-VM process count. `max_projects_per_vm` bounds it.
- **No per-project snapshots.** A snapshot belongs to the VM and covers every
  member; that is the price of a shared rootfs and runtime.
- **No inter-project isolation inside a VM.** Every member is visible at
  `/projects/<slug>`. Group only projects that may read each other's files and
  secrets; use separate VMs for a hard boundary.

---

## 14. Testing plan

- Registry: assignment uniqueness, nesting rejection, worktree-parent rules,
  managed clone paths, hidden projects, atomic writes and lock behaviour.
- VM config: v2 validation, primary membership, resource bounds, deprecated
  global keys.
- Membership: deterministic ordering, slug dedupe, fingerprint stability and
  change sensitivity.
- Cloud-init: N mounts, per-member read-only, `/workspace` symlink, health
  mount count, injection-safe names.
- VMM argv: N `--fs` devices in order, indexed sockets, profile device,
  restore device omission.
- Lifecycle: every member preflighted before spawn; one failed `virtiofsd`
  cleans all; stop kills every daemon from the manifest.
- Snapshots: mismatch skips auto-resume; manual restore refuses; restore
  re-spawns the recorded daemons and reserves control (+ optional data) TAPs.
- SSH: exact argv, key permissions, missing guest IP/user, readiness probe,
  host-key rotation and `--serial` selection.
- Security: destructive commands in any `/projects/*` root; membership file is
  not agent-writable; duplicate/nested export refused.
- TUI: `TestBackend` coverage for the VM badge, VM screen, assign/fork modals,
  attach title, and footer hits.

---

## 15. Implementation plan

Each phase compiles, passes `cargo fmt --check`, clippy and tests, and gets
its own commit and review.

1. **Registry and config v2** — private project registry, `vm.toml`, global
   key renames/deprecations, validation and tests.
2. **Multi-share boot** — N `virtiofsd`, N `--fs`, cloud-init mounts and
   symlink, membership manifest, stop/cleanup.
3. **Snapshots** — fingerprint in metadata, auto-resume skip, N-daemon
   restore, legacy reporting.
4. **Security** — dynamic workspace roots for tirith, duplicate/nesting checks,
   rootfs uniqueness.
5. **CLI** — `vm` and `project` surfaces, hidden `instance` migration error,
   output and JSON.
6. **TUI** — VM badge, VM screen, modals, attach semantics, settings split.
7. **Workspaces** — clone-by-default fork, opt-in worktree, safe deletion.
8. **Docs and diagnostics** — user docs for the new model, doctor findings,
   benchmark adjustments.

---

## 16. Open questions

1. **Repo-local presentation config.** Keep `launch`/`integration`/`auto_start`
   in the project's `hegn.toml`, or move everything private? This design keeps
   the presentation plane in the repo and the control plane private.
2. **Guest multiplexing (resolved).** Parallel sessions use SSH over the
   control TAP, one guest PTY per attach; no vsock broker (ADR-011). Guest
   `tmux` is optional persistence, not the transport.
3. **Cwd injection (resolved).** The SSH login shell starts in `/workspace`;
   `--command` runs through SSH exec with the selected project's guest CWD. hegn
   does not type `cd` into a shared serial shell.
4. **Tool reconciliation.** When should package changes be applied: manual in
   the guest, an explicit `vm rebuild`, or a guest-side manifest reconciler?
