# hegn architecture

This document describes how `hegn` is put together, which invariants the
implementation must preserve, and how the pieces fail. It is the detailed
companion to [`../README.md`](../README.md) (user documentation); measured host
cost and storage figures live in [`resources.md`](resources.md).

`hegn` is a Linux-only Rust application that runs coding agents inside
Cloud Hypervisor VMs. The host process never runs project work itself. Every
shell and agent runs in a real Cloud Hypervisor guest. Interactive sessions
attach over SSH through a per-VM control TAP by default; the serial console
remains the boot, provisioning and recovery channel and is selected explicitly
with `hegn attach <vm> --serial`. Multiple VMs can run at the same time; each one
owns its configuration, runtime directory, serial log and private root
filesystem. A VM hosts one or more projects, each exported by its own sandboxed
`virtiofsd`.

---

## 1. Design goals

1. **Hardware isolation first.** A missing VM is a hard error. There is no
   host shell, local PTY, container or "temporary" fallback anywhere in the
   codebase.
2. **Fail closed.** Binaries, images, the VM profile, the registry and every
   member directory are checked before any child process is spawned. Partial
   failures are cleaned up rather than left running.
3. **No silent bypasses.** Input paths that skip `tirith-core` are visibly
   labelled (`INPUT DIRECT`). Unverified image downloads require an
   explicit flag.
4. **Predictable state.** Runtime files live in one private state directory;
   snapshots and logs are separate, private, and survive restarts.
5. **Operator control.** Every TUI action has a non-interactive CLI equivalent
   so the same operations can be scripted, audited or performed over SSH.

---

## 2. System context

```text
┌────────────────────────────────────────────────────────────────────────────┐
│ HOST (Linux, KVM)                                                          │
│                                                                            │
│  hegn CLI / TUI                                                            │
│    ├── security.rs ── tirith-core (in-memory inspection)                  │
│    ├── project/    ── ~/Projects/hegn picker, registry, forks              │
│    ├── vm/manager  ── lifecycle, lock, membership, session record          │
│    │     ├── vm/config      ── vm.toml, VmStore                            │
│    │     ├── project/registry ── private membership and read-only flags    │
│    │     ├── cloud_hypervisor.rs ── REST over unix socket                 │
│    │     ├── virtiofs.rs    ── one sandboxed daemon per exported project   │
│    │     ├── relay.rs       ── detached serial relay + persistent log     │
│    │     ├── cloud_init.rs  ── NoCloud seed with N mounts + /workspace     │
│    │     ├── profile.rs     ── profiles + copied skill bundles            │
│    │     ├── project/workspace ── clones and nested worktrees             │
│    │     ├── ssh/           ── per-VM key, known_hosts, OpenSSH client     │
│    │     ├── console.rs     ── relay socket + serial attach bar           │
│    │     ├── snapshot/      ── fingerprint, metadata, restore planning    │
│    │     └── logs.rs        ── private logs, tail and pruning             │
│    ├── images.rs ── upstream kernel/rootfs download + SHA-256             │
│    └── doctor.rs ── shared prerequisite checks                            │
│                                                                            │
│  virtiofsd (sandboxed, member 0) ◄──── project A directory                 │
│  virtiofsd (sandboxed, member 1) ◄──── project B directory                 │
│  Cloud Hypervisor ── /dev/kvm, API/serial sockets, control + data TAPs,   │
│        │              seccomp + Landlock                                  │
│        ├── guest serial console ── boot, provisioning, recovery           │
│        └── guest sshd on the control TAP ── SSH PTYs ── agent / shell     │
└────────────────────────────────────────────────────────────────────────────┘
```

The guest sees one mount per exported project below `/projects`, a guest-side
`/workspace` symlink to the VM's primary project, and an optional share
containing only the VM's private profile/skill copy. The host sees control
sockets, logs and metadata. Every VM gets a control TAP (`hegnc<i>`) from
`hegn network setup`; SSH terminates there and the link has no default route,
DNS, forwarding or masquerade, so management traffic cannot leave the host. With
`network = "guarded"` the guest additionally gets a data TAP (`hegn<i>`): public
HTTP(S) and public DNS only, with no route into the host or the local network.
`network = "off"` keeps the control TAP for SSH but removes the data NIC, so the
guest has no egress at all.

### 2.1 Native virtio-fs and VMM hardening

Cloud Hypervisor implements virtio-fs natively. hegn passes one
`--fs tag=<tag>,socket=<virtiofsd socket>` per exported project together with
`--memory size=<MiB>M,shared=on`; no patched VMM build is required. Tags are
index-based (`<share_tag_prefix>-<i>`), so the device order is stable even when a
project is renamed.

The VMM is always launched with seccomp filters and Landlock; there is no
configuration switch to disable either layer. Landlock restricts the VMM's
filesystem view to the kernel, the
disks, the serial socket, the virtio-fs sockets and a dedicated snapshot
staging directory. For every TAP passed to the VMM, hegn adds a read-only rule
for that VM's TAP directory in sysfs: `/sys/devices/virtual/net/hegnc<i>` for
the always-present control TAP and, with guarded networking,
`/sys/devices/virtual/net/hegn<i>` for the data TAP. Cloud Hypervisor checks an
existing TAP's multiqueue support through that sysfs path before opening it, and
its own ruleset only grants `/dev/net/tun`. Without the matching rule the VMM
fails with `ReadSysfsTunFlags`/`EACCES` while creating virtio-net. Other host
interfaces and the rest of `/sys` stay invisible to the VMM. Each `virtiofsd`
independently runs with `--sandbox=namespace` (or `chroot`) and `--cache=never`.
A finished snapshot is moved out of the staging directory and its files are
tightened to `0600`, so the VMM never gets write access to completed
checkpoints.

---

## 3. Runtime topology

### 3.1 Processes

A running session consists of:

| Process | Purpose | Lifetime |
| --- | --- | --- |
| `hegn` CLI/TUI | UI, orchestration, guardrails, API calls | Foreground; may exit while VM continues |
| `ssh` (OpenSSH client) | Interactive guest PTY or `--command` over the control TAP | One process per attached session; ends with that session |
| `virtiofsd` (one per member) | Exports exactly one canonical project directory under `--sandbox=namespace` (or `chroot`) | Until `vm stop` / restore |
| `cloud-hypervisor` | KVM VMM | Until `vm stop` / restore / guest exit |
| `hegn __console-relay` | Serial socket client, persistent log, attach socket | Until the VMM exits or `vm stop` |
| `hegn __autosnapshot` | Detached periodic checkpoint worker | Until `vm stop` / VMM exit |

There is one `virtiofsd` per exported project, one `cloud-hypervisor` and one
relay per VM. `VmRegistry` starts, lists, stops and deletes independent VMs; the
project registry supplies the member list. Worktrees are members but are not
exported, so they do not add a daemon.

The CLI deliberately lets `virtiofsd`, Cloud Hypervisor and the console relay
outlive itself. It forgets the `Child` handles only after the session record is
written; on any error before that point it kills every child started for the VM
and removes the run files.

### 3.2 Run directory

Each VM has its own runtime directory below `vms_dir/<name>/run`:

```text
<vms_dir>/<vm>/run/
  cloud-hypervisor.sock  Cloud Hypervisor REST API socket
  serial.sock            Cloud Hypervisor guest serial socket
  console.sock           relay socket used by the TUI / serial attach
  virtiofs-<i>.sock      virtiofsd vhost-user socket for member i
  profile-virtiofs.sock  virtiofsd vhost-user socket (profile/skill copy)
  cloud-hypervisor.pid   last known VMM pid
  console-relay.pid      last known relay pid
  virtiofs-<i>.pid       last known virtiofsd pid for member i
  profile-virtiofs.pid   last known profile virtiofsd pid
  autosave.pid           last known checkpoint worker pid
  session.json           SessionRecord
  membership.json        exported members of the running boot
  cloud-hypervisor.log   VMM log
  console-relay.log      relay stderr (startup failures only)
  autosave.log           checkpoint worker stderr
  hegn.log               lifecycle/start diagnostics (timestamped, mode 0600)
  hegn.lock              flock for lifecycle operations
  cloud-init.img         per-VM NoCloud seed (no secrets)
```

`run/` is created with mode `0700`. All sockets live inside it; the relay
socket is additionally `0600`. The cloud-init seed is `0600` and contains only
public boot configuration. `membership.json` is `0600`, written atomically, and
lists the tag, path, guest path, read-only flag and kind of every exported
member. `stop` kills daemons from that manifest, and `attach` compares it with
the registry.

### 3.3 Persistent log

```text
<state>/logs/
  session-<timestamp>-<project>-<pid>.log
```

The log is created before Cloud Hypervisor starts, mode `0600`, with
`O_NOFOLLOW | O_CREAT | O_EXCL`. The detached relay is the only client of the
VMM serial socket and appends every byte to this file. On attach it replays the
tail of the log and then streams live output, so `vm stop` leaves a
complete log for `hegn logs`.

Logs may contain secrets printed by an agent. They are never uploaded, and
`hegn logs show` strips terminal control and bidi-override characters unless
`--raw` is passed. Pruning deletes only hegn's own `session-*.log` files and
never the log of the running session.

### 3.4 Data directories

