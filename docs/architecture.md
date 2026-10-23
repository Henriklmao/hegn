# hegn architecture

This document describes how `hegn` is put together, which invariants the
implementation must preserve, and how the pieces fail. It is the detailed
companion to [`../project.md`](../project.md) (specification) and
[`../README.md`](../README.md) (user documentation).

`hegn` is a Linux-only Rust application that runs coding agents inside
Firecracker MicroVMs. The host process never runs project work itself. Every
terminal, shell and agent session is attached to a real Firecracker guest
serial console.

---

## 1. Design goals

1. **Hardware isolation first.** A missing VM is a hard error. There is no
   host shell, local PTY, container or "temporary" fallback anywhere in the
   codebase.
2. **Fail closed.** Binaries, images and the project directory are checked
   before any child process is spawned. Partial failures are cleaned up rather
   than left running.
3. **No silent bypasses.** Input paths that skip `tirith-core` are visibly
   labelled (`RAW`, `INPUT DIRECT`). Unverified image downloads require an
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
│    ├── project.rs  ── ~/Projects/hegn (picker + import symlinks)          │
│    ├── sandbox.rs  ── lifecycle, lock, session record                     │
│    │     ├── firecracker.rs ── REST over unix socket                      │
│    │     ├── virtiofs.rs    ── virtiofsd argv/process                     │
│    │     ├── console.rs     ── serial FIFO + persistent serial log        │
│    │     ├── snapshot.rs    ── snapshot metadata                          │
│    │     └── logs.rs        ── private logs, hard link, pruning           │
│    ├── images.rs ── upstream kernel/rootfs download + SHA-256             │
│    └── doctor.rs ── shared prerequisite checks                            │
│                                                                            │
│  virtiofsd (sandboxed, one project) ◄──── shared project directory         │
│  Firecracker VMM ── /dev/kvm, API unix socket, serial FIFO/log             │
│        └── guest ttyS0 ── agent / shell                                   │
└────────────────────────────────────────────────────────────────────────────┘
```

The guest sees exactly one host directory through virtiofsd. The host sees
control sockets, logs and metadata. There is no network port, no shared
namespace and no second mount by default.

---

## 3. Runtime topology

### 3.1 Processes

A running session consists of:

| Process | Purpose | Lifetime |
| --- | --- | --- |
| `hegn` CLI/TUI | UI, orchestration, guardrails, API calls | Foreground; may exit while VM continues |
| `virtiofsd` | Exports exactly `session.project` under `--sandbox=namespace` (or `chroot`) | Until `sandbox stop` / restore |
| `firecracker` | KVM MicroVM | Until `sandbox stop` / restore / guest exit |

The CLI deliberately lets `virtiofsd` and Firecracker outlive itself. It
forgets the `Child` handles only after the session record is written; on any
error before that point it kills both children and removes the run files.

### 3.2 Run directory

`SandboxPaths` derives everything from `state_dir/run`:

```text
<state>/run/
  firecracker.sock   Firecracker REST API socket
  virtiofs.sock      virtiofsd vhost-user socket
  serial.in          FIFO: host input -> guest ttyS0
  serial.out         hard link to the session log (read side)
  firecracker.pid    last known VMM pid
  virtiofs.pid       last known virtiofsd pid
  session.json       SessionRecord
  firecracker.log    VMM stderr
  hegn.lock          flock for lifecycle operations
```

`run/` is created with mode `0700`. `serial.in` is a FIFO with mode `0600`.
`serial.out` is a hard link to the persistent log, not a copy.

### 3.3 Persistent log

```text
<state>/logs/
  session-<timestamp>-<project>-<pid>.log