| Path | Contents | Permissions |
| --- | --- | --- |
| `~/.config/hegn/config.toml` | User config | user default |
| `~/.config/hegn/templates/` | `AGENTS.md` templates | user default |
| `~/.local/state/hegn/projects.toml` | Private project registry: membership, read-only, kind, lineage | `0600`, lock file beside it |
| `~/.local/state/hegn/logs/` | Persistent serial logs | `0700`, files `0600` |
| `~/.local/state/hegn/state.json` | Recent projects / last project | user default |
| `~/.local/share/hegn/snapshots/` | Completed snapshots (`config.json`, `state.json`, `memory-ranges`, `snapshot.json`) | `0700`, files `0600` |
| `~/.local/share/hegn/snapshots/.staging/` | The only snapshot path the VMM may write | `0700` |
| `~/.local/share/hegn/vms/<name>/` | `vm.toml`, `run/`, `ssh/`, private rootfs, managed workspaces | `0700` |
| `~/.local/share/hegn/vms/<name>/ssh/` | Per-VM SSH client key, public key and `known_hosts`; never exported | `0700`; private key `0600` |
| `~/.local/share/hegn/vms/<name>/vm.toml` | Per-VM tool profile and policy | `0600` |
| `~/.local/share/hegn/vms/<name>/workspaces/<name>/` | hegn-managed clone checkouts | `0700` |
| `~/.local/share/hegn/images/` | Kernel and rootfs | files `0600` |
| `~/Projects/hegn/` | Projects and import symlinks | user default |

All state paths can be overridden in `config.toml` or by environment variables
(see [`configuration.md`](configuration.md)).

---

## 4. Modules

| Module | Responsibility | Notable invariants |
| --- | --- | --- |
| `cli/` | Clap command surface, per-command modules, JSON/text output, exit codes | Every TUI operation has a CLI counterpart; no fallback paths |
| `config/` | Schema, defaults, env overrides, validation, XDG paths, tool lookup; `form/` is the interactive settings editor | Unknown keys rejected; SHA-256 and HTTPS URL validation; log dir `0700`; `virtiofsd` resolved via `PATH` then distribution dirs; writes are validated and installed with an atomic tmp+rename; form edits a validated working copy and never touches disk before `Ctrl-S` |
| `project/` | Discovery, create, import, git summaries, registry, deletion policy | The registry is private and authoritative; import only creates a symlink; dangerous targets rejected; direct picker entries are deleted only as direct children of `projects_root`, while registry-only External projects outside it require `--delete-files` and are re-validated as safe export sources first; import links are never followed |
| `project/registry.rs` | Private `<state_dir>/projects.toml`: membership, read-only, kind, lineage, validation | `0600`, `O_NOFOLLOW`, atomic writes, `flock` for writers; unique names and canonical paths; no ancestor/descendant except a named nested worktree; `max_projects_per_vm`; worktrees inherit their parent's VM |
| `project/workspace.rs` | Clone and worktree forks | Clone is default and lives at `<vms_dir>/<vm>/workspaces/<name>`; clone target verified as a real `.git` directory at the parent HEAD; worktree must be a real `git worktree` inside its writable parent; deletion uses `git worktree remove` or a checked managed path |
| `security/` | `tirith-core` integration and hegn workspace rules | Empty commands allowed; off mode disables tirith but still detects and warns about destructive workspace operations; every `/projects/*` root and `/workspace` is a workspace root |
| `network/` | Control and guarded data TAP pools, nftables rules, setup marker, privileged setup scripts | Root-only marker; anti-spoof; control TAP has no egress; private/IPv6/guest-to-host drop on data; HTTP(S)+public DNS allow-list on data; per-VM `/30` slice in each subnet; stale reservations reclaimed only when the owning VM is not running; opt-in loopback endpoints are DNATed per data TAP with `route_localnet` and cannot write; data TAP pool exempted from ufw/firewalld/iptables routed policies because netfilter applies every `forward` base chain |
| `vm/hypervisor/` | Typed REST client over `/api/v1` and HTTP framing | Frames responses by `Content-Length`/chunked encoding, accepts bodyless `204`, bounded response sizes, pause/resume/info/snapshot only |
| `vm/cloud_init/` | NoCloud seed generation, user-data rendering, sshd drop-in, identity and endpoint env | FAT volume labelled `CIDATA`; serial autologin for recovery; control/data netplan; public-key-only sshd; regenerated host keys; first-boot provisioner; no private keys in the seed; `0600` and `O_NOFOLLOW` |
| `vm/rootfs/` | Per-VM disk growth | Extends raw images sparsely and grows qcow2 virtual size in place; `qemu-img` fallback for unsupported layouts |
| `vm/images/` | Upstream asset pins, streaming download, SHA-256 install/verify | HTTPS only, no credentials in the URL, HTTPS-to-HTTP redirects refused; checksum before rename; no overwrite without `--force` |
| `agent/launch/` | Configurable agent launch commands and terminal integrations | Commands are validated and run in the guest over SSH for external integrations; the embedded terminal stays on the serial relay; `auto` prefers herdr, then tmux, then the current terminal; tmux/herdr run `hegn attach`, never a host agent |
| `agent/profile/` | Per-harness profile templates, guest payload materialization, skill sync | Source/target/mode/env validation; config files refreshed every start; skills seeded once into a private per-VM copy; a second sandboxed virtiofsd exports only that copy; sync-back copies regular files and skips symlinks/special files |
| `agent/provision/` | Harness preset install plans and first-boot script | Preset packages are always unioned; package names validated; only static install commands; waits for DNS, retries apt, emits serial progress stages and verifies the CLI before writing the marker |
| `vm/virtiofs/` | virtiofsd argv/process construction | One daemon per exported member; shared directory canonicalized; always sandboxed; `--cache=never` by default; indexed sockets scoped to the VM run dir |
| `vm/config/` | `vm.toml` model and `VmStore` | Version 2; unknown keys rejected; name becomes a path component and is validated; `0600`, `O_NOFOLLOW`, atomic writes; one config per `<vms_dir>/<name>` |
| `vm/manager/` | VM lifecycle, registry, lock, boot plan, membership manifest, session record, snapshots | One lifecycle lock per VM; registry validation and member preflight before spawn; one `--fs` per exported member; always a control lease, data only with guarded networking; every error path kills all daemons; duplicate cross-VM export refused; explicit rootfs images unique |
| `session/snapshot/` | Snapshot metadata, v3 boot fingerprint, restore selection | Sanitized names; private dirs/files; atomic metadata writes; legacy snapshots visible but never restored, auto-selected or pruned; manual restore refuses a fingerprint mismatch |
| `diagnostics/doctor/` | Shared host checks and automatic setup planning for CLI and TUI | Required vs optional checks; covers `ssh`/`ssh-keygen`, marker version 3, control TAPs, nft control rules and per-running-VM `guest_ip`/`ssh_user`/key; `SetupActions` covers directories, default config, profile templates, pinned images and the control/guarded network; `hegn doctor --fix` and the TUI `s` action are idempotent |
| `diagnostics/benchmark/` | Measured host cost: process RSS, disk usage, snapshot estimate; boots a VM only when asked | Refuses to delete anything outside its own `hegn-benchmark-*` projects; sparse-aware accounting; JSON output |
| `session/ssh/` | Per-VM OpenSSH identity, `known_hosts`, fixed client argv, readiness probe and terminal title | Key `0600` in a `0700` directory; `-F /dev/null` and explicit options; public-key only; no agent/TCP forwarding; hard error instead of serial fallback |
| `session/console/` | Serial transport over the relay socket, tmux-style attach, status bar | Embedded TUI view and `--serial`; never pretends a VM exists; verifies VMM and relay; bar content sanitized; `--no-bar` for full-screen guests |
| `session/attach_bar/` | Bottom status bar overlay and sequence-safe output gate | Clamps DECSTBM so the bar stays pinned; repaints after every output burst; never injects into a split escape sequence or UTF-8 character |
| `session/logs/` | Log naming, listing, resolve, tail, prune | `O_NOFOLLOW`, regular files only, no foreign files deleted, active log protected |
| `session/relay/` | Detached serial relay | Sole VMM serial client; append-only log; replays the log tail on attach; bounded buffers; refuses to replace non-socket paths; stderr goes to `run/console-relay.log` because it has no terminal |
| `session/terminal/` | ANSI-aware scrollback | Strips CSI/OSC; handles CRLF without losing lines; caps line bytes and scrollback |
| `session/state/` | Recent and hidden-project persistence | Atomic tmp+rename |
| `tui/app/` | TUI state machine, modals, key/mouse handling, effects | Pure state transitions; side effects returned as `Effect` |
| `tui/ui/` | Ratatui rendering | Pure renderer; footer, row and settings-stepper hit rectangles; all modals testable with `TestBackend` |
| `tui/` | Terminal lifecycle, event loop, background jobs | Restores terminal on every exit path; project scan and image download off the UI thread |
| `shared/` | Dependency-free fs, process, path, text and numeric helpers | No domain imports, so it can be used by every domain without cycles |

---

## 5. Lifecycle flows

### 5.1 Boot (`hegn open`, `hegn vm start`, TUI open)

1. Resolve the selector: a project to its `vm` assignment, or an explicit VM
   name. Load `config.toml`, `<vms_dir>/<vm>/vm.toml` and the private registry.
2. Build the ordered boot plan from `registry.exported_members(vm)`, sorted by
   canonical path. Validate unique names and paths, nesting, worktree parents,
   `primary`, `max_projects_per_vm`, and every member's export safety. An empty
   VM, a missing member or a duplicate export is a hard error.
3. Preflight every binary, image **and every member directory**; check KVM and
   Landlock; ensure no other running VM exports the same canonical path; check
   that an explicit `rootfs_image` is unique.
4. Take the VM's exclusive `run/hegn.lock`. A second lifecycle operation on the
   same VM fails fast; other VMs are independent.
5. If a stale session exists, clean up its run files and any verified children.
6. Create the session log (`LogStore::create_session`); prune old logs
   best-effort. Give the VM a private rootfs copy on first start (reflink or
   full copy) and extend it to a 16 GiB sparse virtual size so the first-boot
   package installation has room. qcow2 images get a larger L1 table without
   rewriting existing clusters.
7. Materialize the harness profile into `<vm>/profile/bundle`: refresh
   configuration files and environment from the host, keep existing skill
   copies, and compute the manifest id. Missing optional profile sources only
   produce a `run/hegn.log` warning. Profile presence is part of the boot
   fingerprint.
8. If the newest automatic checkpoint's boot fingerprint matches, restore it.
   Otherwise log why it was skipped and cold boot.
9. Ensure the per-VM SSH client identity exists: `<vm>/ssh/id_ed25519` (mode
   `0600`, directory `0700`) and its public key. A missing or corrupt private
   key is regenerated before boot; `known_hosts` stays per VM.
10. Require the root-owned `/run/hegn/network.json` marker (version 3) and
    reserve the slot's **control** TAP. With `network = "guarded"` the same
    lease also claims the data TAP; with `network = "off"` only the control TAP
    exists. The VM's `network_index` is the TAP slot, is set in every mode and
    is persisted so later boots and snapshot restores keep both devices.
11. Write the NoCloud seed when `cloud_init = true`. It enables serial autologin
    for recovery, renders one mount per exported project below
    `guest_projects_base` (`/projects` by default), links `/workspace` to the
    primary project, mounts the profile copy at `/run/hegn/profile`, installs
    the harness preset through a oneshot service (which re-applies the profile
    on every boot), configures the control interface by MAC (static
    `10.214.0.<4i+2>/30`, no default route, no DNS), configures the data
    interface only when guarded, installs the public-key-only sshd drop-in and
    authorizes the per-VM client key for `ssh_user`.
12. Spawn one sandboxed `virtiofsd` per exported member on its indexed socket
    (`virtiofs-<i>.sock`), plus the optional profile daemon, and wait for each
    socket.
13. Spawn Cloud Hypervisor with an explicit, hardened argv: `--api-socket`,
    `--kernel`, `--disk` (rootfs + optional seed), `--cpus`, `--memory ...
    shared=on`, one `--fs` per exported member plus the optional profile
    device, `--net` for the control TAP (and one for the data TAP before it
    when guarded), `--serial socket=...`, `--console off`, `--seccomp` and
    `--landlock` with one sysfs rule per TAP.
14. Spawn the detached relay; it connects to the serial socket and appends to
    the session log. Its stderr goes to `run/console-relay.log`. If the relay or
    the VMM exits before the attach socket exists, the start error names the
    real cause and includes the last log lines instead of blaming the relay.
15. Wait until `/api/v1/vm.info` reports `Running`. Write the pid files,
    `membership.json` and `session.json` (including `guest_ip` and `ssh_user`);
    start the autosave worker; release the lock. The children keep running.

On any failure every daemon and the VMM are killed and run links are removed,
but the session serial log is kept and `run/hegn.log` receives a timestamped
diagnostic line. The CLI error repeats the `hegn.log` path. A failure to record
the session tears the VM down instead of leaking an unmanageable process.
Keeping the failed serial log is intentional: early boot and VMM diagnostics
remain available.

### 5.2 Attach (`hegn attach`, `hegn a`, `hegn open`)

1. Resolve a VM name, or a project name to its VM. Read `session.json`; fail if
   missing.
2. Verify the recorded VMM pid is alive **and** still runs the configured
   Cloud Hypervisor binary (`/proc/<pid>/cmdline`).
3. Choose the transport. SSH is the default; `--serial` selects the serial
   console. There is no automatic fallback in either direction.
4. **SSH path:** require a running VM, `guest_ip` and `ssh_user` in the session
   record, the `ssh` binary and the per-VM key. Build the target
   `<ssh_user>@<guest_ip>:22` with the fixed OpenSSH argv (`-F /dev/null`,
   `IdentitiesOnly=yes`, per-VM `known_hosts`, `StrictHostKeyChecking=accept-new`,
   public-key-only, `BatchMode=yes`, keepalives, no agent or TCP forwarding,
   `-t`). `--command` runs the command through SSH exec in the project's guest
   directory instead of a login shell. Every invocation gets its own guest PTY,
   so several sessions per VM run in parallel and resize independently. The
   host sets the terminal title `hegn: <vm> · <project>` and restores it on exit.
   The remote exit status is propagated; SSH's own transport failure (255) is
   reported as a hegn error with a hint.
5. **Serial path:** verify the recorded relay pid still runs the internal relay
   command, compare the boot-time `membership.json` with the registry (a
   difference only warns until `hegn vm apply`), and connect to `console.sock`.
   The relay replays the log tail, then forwards input and live output. Enter
   the alternate screen, reserve the bottom line with DECSTBM, and forward
   input. The bar is repainted after every output burst and the region is
   re-reserved after a terminal reset. `Ctrl-B` (or `Ctrl-]`) detaches; the VM
   keeps running. `--no-bar` is serial-only and uses the full terminal for
   full-screen guest programs.

Attach never boots, restarts or repairs a VM. An SSH connection refused,
authentication failure or missing `guest_ip`/`ssh_user` is a hard error naming
`--serial` and `hegn vm apply`; hegn never silently drops back to the serial
relay.

### 5.3 Stop

1. Take `hegn.lock`.
2. Resolve the VMM, relay, autosave worker, every member `virtiofsd` from
   `membership.json`, and the profile virtiofsd from `session.json` / pid files.
3. Send `SIGTERM` only when the pid still matches the expected binary/relay
   command, wait up to two seconds, then `SIGKILL` under the same check.
4. Remove run files. The session log and every workspace stay. SSH sessions end
   when the VMM exits.

### 5.4 Snapshot create / restore

Create:

1. Require a running VM and an API socket.
2. Pause the guest.
3. `PUT /api/v1/vm.snapshot` with a `file://` destination inside
   `snapshots_dir/.staging/<name>`.
4. Tighten the files, atomically rename the directory into `snapshots_dir`, and
   record metadata with `SnapshotStore` (the directory holds `config.json`,
   `state.json` and `memory-ranges`). Metadata records the VM, the exported
   membership and the `boot_fingerprint` over membership and boot inputs. The
   boot inputs now include `seed_version` and `network_mode` in addition to
   resources, rootfs, `network_index`, profile presence and `primary`; the
   fingerprint domain is `hegn-snapshot-boot-v3`.
5. Always attempt to resume; if saving metadata fails, remove the snapshot
   directory and resume.

Automatic checkpoints:

1. `start` and `restore` spawn a detached `hegn __autosnapshot --vm <name>`
   worker (`run/autosave.pid`).
2. Every `snapshot_interval_secs` it verifies the API reports `Running`
   (never resuming a deliberately paused VM), creates a snapshot tagged
   `autosave`, and prunes older automatic snapshots beyond `snapshot_keep` for
   that VM.
3. The worker exits when the VMM disappears; `stop` and stale-session cleanup
   terminate it by verified pid.
4. `start` computes the current fingerprint and selects the newest matching
   automatic checkpoint. A mismatch, a missing member, a changed resource, a
   different seed version or network mode, or a snapshot from an older
   fingerprint domain is logged, never restored, and falls back to a cold boot.
   Manual snapshots and legacy snapshots are never selected.

The cold-boot command line adds a host-backed `--rng src=/dev/urandom` device.
The cloud seed installs `hegn-maintenance.service`, which reads the host clock
from the guest RTC and mixes `/dev/hwrng` into the guest CRNG every 15 seconds.
A restored guest therefore corrects its clock and reseeds without NTP.

Restore:

1. Validate the snapshot directory and its `config.json`, `state.json` and
   `memory-ranges` files. Reject a legacy snapshot, an older fingerprint domain
   and a snapshot whose device tree has no control NIC. Refuse a manual restore
   whose fingerprint differs and name the changed fields; there is no
   `--force`.
2. Validate every recorded member path with the same export rules as a boot.
3. Preflight Cloud Hypervisor/virtiofsd before stopping the current VM.
4. Take `hegn.lock`; terminate the old VMM, relay and every filesystem daemon.
   Reserve the control TAP and, when the snapshot recorded guarded networking,
   the data TAP for the same slot.