```

The log is created before Firecracker starts, mode `0600`, with
`O_NOFOLLOW | O_CREAT | O_EXCL`. Firecracker's stdout is the log file itself.
The run directory exposes the same inode as `serial.out`, so attaches can read
the current output from offset 0. `sandbox stop` removes only the run link;
the log survives for `hegn logs`.

Logs may contain secrets printed by an agent. They are never uploaded, and
`hegn logs show` strips terminal control and bidi-override characters unless
`--raw` is passed. Pruning deletes only hegn's own `session-*.log` files and
never the log of the running session.

### 3.4 Data directories

| Path | Contents | Permissions |
| --- | --- | --- |
| `~/.config/hegn/config.toml` | User config | user default |
| `~/.config/hegn/templates/` | `AGENTS.md` templates | user default |
| `~/.local/state/hegn/run/` | Sockets, FIFOs, session record, lock | `0700` |
| `~/.local/state/hegn/logs/` | Persistent serial logs | `0700`, files `0600` |
| `~/.local/state/hegn/state.json` | Recent projects / last project | user default |
| `~/.local/share/hegn/snapshots/` | Snapshot metadata + memory/state files | `0700`, files `0600` |
| `~/.local/share/hegn/images/` | Kernel and rootfs | files `0600` |
| `~/Projects/hegn/` | Projects and import symlinks | user default |

All state paths can be overridden in `config.toml` or by environment variables
(see the README).

---

## 4. Modules

| Module | Responsibility | Notable invariants |
| --- | --- | --- |
| `cli.rs` | Clap command surface, JSON/text output, exit codes | Every TUI operation has a CLI counterpart; no fallback paths |
| `config.rs` | Schema, defaults, env overrides, validation, XDG paths, tool lookup | Unknown keys rejected; SHA-256 and HTTPS URL validation; log dir `0700`; `virtiofsd` resolved via `PATH` then distribution dirs |
| `project.rs` | Discovery, fuzzy filter, git summaries, create, import, checkpoints | Import only creates a symlink; dangerous targets rejected; discovery skips links to `/`, `$HOME` and root ancestors |
| `security.rs` | `tirith-core` integration and hegn workspace rules | Empty commands allowed; off mode still keeps destructive-operation guard |
| `firecracker.rs` | Typed REST client over the API socket | Bounded request/response sizes; errors preserve context |
| `virtiofs.rs` | virtiofsd argv/process construction | Shared dir canonicalized; always sandboxed; sockets scoped to run dir |
| `sandbox.rs` | VM lifecycle, lock, session record, status, snapshots | Exclusive flock for start/stop/restore; preflight before spawn; cleanup on every error path |
| `snapshot.rs` | Snapshot metadata store | Sanitized names; private dirs/files; atomic-ish metadata writes |
| `images.rs` | Upstream asset pins, streaming download, SHA-256 install/verify | HTTPS only by config validation; checksum before rename; no overwrite without `--force` |
| `doctor.rs` | Shared host checks for CLI and TUI | Required vs optional checks; missing images are downloadable |
| `console.rs` | Serial transport, tmux-style attach, status bar | Never pretends a VM exists; bar content sanitized; `--no-bar` for full-screen guests |
| `logs.rs` | Log naming, listing, resolve, tail, prune | `O_NOFOLLOW`, regular files only, no foreign files deleted, active log protected |
| `terminal.rs` | ANSI-aware scrollback | Strips CSI/OSC; caps line bytes and scrollback |
| `state.rs` | Recent-project persistence | Atomic tmp+rename |
| `app.rs` | TUI state machine, modals, key/mouse handling, effects | Pure state transitions; side effects returned as `Effect` |
| `ui.rs` | Ratatui rendering | Pure renderer; footer hit rectangles; all modals testable with `TestBackend` |
| `tui.rs` | Terminal lifecycle, event loop, background jobs | Restores terminal on every exit path; project scan and image download off the UI thread |

---

## 5. Lifecycle flows

### 5.1 Boot (`hegn open`, `hegn sandbox start`, TUI open)

1. Load and validate config.
2. Preflight: project exists, kernel/rootfs exist, Firecracker and virtiofsd
   binaries resolve, KVM available.
3. Take the exclusive `hegn.lock`. A second lifecycle operation fails fast.
4. If a stale session exists, clean up its run files and any verified children.
5. Create the session log (`LogStore::create_session`); prune old logs
   best-effort.
6. Spawn `virtiofsd` with the canonical project path and wait for its socket.
7. Create the serial FIFO, link `serial.out` to the log, spawn Firecracker with
   the FIFO as stdin, the log as stdout and `firecracker.log` as stderr.
8. Wait for the API socket, then `PUT /machine-config`, `/boot-source`,
   `/drives`, `/vhost-user-fs`, and finally `POST /actions` with
   `InstanceStart`.
9. Write `firecracker.pid` and `session.json` atomically enough for this
   single-writer design.
10. Release the lock. The children keep running.

On any failure the VMM and daemon are killed, run files and the half-created
log are removed, and the error is returned. A failure to record the session
tears the VM down instead of leaking an unmanageable process.

### 5.2 Attach (`hegn attach`, `hegn a`, `hegn open`)

1. Read `session.json`; fail if missing.
2. Verify the recorded pid is alive **and** still runs the configured
   Firecracker binary (`/proc/<pid>/cmdline`).
3. If a project argument was given, resolve it and compare canonical paths.
4. Open `serial.in` write-only/non-blocking and `serial.out` read-only.
5. Enter the alternate screen, reserve the bottom line with DECSTBM, replay the
   log and forward input.
6. `Ctrl-B` detaches; the VM keeps running. `--no-bar` uses the full terminal
   for full-screen guest programs.

Attach never boots, restarts or repairs a VM.

### 5.3 Stop

1. Take `hegn.lock`.
2. Resolve pids from `session.json` / pid files.
3. Send `SIGTERM` only when the pid still matches the expected binary, wait up
   to two seconds, then `SIGKILL` under the same check.
4. Remove run files. The session log stays.

### 5.4 Snapshot create / restore

Create:

1. Require a running VM and an API socket.
2. Pause the guest.
3. `PUT /snapshot/create` with a full snapshot.
4. Record metadata with `SnapshotStore`.
5. Always attempt to resume; if saving metadata fails, remove the snapshot
   directory and resume.

Restore:

1. Validate snapshot state/memory files and project.
2. Preflight Firecracker/virtiofsd before stopping the current VM.
3. Take `hegn.lock`; terminate the old VM/daemon.
4. Start a fresh virtiofsd and Firecracker, create a new session log, then
   `PUT /snapshot/load`.
5. Write the new session record; on failure kill both children and remove the
   new log.

### 5.5 Project create / import

Create writes into `projects_root` (default `~/Projects/hegn`), seeds
`AGENTS.md`/`README.md`, initializes git and records `.hegn.toml`.

Import:

1. Expand a leading `~`, require an existing directory.
2. Canonicalize the source.
3. Refuse `/`, `$HOME`, the projects root itself, an ancestor of the root, a
   path already inside the root, and any path that overlaps hegn state,
   snapshots, templates or config.
4. Validate the picker name (`validate_project_name`).
5. Refuse an existing picker entry (including a broken symlink).
6. Create a symlink `projects_root/<name> -> canonical source`.

No project file is moved, copied or rewritten. Discovery resolves the symlink
and skips links to `/`, `$HOME`, an ancestor of the projects root, or any path
overlapping hegn state/snapshots/templates/config, so a manually planted link
cannot export the host filesystem or runtime state through virtiofsd.

### 5.6 Image provisioning

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

### 5.7 Doctor

`doctor::run(&config)` performs cheap blocking checks: Firecracker, virtiofsd,
git, lazygit (optional), `/dev/kvm`, kernel image, rootfs image, projects root,
logs dir (optional), backend, editor (optional), plus `images::status`.

A bare `virtiofsd_bin` is resolved through `PATH` and then the distribution
install directories in `config::VIRTIOFSD_FALLBACKS`; the report names the
resolved path next to the version. An explicit `virtiofsd_bin` path is always
used verbatim.

The TUI opens the doctor modal automatically from `App::fail_start` (VM
start/attach failure). `F3` opens it manually. `d` triggers a background
download through `Effect::DownloadImages`; `r` re-runs the checks. The CLI
prints the same report, with `--json` support.

---

## 6. Security model and invariants

These are product requirements, not implementation details.

1. **No host work, ever.** No fallback shell, local PTY, container or host
   command execution for project work. `hegn doctor` and `hegn logs` inspect the
   host, but do not run project code.
2. **Preflight and cleanup.** Do not spawn `virtiofsd` or Firecracker until
   assets and binaries are known good. On mid-start failure, kill children and
   remove run files and the half-created log.
3. **Single lifecycle writer.** `start`, `stop` and snapshot restore hold an
   exclusive `flock` on `run/hegn.lock`.
4. **One project per VM.** Only `session.project` is exported. virtiofsd
   canonicalizes the shared directory and always uses a sandbox mode.
5. **Attach is read/verify, not recovery.** It checks the recorded pid and
   binary and refuses no-session/dead/mismatched cases.
6. **Least privilege on disk.** Run/log dirs `0700`; locks, logs, sessions,
   snapshots and images `0600`; `O_NOFOLLOW` where untrusted names are opened.
7. **Untrusted output stays untrusted.** Project names, log names and log
   content are sanitized before terminal rendering. `logs show --raw` is an
   explicit unsafe opt-in.
8. **Logs are private but may contain secrets.** No upload/transmission; only
   hegn's own session logs are pruned; the active log is protected.
9. **Direct input is visible.** `RAW` / `INPUT DIRECT` labels are mandatory.
   `tirith-core` interception is the TUI default; raw passthrough is a deliberate
   user action.
10. **Images are pinned, not trusted blindly.** HTTPS URL + SHA-256; unverified
    downloads require `--allow-unverified`; existing files require `--force` to
    replace; temporary files never become visible as final images.
11. **Import never widens the export accidentally.** Symlink only, dangerous
    ancestors/state paths refused, discovery filters dangerous links.
12. **No own guest image.** Kernel and rootfs come from upstream Firecracker CI;
    CI only refreshes pins and opens a reviewable PR.

### 6.1 Threat notes

- A malicious agent inside the guest is constrained by KVM plus the single
  virtiofs export. The host firewall/network is not managed by hegn; configure
  guest networking deliberately (or leave it absent).
- `tirith-core` inspects commands in the TUI; a determined user can use raw
  mode or attach. This is visible, not hidden.
- Serial logs can contain terminal escape sequences. The default `logs show`
  path strips them; `--raw` can affect the terminal and is documented as such.
- Imported symlinks are trusted only as far as the host user's filesystem
  permissions. A same-user process that can rewrite `projects_root` can also
  rewrite the picker; hegn filters the most dangerous targets but cannot defend
  against an attacker with the user's own account.

---

## 7. Image pinning and the daily CI job

hegn intentionally does **not** build, sign or publish its own guest image.
That would add a second supply chain, a package-rebuild cadence and a large
release artifact. Instead:

- `src/images.rs` contains per-architecture constants:
  `KERNEL_VERSION`, `KERNEL_ARTIFACT`, `KERNEL_URL`, `KERNEL_SHA256`,
  `ROOTFS_VERSION`, `ROOTFS_ARTIFACT`, `ROOTFS_URL`, `ROOTFS_SHA256`.
- Defaults currently use the newest upstream Firecracker CI `vmlinux-6.1.*`
  kernel and the newest upstream `ubuntu-*.ext4` rootfs. The two may come from
  different Firecracker CI versions; the pins make that explicit.
- `config.toml` can override URLs and digests (for a mirror), but URLs must be
  `https://` and non-empty digests must be 64 hex characters.