5. Re-spawn one sandboxed `virtiofsd` per recorded member on the same indexed
   sockets, plus the profile daemon when the snapshot recorded it. Regenerate
   the seed from the recorded membership and network mode, create a new session
   log, then start Cloud Hypervisor with
   `--restore source_url=file://<dir>,resume=true`. Restore repeats
   `--seccomp true` and `--landlock` (with the snapshot-staging and a sysfs rule
   for each recorded TAP) because the restored VMM is a new process; it checks
   both TAP MACs against the snapshot before starting.
6. Start the relay, wait for the API, write the new session record and
   membership manifest; on failure kill the children and remove the new log. On
   success, restart the autosave worker so the next restart can use a newer
   checkpoint.

### 5.5 Project create, import and assignment

Create writes into `projects_root` (default `~/Projects/hegn`), seeds
`AGENTS.md`/`README.md`, initializes git, and registers a `ProjectEntry` in
`<state_dir>/projects.toml`. A repo-local `hegn.toml` is optional and is written
only when presentation settings are edited. With
`--vm` it assigns the entry immediately; with `--solo` (or an accepted prompt)
it creates a one-member VM named after the project and assigns it; otherwise the
project stays unassigned.

Import:

1. Expand a leading `~`, require an existing directory.
2. Canonicalize the source.
3. Refuse `/`, `$HOME`, the projects root itself, an ancestor of the root, a
   path already inside the root, and any path that overlaps hegn state,
   snapshots, VMs, templates, profiles or config.
4. Validate the picker name (`validate_project_name`).
5. Refuse an existing picker entry (including a broken symlink).
6. Create a symlink `projects_root/<name> -> canonical source`.
7. Register the canonical target and apply the same `--vm`/`--solo` selection.

No project file is moved, copied or rewritten. Discovery resolves the symlink
and skips links to `/`, `$HOME`, an ancestor of the projects root, or any path
overlapping hegn state/snapshots/VMs/templates/config, so a manually planted link
cannot export the host filesystem or runtime state through virtiofsd.

`project assign` upserts the entry, sets `vm` and `read_only`, saves the VM's
`primary` when it had none, and validates the whole registry before returning.
Any failure rolls the registry and primary back. `project unassign` clears the
assignment, moves the primary to the next exported member when needed, and keeps
the VM. A running VM is not modified; it warns until `hegn vm apply`.

`project fork` uses `WorkspaceService`: a clone runs `git clone --local` into
`<vms_dir>/<vm>/workspaces/<name>`, verifies the `.git` directory and HEAD, and
registers a managed `clone`. A worktree runs `git worktree add` inside the
parent, verifies it with `git worktree list`, and registers a non-exported
`worktree` that inherits the parent's VM. Deleting a clone removes only the
checked managed path; deleting a worktree uses `git worktree remove`.

### 5.6 Harness profiles and skills

1. `hegn config init` and `hegn doctor --fix` call
   `profile::ProfileStore::write_templates`, which writes one `0600` template per
   agent harness into `profiles_dir` and never overwrites an existing file.
2. On start, `profile::prepare_for_start` loads `profiles/<harness>.toml`,
   validates every package, source, guest target, mode and environment entry,
   and materializes `<vm>/profile/bundle/`.
3. Configuration files are copied fresh on every start. Skills are seeded only
   when their per-VM copy does not exist; later starts keep the copy so
   guest-created skills survive reboots and can be synced back.
4. The bundle carries a tab-separated `manifest`, an `env.sh`, and the copies.
   The host-side mapping (`local.json`) stays outside the exported directory.
5. A second `virtiofsd` exports only `bundle/` at `/run/hegn/profile`. The
   guest's `/usr/local/sbin/hegn-apply-profile` copies config files, points
   each skill target at the per-VM copy, and installs `env.sh` in
   `/etc/profile.d`. The provisioning oneshot runs it on every boot, before its
   install marker check.
6. The provisioner grows the root partition/filesystem, waits for guest DNS,
   retries `apt-get`, and emits `hegn-progress:` JSON lines on the serial
   console. The TUI/`hegn open` tail the session log and render those stages as
   a progress bar with a rough ETA.
7. `hegn skills sync` merges new/changed host files into the existing skill
   copy. `hegn skills sync-back` walks the copy and writes changed/new regular
   files to the configured host source, refusing symlinks and special files; it
   refuses a live VM unless `--force` is given.

### 5.7 Image provisioning

`hegn images download`:

1. Read `kernel_url`/`kernel_sha256` and `rootfs_url`/`rootfs_sha256`.
2. Skip files that already exist unless `--force`.
3. Build a reqwest blocking client with connect/overall timeouts.
4. Stream the response into a private `.<name>.part-<pid>-<nanos>` sibling,
   hashing while writing and enforcing a 2 GiB cap.
5. On a checksum mismatch (or any I/O error), remove the temporary file.
6. On success, fsync, rename over the destination, then set mode `0600`.

`hegn images verify` re-hashes installed files and returns exit code 1 on a
mismatch. `hegn images status` shows path, presence and size.

### 5.8 Doctor

`doctor::run(&config)` performs cheap blocking checks: Cloud Hypervisor,
virtiofsd, `ssh`, `ssh-keygen`, git, lazygit (optional), `/dev/kvm`, Landlock
(required when enabled), kernel image, rootfs image, projects root, logs dir
(optional), backend, editor (optional), plus `images::status`. The network check
requires setup marker version 3, the control TAP family and the control nftables
rules (sets, per-TAP source drops, established accept before the control drop,
forward drop and no control masquerade). For every running VM it verifies
`guest_ip`, `ssh_user` and the presence of the per-VM client key, and points at
`hegn vm apply` or `--serial` when a record predates SSH support. It also reports
four optional legacy checks: `legacy instance dirs`, `legacy snapshots`,
`legacy project keys` and `deprecated config keys`, with a short remediation for
each. They are informational, are rendered with the `!` marker, never change the
exit status, and never delete anything.

A bare `virtiofsd_bin` is resolved through `PATH` and then the distribution
install directories in `config::VIRTIOFSD_FALLBACKS`; the report names the
resolved path next to the version. An explicit `virtiofsd_bin` path is always
used verbatim.

A bare `cloud_hypervisor_bin` accepts both `cloud-hypervisor` and
`cloud-hypervisor-static` (the AUR `cloud-hypervisor-bin` package installs only
the static name), then the absolute fallbacks in
`config::CLOUD_HYPERVISOR_FALLBACKS`. The resolved path is used for spawning and
for `/proc/<pid>/cmdline` verification.

The TUI opens the doctor modal on the first launch when automatic setup is
available or a required check fails, and from `App::fail_start` (VM
start/attach failure). `F3` opens it manually.
`d` triggers a background download through `Effect::DownloadImages`; `r`
re-runs the checks. The worker streams `images::DownloadProgress` values over
the scan channel (throttled to 256 KiB), which `App::set_download_progress`
turns into a bottom-right notification with a progress bar; the notification is
cleared when the worker reports the finished doctor run. `s` asks for explicit
confirmation and then runs
`Effect::SetupEnvironment`: create directories, write the default config,
download/verify images, and call the control/guarded network setup through
`sudo`/`pkexec` with the terminal temporarily restored for the password prompt.
Every step is idempotent and only hegn-owned state is changed. The CLI exposes
the same plan as `hegn doctor --fix`, with `--json` support.

---

## 6. Security model and invariants

These are product requirements, not implementation details.

1. **No host work, ever.** No fallback shell, local PTY, container or host
   command execution for project work. `hegn doctor` and `hegn logs` inspect the
   host, but do not run project code.
2. **Preflight and cleanup.** Do not spawn `virtiofsd`, Cloud Hypervisor or the
   relay until assets and binaries are known good. On mid-start failure, kill
   children and remove run files; the session serial log is kept for diagnosis.
3. **Single lifecycle writer.** `start`, `stop` and snapshot restore hold an
   exclusive `flock` on `run/hegn.lock`.
4. **One sandboxed daemon per export.** Every exported project gets its own
   `virtiofsd`, canonicalized directory and sandbox mode. Multiplicity never
   comes from exporting a parent. Worktrees are not exported separately. A
   project path appears at most once in a VM, and two running VMs may not export
   the same canonical path.
5. **Membership is host-owned.** `<state_dir>/projects.toml` is private,
   `O_NOFOLLOW`, atomic and locked for writers. A repo-local `hegn.toml` cannot
   change membership, read-only status, resources or the tool profile. The
   registry is never inside an exported directory.
6. **Nesting is rejected.** No member may be an ancestor or descendant of
   another, except a `worktree` inside its named parent. That worktree must
   inherit the parent's VM and cannot be independently assigned.
7. **Read-only is enforced twice.** `virtiofsd --readonly` plus a guest `ro`
   mount. It prevents writes, not reads.
8. **Attach is read/verify, not recovery.** It checks the recorded VMM pid and
   binary, then the SSH target or, with `--serial`, the relay and membership
   manifest. It refuses no-session, dead and mismatched cases. Membership drift
   only warns until `hegn vm apply`.
9. **Least privilege on disk.** Run/log dirs `0700`; locks, logs, sessions,
   registry, `vm.toml`, snapshots and images `0600`; `O_NOFOLLOW` where
   untrusted names are opened.
10. **Untrusted output stays untrusted.** Project names, log names and log
    content are sanitized before terminal rendering. `logs show --raw` is an
    explicit unsafe opt-in.
11. **Logs are private but may contain secrets.** No upload/transmission; only
    hegn's own session logs are pruned; the active log is protected.
12. **Direct input is visible.** `NORMAL` / `INSERT` and `INPUT DIRECT` labels
    are mandatory. The project terminal opens in normal mode and forwards
    nothing; insert mode is inspected by `tirith-core`; `F2` and `hegn attach`
    hand the terminal directly to the guest (SSH or serial) as a deliberate
    user action.
13. **Images are pinned, not trusted blindly.** HTTPS URL + SHA-256; unverified
    downloads require `--allow-unverified`; existing files require `--force` to
    replace; temporary files never become visible as final images.
14. **Import never widens the export accidentally.** Symlink only, dangerous
    ancestors/state paths refused, discovery filters dangerous links.
15. **No own guest image.** Kernel and rootfs come from upstream Cloud
    Hypervisor releases; CI only refreshes pins and opens a reviewable PR.
16. **Delete is bounded.** `project delete` removes one registry entry and never
    a VM or a sibling. `vm delete` unassigns ordinary projects and asks first; a
    project's source directory is deleted only with an explicit file policy.
17. **Explicit rootfs images are unique per VM** and checked before boot.
18. **Projects in one VM are mutually visible.** This is documented, not
    hidden. The VM is the trust boundary; read-only does not provide secrecy.
19. **SSH is management-only.** SSH terminates on the guest's control NIC
    (`hegnc<i>`). The control TAP has no default route, DNS, forwarding or
    masquerade, so it cannot reach the host, the local network or the internet.
    All egress still goes through the guarded data TAP, which exists only with
    `network = "guarded"`.
20. **Serial is recovery, not a fallback.** `hegn attach` defaults to SSH;
    `--serial` is an explicit choice. A refused or failed SSH connection is a
    hard error and never silently attaches to the serial relay.
21. **SSH identity is per VM and private.** The client key lives in
    `<vms_dir>/<vm>/ssh/` (`0700`, key `0600`) with a per-VM `known_hosts`;
    host keys are regenerated on the next boot after `hegn vm apply`, and
    `hegn vm rebuild`/`clone` get a fresh client key. sshd accepts public keys
    only, for the configured `ssh_user` only, with agent, TCP and X11
    forwarding disabled.

### 6.1 Threat notes

- A malicious agent inside the guest is constrained by KVM plus the VM's
  `virtiofsd` exports. Every member is readable; read-only members are not
  writable. The guest network is enforced on the host, outside the
  guest's control: nftables accepts only outbound TCP 80/443 and DNS to public
  resolvers, drops all RFC1918/CGNAT/link-local/multicast destinations, drops
  IPv6 and drops every packet from the data TAP addressed to the host. Guest
  root cannot remove host rules. `network = "off"` removes the data device
  entirely;
  only the control TAP remains, and it is accepted solely for established SSH
  replies to the host address.
- The control TAP is a separate management link with no route off the host.
  Packets arriving on a control TAP must come from the slot's guest address;
  only established/related replies to host-initiated SSH are accepted, and
  everything else is dropped. A forward rule drops control traffic before any
  forwarding decision. The guest runs sshd with public-key authentication only,
  `AllowUsers <ssh_user>`, no password or keyboard-interactive login, no agent
  or TCP forwarding and a bounded `MaxSessions`. Guest root can change its own
  sshd, but it cannot give the control TAP a default route or reach the host,
  local network or internet through it. SSH input bypasses `tirith-core` exactly
  like raw serial input; the shell and any `--command` run with the guest user's
  rights inside the VM.
- netfilter evaluates every base chain hooked at `forward`, and a drop in any of
  them wins, so `table inet hegn` can only ever *allow* a packet. A host
  firewall with a routed drop policy (ufw sets `DEFAULT_FORWARD_POLICY="DROP"`,
  firewalld drops unassigned interfaces) would therefore discard exactly the
  traffic hegn accepted, which presents as a guest with no DNS or HTTP. Setup
  exempts the **data** TAP pool from whichever firewall is active (`ufw route
  allow in on hegnN`, a firewalld `FORWARD` direct rule, or an iptables
  `-I FORWARD` accept). The exemption is a blanket per-interface accept *in the
  host firewall*; it cannot widen what a guest reaches, because hegn's own table
  still has to accept the same packet. The control TAPs get no host-firewall
  forward exemption: their input path is accepted by hegn's own
  established/related rule, and their forward path is dropped. Teardown removes
  the data exemptions again, and marker version 3 invalidates guards that lack
  the control rules. A privileged reader (`hegn network setup`, root-run `hegn doctor`)
  also lists the live `inet hegn` table and refuses to trust the marker when the
  rules are absent or partially flushed. `hegn network reload` rewrites only the
  table (including `route_localnet` and the endpoint DNAT set) when the endpoint
  list changes, leaving TAPs and host-firewall exemptions untouched.
- Apart from established replies to host-initiated SSH connections on the
  control TAP, the only guest-to-host exception is the configured
  `local_endpoints` list: those TCP ports are DNATed from the VM's own **data**
  TAP host address to `127.0.0.1` and accepted in the input chain, and
  `route_localnet=1` is set only on the data TAP interfaces. Ports below 1024
  are rejected, no arbitrary loopback address is reachable, and the guest
  cannot influence the rules. Control TAPs never get endpoint DNAT.
- DNS is deliberately limited to public resolvers. DNS tunnelling to an
  attacker-controlled authoritative domain is still possible; a future
  resolver proxy should filter queries. TLS content is not inspected.
- `tirith-core` inspects commands in the TUI; a determined user can use raw
  mode, an SSH session or the serial console. This is visible, not hidden.
- Serial logs can contain terminal escape sequences. The default `logs show`
  path strips them; `--raw` can affect the terminal and is documented as such.
- Imported symlinks are trusted only as far as the host user's filesystem
  permissions. A same-user process that can rewrite `projects_root` can also
  rewrite the picker; hegn filters the most dangerous targets but cannot defend
  against an attacker with the user's own account.

---

## 7. Image pinning and the weekly CI job

hegn intentionally does **not** build, sign or publish its own guest image.
That would add a second supply chain, a package-rebuild cadence and a large
release artifact. Instead:

- `src/vm/images/assets.rs` contains per-architecture constants:
  `KERNEL_VERSION`, `KERNEL_ARTIFACT`, `KERNEL_URL`, `KERNEL_SHA256`,
  `ROOTFS_VERSION`, `ROOTFS_ARTIFACT`, `ROOTFS_URL`, `ROOTFS_SHA256`.
- Defaults use the official Cloud Hypervisor Linux kernel release and the
  newest official Cloud Hypervisor cloud image (qcow2 on x86_64, raw on
  aarch64). The pins make the exact artifacts explicit.
- `config.toml` can override URLs and digests (for a mirror), but URLs must be
  `https://` and non-empty digests must be 64 hex characters.
- `.github/workflows/vm-images.yml` runs **weekly** (`17 4 * * 1`) and on
  `workflow_dispatch`.
- `scripts/update_vm_images.py --arch <arch>` queries the Cloud Hypervisor
  Linux release and the upstream `test_assets.yaml`, selects the kernel and
  cloud image, downloads both, computes SHA-256 from the actual bytes, and
  rewrites only the matching architecture block in `src/vm/images/assets.rs`.
- If the file changed, the workflow force-pushes a `chore/vm-images` branch and
  creates or updates a pull request. A human reviews the new artifacts before
  merging; the normal Rust CI then re-runs tests and clippy.
- `--check` mode fails when the working tree pins are stale, which is useful for
  manual audits.

The workflow needs `contents: write` and `pull-requests: write`; it uses the
repository `GITHUB_TOKEN` and `gh`, not a third-party PR action. It does not
publish releases or cache a hegn-owned image. Because the job downloads the
artifacts to hash them, it needs several GiB of temporary network transfer and
disk per architecture; no file is written outside the process.

### 7.1 Updating pins manually

```bash
python3 scripts/update_vm_images.py --arch x86_64
python3 scripts/update_vm_images.py --arch aarch64
cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings
hegn images verify
```

Review the URL/version movement, especially when the rootfs and kernel versions
diverge. The pinned root filesystem must stay compatible with `rootfs_format`
and `root_device`; the official qcow2 image uses `/dev/vda1`.

---

## 8. TUI architecture

- `src/tui/app/mod.rs` owns all state: projects, VMs, membership, selection,
  focus, modal, VM status, snapshots, terminal scrollback, input mode, toasts
  and footer hit rectangles. Its submodules cover actions, state, modals, keys,
  mouse, config, project, snapshots, launch, doctor, security, provisioning and
  the terminal.
- `src/tui/ui/mod.rs` is a pure renderer, split into `screens/` and `modals/`.
  It returns hit rectangles for footer buttons, project/VM/snapshot rows, and
  the settings modal's rows and `‹ ›` steppers; mouse handling lives in
  `src/tui/app/`.
- `src/config/form/` owns settings-modal navigation, inline editing, validation
  and the save (`state.rs`, `edit.rs`, `persist.rs`, `rows.rs`, `parse.rs`). It
  edits a working copy and only writes on `Ctrl-S` (`config::save_to` for
  globals, `VmStore::save` for `vm.toml`, project presentation for
  `hegn.toml`).