- `.github/workflows/vm-images.yml` runs **daily** (`17 4 * * *`) and on
  `workflow_dispatch`.
- `scripts/update_vm_images.py --arch <arch>` lists the S3 bucket, selects the
  newest kernel and the newest ext4 rootfs, downloads both, computes SHA-256
  from the actual bytes, and rewrites only the matching architecture block in
  `src/images.rs`.
- If the file changed, the workflow force-pushes a `chore/vm-images` branch and
  creates or updates a pull request. A human reviews the new artifacts before
  merging; the normal Rust CI then re-runs tests and clippy.
- `--check` mode fails when the working tree pins are stale, which is useful for
  manual audits.

The workflow needs `contents: write` and `pull-requests: write`; it uses the
repository `GITHUB_TOKEN` and `gh`, not a third-party PR action. It does not
publish releases or cache a hegn-owned image. Because the job downloads the
artifacts to hash them, it needs roughly 350 MiB of temporary network transfer
per architecture; no file is written outside the process.

### 7.1 Updating pins manually

```bash
python3 scripts/update_vm_images.py --arch x86_64
python3 scripts/update_vm_images.py --arch aarch64
cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings
hegn images verify
```

Review the URL/version movement, especially when the rootfs and kernel versions
diverge. The rootfs must remain an ext4 image; Firecracker CI's newer squashfs
artifacts are not used by the current boot configuration.