- `src/tui/event_loop.rs` owns the terminal, the 50 ms tick, crossterm events
  and effects. `effects.rs`, `messages.rs` and `jobs.rs` define effects,
  messages and background work.
- Effects that must release or block the terminal (editor, lazygit) or run
  async work (snapshots, image downloads) are returned to the event loop.
- Project scanning and image downloads run on named background threads and
  report through an `mpsc` channel. The UI keeps ticking while a large image
  downloads.
- The terminal guard restores raw mode, alternate screen, cursor and scroll
  region on normal exit and on panic unwinding.

Modals: create VM, edit VM, assign project, fork workspace, detach project,
delete VM, create project, import project, doctor, security prompt, confirm,
delete project, help and error. Every footer button and every keybinding hint
drawn inside a modal has an `ActionId` and is clicked through the same
`handle_action` path as its keybinding. Both invert their colors while
`MouseEventKind::Moved` reports the pointer inside their rectangle. Destructive
confirmations (delete VM, delete project, delete/restore snapshot, allow a
flagged command) accept only their explicit key — `Enter` deliberately leaves
the modal open.

The project delete modal re-reads git status before it opens, shows the project's
branch, sync state and changed-file counts, and states that the VM is not
deleted. `y` unregisters the project and keeps its files (a real directory is
recorded in the persisted hidden-project list; an import symlink is removed);
`f` also deletes the project directory. Deletion removes the registry entry,
drops the project from the in-memory dashboard immediately, and reloads the
persisted state so the next scan does not re-add a kept directory.

The help modal filters its keybinding sections with a case-insensitive query
entered after `/`; section titles, keys and descriptions all match, and every
whitespace-separated term must occur.

Modals are responsive. `centered` grows small dialogs to the available width
and height, the help key/description table and the settings label/value rows
collapse to one column below their narrow thresholds, and overflowing bodies
get a one-column scrollbar. The renderer returns the track rectangle as a
`ScrollHit`; `src/tui/app/` turns wheel events, `j`/`k`, arrows, `PgUp`/`PgDn`, clicks
and drags into offset changes, and the thumb inverts while hovered or dragged.
Long lines are ellipsized by `fit_line` rather than wrapped, so one logical line
is always one row and the scrollbar offset stays exact.

The dashboard is a project picker plus a compact preview. Each project row shows
its VM badge (or `unassigned`). Entering a project does not boot a VM; it opens
the project screen with a folder-style title bar, the project path, VM state and
four numbered sections: `1` VM, `2` snapshots, `3` health, `4` git. The VM
section shows the tool profile, mount path and sibling projects. `v` opens the
VM screen with `1` projects (assign/detach/set primary), `2` snapshots, `3`
tools and `4` health.

The terminal starts as a one-line strip. `i`, `Enter` or a click opens it on the
right at 55%; pressing a section number expands that section on the left and
shrinks the terminal to 30%. All four sections remain visible in that view: the
focused one takes the remaining height and the others collapse to a tab-sized
header plus one content row. Refocusing the terminal restores 55%. `Esc` in
insert mode returns to normal; `Esc` in normal mode returns focus to the section
headers while keeping the pane open. `←` and the clickable `← all projects`
breadcrumb return to the dashboard; `Ctrl-B`/`Ctrl-]` do so from every mode and
the VM keeps running. `F2` releases the TUI and opens an external SSH session
through the real `hegn attach` client instead of a raw in-TUI mode. The
embedded terminal itself remains the serial relay; there are no TUI tabs and no
VT emulator. `F4` edits global config, `F5` edits
`vm.toml`, and `F6` edits the project's `hegn.toml` (launch command, terminal
integration and auto-launch). `Enter` saves and `r` saves and launches
immediately, which is how the `tmux` and `herdr` integrations are selected
without hand-editing TOML.

The VM section is scoped to the open project's VM. Switching projects rebuilds
the member list from the registry (no extra disk scan). A fork is another member
of the same VM, so it appears in the list and is attached with one `Enter`.
Clone workspace commands (`git clone`, `git config`) capture stdout/stderr
instead of inheriting the TUI's terminal; git warnings such as an empty
repository are reported as a warning toast.

The health section reads the host hegn version, VM image readiness, VM creation
date, the newest VM snapshot, the project template, and the last `hegn-health`
line from the VM's serial log. The guest writes that line at boot; the host never
queries the guest from the renderer. The line carries `provision=` and
`mounts=<ok>/<total>` so a failed first-boot provision or a missing project mount
is visible. `hegn doctor` parses the newest line from the five newest session
logs and warns when provisioning failed, a mount is missing, or the report is
older than 24 hours.

VM starts run on a named worker thread (`hegn-start-vm`) and report through the
same channel as project scans. The UI therefore keeps drawing an animated
`STARTING` spinner, the VM note and a toast immediately after Enter;
`ensure_console`, all member `virtiofsd` processes, Cloud Hypervisor boot, and
the optional session branch do not block the event loop. Snapshot restore is
still on the runtime path and is the next candidate for the same treatment.

### 8.1 Theming

All rendering uses the terminal's default foreground/background plus ANSI named
colors (`Cyan`, `Green`, `Yellow`, `Red`, `DarkGray`, `Black` on colored
badges). There is no custom RGB palette and no full-screen background fill, so
light and dark terminal themes both work. `Color::Reset` is the normal text
color. Colored backgrounds are limited to small cells: selection bars, footer
button chips, toast badges and status markers.

### 8.2 Attach bar

The CLI attach view is not a full terminal emulator. It reserves the bottom host
row with a DECSTBM scroll region and paints a reverse-video status line
containing project, harness, VM pid, log and the detach keys. Unlike a one-shot
overlay, the bar is repainted after every burst of guest output and the scroll
region is re-reserved after a terminal reset. DECSTBM sequences from the guest
are clamped so the reserved row can never enter the guest's scrolling region,
which keeps line feeds from scrolling the bar up into the guest output.
`--no-bar` disables the reservation for guest programs that need every row.
For serial attaches the guest tty geometry is fixed by the serial console and
cannot be resized from the host; SSH sessions get a real PTY and resize.

An `OutputGate` sits between the relay socket and the terminal. It holds
incomplete escape sequences and reports whether the stream ends on a UTF-8
character boundary; the bar is only injected at such a boundary so a repaint
can never split a guest sequence or character.

`Ctrl-B` detaches without stopping the VM. `Ctrl-]` is accepted as well, which
matters when hegn runs inside tmux and `Ctrl-B` is consumed as the tmux prefix.

---

## 9. CLI / TUI parity

| Capability | TUI | CLI |
| --- | --- | --- |
| List projects | dashboard | `hegn projects` |
| Create project | `c` | `hegn project create` |
| Import project | `h` / `←` | `hegn project import` |
| Delete project | `d` on the dashboard | `hegn project delete` |
| Restore a kept project | — | `hegn project restore` |
| Open/attach VM | `Enter` (SSH), embedded terminal (serial) | `hegn open`, `hegn attach` (SSH default; `--serial`) |
| Assign project | assign modal | `hegn project assign` |
| Detach project | project/VM section | `hegn project unassign` |
| Start/stop VM | VM screen | `hegn vm start`, `hegn vm stop` |
| Apply membership | VM screen | `hegn vm apply` |
| Choose primary | VM screen | `hegn vm primary` |
| Status | header/sidebar | `hegn vm list`, `hegn vm show` |
| VM settings | `F5` (`vm.toml`) | `hegn vm config` |
| Snapshots | snapshot panel | `hegn snapshot list/create/restore/delete` |
| Logs | — | `hegn logs list/path/show/prune` (`--vm` on list, path and show) |
| Security check | modal + insert-mode input | `hegn check` |
| Fork workspace | fork modal | `hegn project fork` |
| Launch agent/command | `L` (uses project settings) | `hegn launch`, `hegn open --command` |
| Launch in tmux/herdr | `F6` integration → `r` | `hegn launch --integration tmux\|herdr` |
| Project settings | `F6` (`hegn.toml`) | edit `hegn.toml` |
| Doctor | `F3` + automatic on first launch/failure | `hegn doctor` |
| Automatic setup | doctor modal `s` (confirmed) | `hegn doctor --fix` |
| Image download | doctor modal `d` | `hegn images download` |
| Image verification | doctor report | `hegn images verify` |

The TUI is intentionally not required for automation. Anything an operator
needs to recover or audit has a CLI form.

---

## 10. Failure handling