---

## 8. TUI architecture

- `app.rs` owns all state: projects, selection, focus, modal, sandbox status,
  snapshots, terminal scrollback, input mode, toasts and footer hit rectangles.
- `ui.rs` is a pure renderer. It returns hit rectangles for footer buttons and
  project/snapshot rows; mouse handling lives in `app.rs`.
- `tui.rs` owns the terminal, the 50 ms tick, crossterm events and effects.
- Effects that must release or block the terminal (editor, lazygit) or run
  async work (snapshots, image downloads) are returned to `tui.rs`.
- Project scanning and image downloads run on named background threads and
  report through an `mpsc` channel. The UI keeps ticking while a large image
  downloads.
- The terminal guard restores raw mode, alternate screen, cursor and scroll
  region on normal exit and on panic unwinding.

Modals: create project, import project, doctor, security prompt, confirm,
help and error. Every footer button has an `ActionId` and is clicked through the
same `handle_action` path as its keybinding.

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
containing project, harness, VM pid and log. It uses DECSC/DECRC around bar
repaints and refreshes only after the guest has been quiet for ten seconds.
`--no-bar` disables the reservation for guest programs that need every row.
The guest tty geometry is fixed by the serial console and cannot be resized
from the host.

---

## 9. CLI / TUI parity

| Capability | TUI | CLI |
| --- | --- | --- |
| List projects | dashboard | `hegn projects` |
| Create project | `c` | `hegn project create` |
| Import project | `h` / `←` | `hegn project import` |
| Open/attach VM | `Enter` | `hegn open`, `hegn attach` |
| Stop VM | — | `hegn sandbox stop` |
| Status | header/sidebar | `hegn sandbox status` |
| Snapshots | snapshot panel | `hegn snapshot list/create/restore/delete` |
| Logs | — | `hegn logs list/path/show/prune` |
| Security check | modal + intercepted input | `hegn check` |
| Doctor | `F3` + automatic on failure | `hegn doctor` |
| Image download | doctor modal `d` | `hegn images download` |
| Image verification | doctor report | `hegn images verify` |

The TUI is intentionally not required for automation. Anything an operator
needs to recover or audit has a CLI form.

---

## 10. Failure handling

| Failure | Behaviour |
| --- | --- |
| Missing kernel/rootfs | Hard error before spawn; doctor modal/CLI report; `hegn images download` offered |
| Missing Firecracker/virtiofsd | Hard error before any child is spawned |
| Missing `/dev/kvm` | Hard error; doctor reports it |
| Bad download/checksum | Temporary file removed; no final image written |
| Empty checksum | Refused unless `--allow-unverified` |
| Existing image | Refused unless `--force` |
| API socket missing | `status` reports stopped/unknown; start/attach explain the next step |
| VM dies during attach | Console loop ends; status updates; no host fallback |
| Session record write fails | VM and daemon killed; run files and new log removed |
| Two lifecycle calls | Second fails on `flock` with a retry hint |
| Import target unsafe | Form/CLI error; no symlink created |
| Log symlink | Listing skips it; resolution/reads refuse it |
| Snapshot load fails | New Firecracker process dies; cleanup; old VM already stopped by design (preflight prevents config errors) |
| Terminal too small for attach bar | Refuse to attach before raw mode; suggest `--no-bar`/resize |

---

## 11. Testing strategy

- All modules are unit-tested from the library crate (`src/*.rs` `#[cfg(test)]`).
- Firecracker API tests use a local Unix socket server; no `/dev/kvm` needed.
- Console tests use a FIFO and a regular serial file with a fake session record
  whose pid belongs to the test binary.
- Lifecycle tests assert preflight ordering and cleanup using missing binaries.
- Log tests cover naming, permissions, symlink refusal, tail semantics, prune
  protection and foreign-file preservation.
- Import tests cover symlink creation, discovery and dangerous targets.
- Doctor tests cover required vs optional failures.
- Image tests cover checksum success/mismatch, no-overwrite, unverified opt-in
  and atomic temporary cleanup without network access.
- UI tests render with ratatui's `TestBackend` and assert text/footer hits.
- CI runs `cargo fmt --check`, clippy with warnings denied, the full test suite,
  and a release build on Rust 1.99.1.

The daily image workflow is the only job that performs network downloads; it
does not run the test suite. The normal CI must stay offline/reproducible and
must not depend on S3 availability.

---

## 12. Operations runbook

### First install

```bash
hegn config init
hegn images download
hegn doctor
hegn project create first-project --harness opencode
hegn open first-project
```

### Add an existing repository

```bash
hegn project import ~/code/existing --name existing
hegn open existing
```

or press `h`/`←` in the project list. The original directory is untouched.

### A VM will not start

1. Read the doctor modal (opens automatically) or run `hegn doctor`.
2. If images are missing: `hegn images download`.
3. If virtiofsd/Firecracker is missing: install it or set the binary path.
   Distribution packages put `virtiofsd` in `/usr/lib/virtiofsd` (Arch),
   `/usr/libexec/virtiofsd` (Fedora), `/usr/lib/qemu/virtiofsd` (Debian) or
   `/usr/libexec/qemu/virtiofsd` (openSUSE); hegn searches those after `PATH`.