| Failure | Behaviour |
| --- | --- |
| Missing kernel/rootfs | Hard error before spawn; doctor modal/CLI report; `hegn images download` offered |
| Missing Cloud Hypervisor/virtiofsd | Hard error before any child is spawned |
| Missing `/dev/kvm` | Hard error; doctor reports it |
| Landlock unavailable | Hard error; there is no bypass; doctor names the missing LSM |
| VMM keeps the API socket open | Client frames by `Content-Length`/chunked encoding and returns as soon as the response is complete; bodyless `204` ends at the headers |
| VM boot is slow | Worker thread; UI shows an animated `STARTING` spinner and remains responsive |
| Start fails before guest boot | `run/hegn.log` gets the error; the serial log is preserved and `hegn logs show` exposes VMM messages |
| Bad download/checksum | Temporary file removed; no final image written |
| Empty checksum | Refused unless `--allow-unverified` |
| Existing image | Refused unless `--force` |
| API socket missing | `status` reports stopped/unknown; start/attach explain the next step |
| `ssh` or `ssh-keygen` missing | Doctor required check fails; SSH attach fails before spawn with an install hint |
| SSH connection refused or timed out | Hard error naming user/host and suggesting `--serial` or `hegn vm apply`; the VM keeps running |
| SSH permission denied | Hard error; reports the configured `ssh_user` and points at `--serial`/`apply`; no password fallback |
| Host key verification failed | Hard error; the per-VM `known_hosts` is not overwritten silently. `hegn vm apply` or `rebuild` rotates the guest host keys |
| Session record has no `guest_ip`/`ssh_user` | Attach fails with `--serial`/`hegn vm apply` hint; it never guesses a target |
| VM dies during attach | Console or SSH loop ends; status updates; no host fallback |
| Session record write fails | VM and every daemon killed; run files and new log removed |
| Two lifecycle calls | Second fails on `flock` with a retry hint |
| VM has no members | Hard error before spawn; `hegn project assign` hint |
| Member directory missing | Hard error listing every missing member; no partial boot |
| Duplicate or nested members | Registry validation error before spawn |
| Two running VMs export the same path | Boot fails and names both VMs |
| More than `max_projects_per_vm` | Validation error before spawn |
| One member `virtiofsd` fails | Kill all member daemons, VMM and relay; keep the serial log |
| Membership changed while running | Attach warns; `hegn vm apply` restarts |
| Auto-resume fingerprint mismatch | Skip checkpoint, cold boot, log the reason |
| Snapshot from an older domain or without the control NIC | Not resumed; manual restore is refused; the VM cold boots |
| Manual restore fingerprint mismatch | Refuse and name the changed fields; no `--force` |
| Legacy v1 instance or snapshot | Ignored, reported by doctor, never deleted |
| Shared explicit rootfs | Boot fails for the second VM |
| Import target unsafe | Form/CLI error; no symlink created |
| Log symlink | Listing skips it; resolution/reads refuse it |
| Snapshot restore fails | New Cloud Hypervisor process dies; cleanup; old VM already stopped by design (preflight prevents config errors) |
| Terminal too small for attach bar | Refuse to attach before raw mode; suggest `--no-bar`/resize |

---

## 11. Testing strategy

- All modules are unit-tested from the library crate (`src/**/*.rs` `#[cfg(test)]`).
- Cloud Hypervisor API tests use a local Unix socket server; no `/dev/kvm`
  needed.
- Console and relay tests use Unix socket pairs with a fake session record whose
  pid belongs to the test binary.
- Cloud-init seed tests open the generated FAT image and verify its label and
  files without a VM.
- Lifecycle tests assert preflight ordering and cleanup using missing binaries.
- Registry tests cover assignment uniqueness, duplicate paths, nesting rejection,
  worktree-parent rules, managed clone paths, hidden projects, atomic writes and
  lock behaviour.
- Workspace tests cover clone paths and HEAD verification, worktree containment
  and `git worktree list` checks, and deletion boundaries.
- Membership tests cover deterministic ordering, slug dedupe, fingerprint
  stability and sensitivity, and cross-VM duplicate export refusal.
- Cloud-init tests cover N mounts, per-member read-only, the `/workspace`
  symlink, injection-safe names, the control-only netplan, the sshd drop-in,
  authorized-key path, root login under hardening and host-key regeneration.
- SSH tests cover the exact OpenSSH argv, key permissions, a missing binary,
  a missing guest IP, the readiness probe and exit-status propagation.
- Snapshot tests cover mismatch skipping, manual refusal, legacy handling and
  fingerprint sensitivity to `seed_version` and `network_mode`.
- Log tests cover naming, permissions, symlink refusal, tail semantics, prune
  protection and foreign-file preservation.
- Import tests cover symlink creation, discovery and dangerous targets.
- Doctor tests cover required vs optional failures.
- Image tests cover checksum success/mismatch, no-overwrite, unverified opt-in
  and atomic temporary cleanup without network access.
- UI tests render with ratatui's `TestBackend` and assert text/footer hits.
- CI runs `cargo fmt --check`, clippy with warnings denied, the full test suite,
  and a release build on Rust 1.99.1.

The weekly image workflow is the only job that performs network downloads; it
does not run the test suite. The normal CI must stay offline/reproducible and
must not depend on upstream hosting availability.

---

## 12. Operations runbook

### First install

```bash
hegn config init
hegn images download
hegn doctor
hegn vm create dev --harness opencode
hegn project create first-project --vm dev
hegn open first-project
```

### Add an existing repository

```bash
hegn vm create dev --harness opencode
hegn project import ~/code/existing --name existing --vm dev
hegn open existing
```

or press `h`/`←` in the project list. The original directory is untouched.

### Inspect a past session

```bash
hegn logs
hegn logs list --vm dev
hegn logs show my-project --lines 200
hegn logs path my-project
```

### Recover disk space

```bash
hegn logs prune --keep 20
hegn snapshot list
hegn snapshot delete <name>
hegn vm delete old-vm --yes --delete-files
```

### Audit image pins

```bash
hegn images status
hegn images verify
python3 scripts/update_vm_images.py --arch x86_64 --check
```

### Detach vs stop

- Closing an SSH session (or `exit` in the guest) ends only that PTY;
  `hegn vm stop <vm>` terminates every SSH session, the VMM, relay, every member
  `virtiofsd` and the autosave worker; logs and workspaces remain.
- `Ctrl-B` in a serial attach (`hegn attach --serial`/`hegn open --serial`) only
  detaches. `--no-bar` is serial-only.
- If SSH fails, read the printed hint: `hegn attach <vm> --serial` reaches the
  boot/recovery console; `hegn vm apply <vm>` applies a new seed and rotates the
  guest host keys.

---

## 13. Decisions and trade-offs

### ADR-001: No hegn-owned guest image

**Decision:** consume upstream Cloud Hypervisor CI artifacts; pin URL + SHA-256;
refresh pins weekly via CI PR.

**Why:** avoids maintaining a rootfs build/signing pipeline and a large release
artifact, while still making updates reviewable. Users who need extra packages
can build their own rootfs and point `rootfs_image`/`rootfs_*` at it.

**Consequence:** the upstream rootfs stays signed upstream; hegn never rebuilds
it. Harness binaries are installed *inside* the guest on first boot by the
NoCloud provisioner (`src/agent/provision/`): apt packages plus a static per-preset
installer, retried by a oneshot until `/var/lib/hegn/provisioned` exists. The
same oneshot re-applies the user's `profiles/<harness>.toml` payload on every
boot; profile skills live in a per-VM copy (`src/agent/profile/`), never in a
host mount. The host image is never mutated, and a pre-baked custom rootfs
remains an explicit opt-in.

### ADR-002: Import by symlink, never copy

**Decision:** `project import` creates a symlink inside `projects_root`.

**Why:** copying a large repository is slow, duplicates secrets and creates two
diverging working trees. Moving user data is worse. virtiofsd canonicalizes the
symlink target, so isolation is still exactly one directory.

**Consequence:** the imported project depends on its original path remaining
available. Broken links are ignored by discovery.

### ADR-003: Default projects root `~/Projects/hegn`

**Decision:** new installations put projects under `~/Projects/hegn`.

**Why:** keeps hegn-managed projects together and avoids mixing them with
unrelated directories in `~/Projects`.

**Consequence:** existing `config.toml` files keep their configured root; users
who want the new default must update `projects_root` or rerun `hegn config init`
after moving the file.

### ADR-004: Persistent log via a detached relay

**Decision:** a detached relay reads Cloud Hypervisor's serial socket and
appends to `state/logs/session-*.log`; attach clients connect to a second relay
socket.

**Why:** Cloud Hypervisor's serial socket accepts a single client. The relay
keeps the log complete while detached and replays its tail to the next attach
without a second writer.

**Consequence:** one small hegn process per VM. It has no project access, no
network and only the two socket paths plus the log.

### ADR-005: Attach bar is an overlay, not a terminal emulator

**Decision:** reserve one host row with DECSTBM and paint the status line.

**Why:** implementing a complete VT emulator plus a guest-side resize protocol
is disproportionate; the legacy serial console cannot transmit window-size
changes to the guest tty anyway.

**Consequence:** full-screen guests see one fewer usable row (or can be attached
with `--no-bar`); cursor save/restore and SGR state can interact with the bar.
The overlay repaints after each output burst rather than relying on a one-shot
paint, clamps guest DECSTBM sequences so the reserved row stays outside their
scroll region, and re-reserves the region after RIS/DECSTR. The TUI remains the
modal, line-intercepted view: `i`/`a` enters insert mode and `Esc` returns to
normal mode. The bar belongs to the serial path only; the default SSH attach
does not use it, gets a real PTY and never needs a reserved row.

### ADR-006: Multiple projects per VM

**Decision:** a VM hosts many projects. Each project is exported by its own
sandboxed `virtiofsd` and mounted at `/projects/<slug>`; `/workspace` is a
guest-side symlink to the VM's primary project. Membership changes require a
restart.

**Why:** toolchains, the harness install and manual package installs are
expensive to duplicate per project. Multiplicity must not be bought by
exporting a parent directory: one wide export would give every agent access to
every sibling project and lose per-project read-only mode.