4. If `/dev/kvm` is missing: enable KVM and add your user to the `kvm` group.
5. If the config is wrong: `hegn config show`, `hegn doctor --json`.

### Inspect a past session

```bash
hegn logs
hegn logs show my-project --lines 200
hegn logs path my-project
```

### Recover disk space

```bash
hegn logs prune --keep 20
hegn snapshot list
hegn snapshot delete <name>
```

### Audit image pins

```bash
hegn images status
hegn images verify
python3 scripts/update_vm_images.py --arch x86_64 --check
```

### Detach vs stop

- `Ctrl-B` in `hegn attach`/`hegn open` only detaches.
- `hegn sandbox stop` terminates the VM and virtiofsd; logs remain.

---

## 13. Decisions and trade-offs

### ADR-001: No hegn-owned guest image

**Decision:** consume upstream Firecracker CI artifacts; pin URL + SHA-256;
refresh pins daily via CI PR.

**Why:** avoids maintaining a rootfs build/signing pipeline and a large release
artifact, while still making updates reviewable. Users who need extra packages
can build their own rootfs and point `rootfs_image`/`rootfs_*` at it.

**Consequence:** the upstream rootfs may not contain every agent/harness binary;
the harness/package list in `.hegn.toml` records intent but does not mutate the
upstream rootfs. A future custom-image workflow would be an explicit opt-in.

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

### ADR-004: Persistent log via hard link

**Decision:** Firecracker writes to `state/logs/session-*.log`; `run/serial.out`
is a hard link to the same inode.

**Why:** the boot log survives `stop` with no logger process, no copy and no
second writer. Attach sees the full log from offset 0.

**Consequence:** logs and run directory must be on the same filesystem. They are
both derived from `state_dir`, so this is normally true; a hard-link failure is
a hard error.

### ADR-005: Attach bar is an overlay, not a terminal emulator

**Decision:** reserve one host row with DECSTBM and paint the status line.

**Why:** implementing a complete VT emulator plus a guest-side resize protocol
is disproportionate; Firecracker's serial console cannot transmit window-size
changes to the guest tty anyway.

**Consequence:** full-screen guests see one fewer usable row (or can be attached
with `--no-bar`); cursor save/restore and SGR state can interact with the bar.
The TUI remains the line-intercepted view.

---

## 14. Known limitations and future work

- No guest network management; configure it outside hegn.
- No guest tty resize channel over the Firecracker serial port.
- Attach input bypasses `tirith-core` by design and is labelled; a guarded raw
  proxy would require a full terminal emulator.
- Per-session logs are not truncated while Firecracker owns the file; retention
  is by count and explicit `logs prune`.
- Snapshot restore intentionally stops the current VM after preflight; there is
  no live migration.
- The upstream rootfs is generic; harness-specific package installation is
  recorded but not baked into the downloaded image.
- CI image refresh opens a PR rather than auto-merging, so updates still need a
  human review.

---

## 15. Repository map

```text
src/
  main.rs        thin binary / exit codes
  lib.rs         module exports and crate documentation
  cli.rs         clap commands and text/JSON output
  config.rs      schema, defaults, env overrides, validation
  project.rs     discovery, import, templates, git helpers
  security.rs    tirith-core + hegn workspace guardrails
  firecracker.rs typed REST client
  virtiofs.rs    hardened virtiofsd process construction
  sandbox.rs     MicroVM lifecycle, lock, session, report
  snapshot.rs    snapshot metadata store
  images.rs      upstream image pins, downloads, SHA-256
  doctor.rs      shared host checks
  console.rs     serial transport, attach bar
  logs.rs        persistent logs, tail, prune
  terminal.rs    ANSI-aware scrollback
  state.rs       recent-project state
  app.rs         TUI state machine
  ui.rs          ratatui renderer
  tui.rs         terminal loop and effects
scripts/
  update_vm_images.py  refresh upstream pins (CI)
.github/workflows/
  rust.yml          fmt, clippy, tests, release build
  vm-images.yml     daily upstream pin refresh PR
  release.yml       tagged AppImage release
  aur-publish.yml   tagged AUR publish
docs/
  architecture.md   this document
project.md          specification and roadmap
README.md           user-facing documentation
AGENTS.md           repository working agreement
```