**Consequence:** a VM can run N `virtiofsd` processes (bounded by
`max_projects_per_vm`). Cloud-init renders N mounts, the VMM gets N `--fs`
devices, snapshots must re-spawn all recorded daemons, and guardrails treat
every `/projects/*` root as workspace. Projects in one VM share a guest user
and can read each other's files: the VM, not the project, becomes the trust
boundary. Read-only mode prevents writes, not reads. See
[`design/multi-project-vms.md`](design/multi-project-vms.md).

### ADR-007: Project-first membership in private state

**Decision:** the project remains the primary user object. A VM is a named
tool profile and runtime that projects opt into. The project → VM assignment,
read-only flag and clone lineage live in a private registry under `state_dir`,
not in the exported project directory. A repo-local `hegn.toml` may only hold
presentation preferences (`launch`, `integration`, `auto_start`).

**Why:** the registry decides which host directories are exported and whether
they are writable. A file inside the project is writable by the guest through
`virtiofsd`; letting it control membership would let an agent widen its own
export. Keeping membership private is the same principle as host-owned
nftables rules and TAP reservations.

**Consequence:** the TUI picker stays the home screen and shows a VM badge;
`hegn vm` and `hegn project assign` become the control surface. The registry
needs atomic writes and a lock, and unassigned projects need an explicit
create-or-choose-VM flow.

### ADR-008: Clones by default, worktrees opt-in

**Decision:** parallel workspaces default to self-contained `git clone`
members of the same VM. A nested git worktree is supported as an explicit
`--mode worktree` fork only when it lives inside its parent project's exported
tree.

**Why:** a worktree's `.git` points at `<main>/.git/worktrees/<name>`, outside
the single exported directory; the guest cannot resolve it, and exporting the
main repository to fix that lets one agent mutate another's refs, hooks and
object store. Clones are self-contained and can be read-only, detached or
deleted independently.

**Consequence:** clones consume a checkout and an object store but no extra
rootfs or tool install. Worktrees share mutable git state and are documented
as such; hegn never deletes a worktree implicitly.

### ADR-009: Hard cut to v2

**Decision:** v2 reads a new `vms_dir` and private project registry. Legacy
`instances_dir` entries and v1 snapshots are ignored, reported by
`hegn doctor`, and never deleted automatically.

**Why:** the one-project-per-VM fields (`project`, `workspace`, `shared_tag`,
`guest_mount`, `read_only_workspace`, `ephemeral`) have no faithful v2
equivalent without inventing a migration policy for clones, snapshots and
running sessions. Maintaining both execution paths would double the lifecycle,
snapshot and security surface. Keeping only one path also keeps the security
invariants auditable.

**Consequence:** upgrading is a deliberate recreate. Global `config.toml`
still accepts `instances_dir`, `sandboxes_dir`, `shared_tag` and `guest_mount` as
deprecated aliases with warnings, so startup does not fail; logs remain readable.

### ADR-010: Snapshot boot fingerprint

**Decision:** every snapshot records the VM name, an ordered membership
manifest and a `boot_fingerprint` over the VMM-affecting configuration
(members, tags, mounts, read-only flags, vCPUs, memory, rootfs, network slot and
mode, seed version, profile presence). Automatic resume skips a mismatched
checkpoint; manual restore refuses one.

**Why:** Cloud Hypervisor restore uses the device tree embedded in the
snapshot. Adding or removing a virtio-fs share after the snapshot cannot be
reflected by the restore argv, so a "successful" restore would silently leave
new projects unmounted or removed projects still exported.

**Consequence:** assigning or detaching a project, or changing VM resources,
makes older snapshots non-resumable and requires a cold boot or
`hegn vm apply`. Legacy snapshots without a fingerprint are listed as
non-restorable.

### ADR-011: SSH over the control TAP instead of a vsock broker

**Decision:** `hegn attach` opens OpenSSH to the guest's sshd at
`<ssh_user>@10.214.0.<4i+2>` over a per-VM control TAP (`hegnc<i>`). The serial
console stays available as the boot, provisioning and recovery channel with
`--serial`. There is no vsock session broker, no PTY proxy and no PTY forwarding
through the serial relay.

**Why:** SSH gives every session a real guest PTY with resize, so several
terminals, tmux panes or full-screen TUIs can run in parallel without sharing
one serial line; it also replaces the fragile "type into a single serial shell"
model with a well-understood, audited transport. A vsock broker would need a
guest agent, a host-side protocol, PTY allocation and resize negotiation, and
its own security review. The control TAP keeps the existing egress guarantee:
it has no default route, DNS, forwarding or masquerade, so SSH can never become
a second path to the host, the local network or the internet.

**Consequence:** hosting now requires `ssh` and `ssh-keygen`; `network = "off"`
still needs the root-owned network setup and marker version 3 for the control
TAP. Each VM owns a private key and `known_hosts`, and `apply`/`rebuild` must
rotate the guest host keys. Existing VMs need `hegn vm apply` before SSH attach
and snapshots from before seed version 13 / the v3 fingerprint domain cold boot.
SSH sessions bypass `tirith-core`, exactly like raw serial input, and the TUI
keeps its embedded terminal on the serial relay rather than embedding a VT
emulator.

---

## 14. Known limitations and future work

- The network guard is an allow-list, not an inspecting proxy: it does not
  decrypt TLS or filter DNS answers. Resolver-level filtering and an optional
  CONNECT proxy are future work.
- The legacy serial console has no guest tty resize channel. SSH sessions do
  get a real guest PTY and resize with the host terminal.
- Attach input (SSH and serial) bypasses `tirith-core` by design and is
  labelled; a guarded raw proxy would require a full terminal emulator.
- Per-session logs are not truncated while the relay owns the file; retention
  is by count and explicit `logs prune`.
- Snapshot restore intentionally stops the current VM after preflight; there is
  no live migration.
- Cloud Hypervisor snapshots embed absolute device and socket paths (rootfs,
  serial, virtio-fs). Restore assumes the same `state_dir`/`snapshots_dir`;
  moving them fails closed.
- The upstream rootfs is generic; harness-specific package installation is
  recorded but not baked into the downloaded image.
- The default rootfs is writable and reused between sessions; guest changes
  persist and are shared by every project in that VM. Use a per-VM custom rootfs
  when projects need different tool sets.
- Several SSH sessions per VM run in parallel, each with its own guest PTY.
  There is no hegn-side session multiplexer, PTY proxy or SSH status bar, and the
  embedded TUI view remains the serial console. No vsock session broker is
  planned (ADR-011). Guest `tmux` still helps if a session must survive a
  disconnect.
- Membership changes require a restart; there is no live virtio-fs hot-plug.
- Tool changes after first boot are manual in the guest or require
  `hegn vm rebuild`; there is no guest manifest reconciler.
- Projects in one VM can read each other's files; use separate VMs for a hard
  boundary.
- CI image refresh opens a PR rather than auto-merging, so updates still need a
  human review.

---

## 15. Repository map

```text
src/
  main.rs        thin binary / exit codes
  lib.rs         module tree and crate documentation
  shared/        dependency-free fs/process/path/text helpers
  cli/           clap commands, per-command modules, output and resolution
  config/        schema, defaults, env overrides, validation; form/ editor model
  project/       discovery, create, import, templates, git helpers
                 registry.rs private membership and validation
                 workspace.rs clone/worktree forks
  security/      tirith-core + hegn workspace guardrails
  network/       control + guarded TAP pools, nftables rules and reservations
  vm/            config/ vm.toml model and store
                 manager/ lifecycle, boot plan, membership, lock, session
                 registry.rs VM listing and lookup
                 hypervisor/ REST client
                 cloud_init/ NoCloud seed generation
                 images/ upstream pins, downloads, SHA-256
                 rootfs/ sparse raw/qcow2 growth
                 virtiofs/ hardened virtiofsd process construction
  agent/         launch/ named commands + integrations
                 profile/ per-harness profiles and skill sync
                 provision/ harness preset installers and first-boot script
  session/       ssh/ per-VM identity, known_hosts, OpenSSH client, title
                 relay/ detached serial relay
                 console/ serial transport and attach
                 attach_bar/ status bar overlay + output gate
                 logs/ persistent logs, tail, prune
                 terminal/ ANSI-aware scrollback
                 snapshot/ snapshot metadata store
                 state/ recent and hidden project state
  diagnostics/   doctor/ shared host checks
                 benchmark/ measured host cost and disk usage
  tui/           terminal loop and effects
                 app/ TUI state machine
                 ui/ ratatui renderer
scripts/
  update_vm_images.py  refresh upstream pins (CI)
.github/workflows/
  rust.yml          fmt, clippy, tests, release build
  vm-images.yml     weekly upstream pin refresh PR
  release.yml       tagged AppImage release
  aur-publish.yml   tagged AUR publish
vendor/
  tirith-core/      patched newest upstream release (RustSec fixes)
docs/
  architecture.md   this document
  projects-and-vms.md  projects, VMs and workspaces
  migration-v2.md   v1 → v2 hard-cut notes
  llm-setup.md      install guide written for LLM agents
  resources.md      host cost, overhead and storage
README.md           user-facing documentation
AGENTS.md           repository working agreement
```
