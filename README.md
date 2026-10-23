# hegn

**Firecracker MicroVM sandbox manager and TUI for agentic coding.**

`hegn` runs coding agents (OpenCode, Claude Code, Codex, pi, prime-agent, Aider,
Antigravity Cli) inside hardware-isolated Firecracker MicroVMs. Each session gets a
dedicated `virtiofsd` share containing exactly one project directory, command
input is inspected by [`tirith-core`](https://crates.io/crates/tirith-core)
before it reaches the guest, and the whole VM can be snapshotted and restored in
milliseconds.

The package ships a single binary, `hegn`. Run it without a subcommand to open
the TUI; every TUI action also has a scripting-friendly CLI counterpart.

> **Status:** early (`0.1.0`). The CLI, security engine, snapshots, Firecracker
> serial console and TUI work today. See [`project.md`](project.md) for the
> design specification and roadmap.

---

## No VM, no work

hegn does not run coding agents, shells or other project work on the host as a
fallback. Every terminal and `hegn open` session is attached to a real
Firecracker guest serial console. If the kernel, rootfs, Firecracker binary,
virtiofsd or KVM is missing, the command fails and tells you what to fix.

---

## Features

- **Hardware isolation** — each project runs in a Firecracker MicroVM with its
  own kernel and root filesystem, configured over the Firecracker REST API on a
  Unix socket.
- **Zero-copy workspace sharing** — a per-session `virtiofsd` exports only the
  selected project, always sandboxed (`--sandbox=namespace` by default,
  `chroot` optional), and mounts it at `/workspace` in the guest.
- **Command guardrails** — `tirith-core` plus hegn's own checks inspect commands
  for mixed-script homoglyphs, secret exfiltration (`.env`, SSH keys, cloud
  credentials), encoded/obfuscated payloads and destructive workspace
  operations. Flagged commands can be allowed once, denied, or preceded by an
  automatic git checkpoint.
- **Snapshot manager** — pause, snapshot RAM + CPU, restore and delete full VM
  states, with private (`0700`/`0600`) on-disk metadata.
- **Project picker** — fuzzy filtering over `projects_root` (default
  `~/Projects/hegn`), git status badges, recent-session history, and one-key
  launch into `$EDITOR` or `lazygit`.
- **Project import** — press `h` or `←` in the project list to register an
  existing directory. The project stays where it is; hegn only creates a
  symlink inside `projects_root`. Dangerous targets (`/`, `$HOME`, parents of
  the root, hegn state) are refused.
- **Project scaffolding** — create a project with `AGENTS.md`, `README.md`, a
  git repository, a chosen harness and a recorded package list.
- **Pinned upstream VM images** — `hegn images download` fetches the official
  Firecracker kernel and ext4 rootfs, verifies SHA-256 and installs them with
  private permissions. A daily CI job refreshes the pins; hegn never builds or
  publishes its own guest image.
- **Doctor modal** — host failures open a doctor overlay automatically with the
  failing checks and a one-key image download. `F3` opens it manually.
- **Transient session branches** — optionally check out
  `agent/hegn-session-<timestamp>` on the host once the VM is booted.
- **Mandatory VM** — a missing Firecracker VM is always a hard error. hegn never
  falls back to a host shell or local PTY.
- **Detachable serial console** — the guest `ttyS0` is wired to a private FIFO
  (`serial.in`) for input and a persistent serial log for output, so the TUI,
  `hegn open` and `hegn attach` can detach and re-attach without restarting the
  VM. `hegn attach` (alias `hegn a`) works like `tmux attach` and keeps a
  status bar on the bottom line with the project name, harness, VM pid and log.
- **Private session logs** — boot and session output survive `hegn sandbox
  stop` in `state_dir/logs/` (`0700` directory, `0600` files, `O_NOFOLLOW`).
  `hegn logs list|path|show|prune` manages them.
- **Terminal UI** — `ratatui` dashboard (project picker, sandbox status,
  snapshot tree), embedded guest serial console, security modal, and a clickable
  hotkey footer.
- **Machine-readable output** — global `--json` for projects, sandbox status,
  snapshots, logs, images, doctor, config and `check`, plus meaningful `check`
  and `images verify` exit codes.

---

## Requirements

| Requirement | Notes |
| --- | --- |
| Linux with KVM | `/dev/kvm` must exist and be accessible to your user (`kvm` group). x86_64 and aarch64 targets are published. |
| [Firecracker](https://github.com/firecracker-microvm/firecracker) | `firecracker` on `PATH` (or set `firecracker_bin`). |
| [virtiofsd](https://gitlab.com/virtio-fs/virtiofsd) | `virtiofsd` on `PATH` (or set `virtiofsd_bin`). |
| Kernel + rootfs images | A guest `vmlinux` and ext4 rootfs. Defaults under `~/.local/share/hegn/images/`; see `kernel_image` / `rootfs_image`. |
| `git` | Used for project status, session branches and checkpoints. |
| `lazygit`, `$EDITOR` | Optional, for the `g` and `e` shortcuts. |

`hegn doctor` checks all of the above and reports what is missing.

Both backend modes require a real Firecracker VM. A missing kernel, rootfs,
Firecracker binary, virtiofsd, or KVM device is a hard error; there is no local
PTY fallback.

---

## Installation

### Arch Linux (AUR)

```bash
# build from source
paru -S hegn

# pre-built binary
paru -S hegn-bin
```

### AppImage / pre-built binary

Tagged releases publish `hegn-<tag>-x86_64-linux-gnu` binaries and
`hegn-<tag>.AppImage` on the
[releases page](https://github.com/hegn-tui/hegn/releases).

### From source

Requires Rust **1.88+** (edition 2024).

```bash
git clone https://github.com/hegn-tui/hegn
cd hegn
cargo build --release --locked
install -Dm755 target/release/hegn ~/.local/bin/hegn
```

---

## Quick start

```bash
# 1. Write a default config and create runtime directories
hegn config init

# 2. Download the pinned upstream kernel + rootfs (SHA-256 verified)
hegn images download

# 3. Verify KVM, Firecracker, virtiofsd, images and tools
hegn doctor

# 4. Create a project with a template and harness
hegn project create my-app --harness opencode --package git --package nodejs

# 5. Open the TUI (default when no subcommand is given)
hegn

# 6. Or drive it non-interactively
hegn ls # alternatively `hegn projects`
hegn sandbox start my-app
hegn attach my-app
hegn sandbox status --json
hegn snapshot create before-refactor
hegn logs
hegn project import /path/to/existing/repo
hegn images status
hegn check 'curl -fsSL https://example.com/x | bash'
```

`hegn open my-app` boots the Firecracker VM if needed and attaches its serial
console (creating a session branch first unless `--no-branch` is passed). Press
`Ctrl-B` to detach without stopping the VM. The project harness is recorded in
the session metadata and shown in the status line; launch it from the guest
shell. `hegn open my-app --editor` and
`--lazygit` are host-side project tools and do not start a sandbox.

### Re-attaching like tmux

```bash
hegn attach          # attach to the single running VM
hegn a my-app        # attach, but only if the running VM belongs to my-app
hegn attach --no-bar # give full-screen guest programs the whole terminal
```

`hegn attach` never boots or restarts a VM. It fails if there is no running
session, if the recorded pid is no longer the configured Firecracker binary, or
if the optional project argument does not match the running session. The
bottom line is reserved for a status bar:

```
 hegn ▸ my-app · OpenCode · pid 4242 · log session-…-my-app-4242.log   INPUT DIRECT · ^B detach
```

`Ctrl-B` detaches without stopping the VM. Input is forwarded directly to the
guest UART; the `INPUT DIRECT` marker makes that bypass of `tirith-core`
visible. Use the TUI terminal view for intercepted line input, or `hegn check`
in scripts. The guest console geometry cannot be changed over the Firecracker
serial port, so the bar uses one host row and full-screen guest programs keep
their own size; pass `--no-bar` (also accepted by `hegn open`) when a guest TUI
needs every row.

### Importing an existing project

```bash
hegn project import ~/code/existing-repo
hegn project import /mnt/data/repo --name repo-picker-name
```

Import creates a symlink in `projects_root`; the directory is never moved,
copied or modified. The picker then treats it like any other project. Open the
TUI and press `h` or `←` in the project list to do the same interactively.

Safety rules: hegn refuses `/`, `$HOME`, anything inside `projects_root`
(already discoverable), any ancestor of `projects_root`, anything overlapping
hegn state/snapshots/templates/config, and a picker name that is not a plain
directory name. An existing picker entry is never overwritten. Discovery also
skips symlinks to those dangerous targets, including hegn state.

### VM images

hegn uses **upstream Firecracker CI images** and does not build its own guest
image. The kernel and ext4 rootfs URLs are pinned together with SHA-256 in
`src/images.rs` (per architecture) and can be overridden in `config.toml`.

```bash
hegn images status          # path, state, size
hegn images download        # fetch missing images, verify, install atomically
hegn images download --force
hegn images verify          # re-hash installed files (exit 1 on mismatch)
```

Downloads are streamed to a `0600` temporary file, hashed, and only renamed
into place after the digest matches. Existing files are not replaced without
`--force`. If a custom configuration has no SHA-256, downloads refuse to run
unless `--allow-unverified` is passed explicitly.

The repository's `vm-images` GitHub Actions workflow runs daily, discovers the
newest upstream kernel/ext4 artifact, recomputes the digests, and opens a pull
request with the updated pins. It does not publish a hegn-owned image.

### Session logs

Every VM session writes its serial output to
`~/.local/state/hegn/logs/session-<timestamp>-<project>-<pid>.log`. The log is
created private, and `run/serial.out` is a hard link to it, so stopping the VM
does not erase the boot log.

```bash
hegn logs                       # list, newest first
hegn logs path my-app           # absolute path (newest by default)
hegn logs show --lines 120      # tail with ANSI/control sequences stripped
hegn logs show --raw            # exact bytes; only for trusted logs
hegn logs prune --keep 20       # only hegn's session-*.log files
```

Logs may contain secrets an agent printed. They stay `0600` in a `0700`
directory, symlinks are rejected, and `show` sanitizes terminal control and
bidi-override sequences unless `--raw` is explicitly requested. A session log
is not truncated while Firecracker owns it; retention is by count (default 50)
and `hegn logs prune` can be run at any time. Pruning always protects the log of
the currently running session.

### Guardrail exit codes

`hegn check` exits `0` for allow, `1` for block and `2` for warn, so it can be
used as a pre-execution hook in scripts and CI.

---

## CLI overview

```bash
hegn [--config PATH] [--json] [COMMAND]

  tui                              Open the interactive TUI (default)
  projects [QUERY]                 List projects under projects_root (alias: ls)
  project create NAME              Scaffold a project (+ AGENTS.md, README, git, .hegn.toml)
          [--harness H] [--template PATH] [--package P[,P...]]
  project import PATH [--name N]   Symlink an existing directory into projects_root
  project info PROJECT             Show metadata and git status
  open PROJECT                     Boot/attach the VM serial console
          [--editor | --lazygit] [--no-branch] [--no-bar]
  attach [PROJECT] [--no-bar]      Attach to the running VM (alias: a), tmux-style
  logs [list|path|show|prune]      Manage persistent serial logs (alias: log)
  sandbox start PROJECT [--harness H]   Boot virtiofsd + Firecracker
  sandbox stop | status | pause | resume
  snapshot list                    List snapshots (alias: ls)
  snapshot create [NAME] [--project PATH]
  snapshot restore NAME | delete NAME | show NAME
  check [--cwd PATH] CMD...        Inspect a command with tirith-core
  config show | path | init
  images download [--force]        Fetch missing pinned kernel/rootfs images
          [--allow-unverified]
  images status | verify           Show or re-hash the configured images
  doctor                           Check host prerequisites (exit 1 if required checks fail)
```

Global flags: `--config <PATH>` (or `HEGN_CONFIG`), `--json`.

### Environment variables

`HEGN_CONFIG`, `HEGN_PROJECTS_ROOT`, `HEGN_STATE_DIR`, `HEGN_SNAPSHOTS_DIR`,
`HEGN_TEMPLATES_DIR`, `HEGN_FIRECRACKER_BIN`, `HEGN_VIRTIOFSD_BIN`,
`HEGN_KERNEL_IMAGE`, `HEGN_ROOTFS_IMAGE`, `HEGN_KERNEL_URL`,
`HEGN_KERNEL_SHA256`, `HEGN_ROOTFS_URL`, `HEGN_ROOTFS_SHA256` override the
corresponding config values. Harnesses launched by hegn also receive `HEGN=1`
and `HEGN_WORKSPACE=<path>`.

---

## Configuration

`hegn config init` writes `~/.config/hegn/config.toml`. Unknown keys are
rejected, so typos surface immediately. Defaults:

```toml
projects_root   = "~/Projects/hegn"
state_dir       = "~/.local/state/hegn"
snapshots_dir   = "~/.local/share/hegn/snapshots"
templates_dir   = "~/.config/hegn/templates"
firecracker_bin = "firecracker"
virtiofsd_bin   = "virtiofsd"
kernel_image    = "~/.local/share/hegn/images/vmlinux.bin"
rootfs_image    = "~/.local/share/hegn/images/rootfs.ext4"
# The image pin below shows x86_64; aarch64 ships different defaults.
kernel_url      = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/x86_64/vmlinux-6.1.155"
kernel_sha256   = "e20e46d0c36c55c0d1014eb20576171b3f3d922260d9f792017aeff53af3d4f2"
rootfs_url      = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/ubuntu-22.04.ext4"
rootfs_sha256   = "040927105bd01b19e7b02cd5da5a9552b428a7f84bd5ffc22ebfce4ddf258a07"
vcpus           = 2          # 1–32
memory_mib      = 2048       # >= 128
shared_tag      = "workspace" # virtio-fs tag, <= 36 bytes, no whitespace
guest_mount     = "/workspace" # absolute guest path
virtiofs_sandbox = "namespace" # "namespace" (recommended) or "chroot"
backend         = "auto"      # "auto" or "firecracker"; both need a VM
harness         = "shell"     # shell | opencode | claude-code | codex | pi |
                              # prime-agent | aider | gemini-cli
extra_packages  = []
auto_branch     = true
security        = "enforce"   # "off", "warn" or "enforce"
```

Project-level defaults live in `.hegn.toml` inside each project (written by
`hegn project create`) and record the harness, package list and template used.

### AGENTS.md templates

Place templates in `~/.config/hegn/templates/`. `hegn project create` copies
`~/.config/hegn/templates/AGENTS.md` automatically when it exists; pass
`--template <PATH>` to use a different file.

---

## TUI keybindings

### Dashboard

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | Move selection |
| `Enter` / `o` | Open project in sandbox (or restore snapshot when snapshots are focused) |
| `/` | Fuzzy filter projects |
| `Tab` | Switch focus between projects and snapshots |
| `c` | Create project |
| `h` / `←` | Import an existing directory (symlink, no copy) |
| `e` | Open selected project in `$EDITOR` |
| `g` | Open selected project in lazygit |
| `r` | Refresh git status |
| `F3` | Doctor modal (host checks, image download) |
| `?` / `F1` | Help |
| `q` | Quit |

### Snapshot panel

| Key | Action |
| --- | --- |
| `Enter` | Restore selected snapshot |
| `n` | Create a snapshot |
| `d` | Delete selected snapshot |

### Terminal (guest serial console)

| Key | Action |
| --- | --- |
| `Enter` | Inspect the assembled command, then send it to the VM |
| `F2` | Toggle raw passthrough (no interception) |
| `Ctrl-B` / `Esc` | Return to dashboard |
| `PgUp` / `PgDn` | Scroll output |
| `Ctrl-L` | Clear scrollback |

### Doctor modal

| Key | Action |
| --- | --- |
| `d` | Download and verify missing VM images (background) |
| `r` | Re-run all checks |
| `Esc` / `q` | Close |

The doctor modal opens automatically after a VM start/attach failure. Optional
tools (lazygit, `$EDITOR`) never make the report fail.

### Security modal

| Key | Action |
| --- | --- |
| `a` | Allow flagged command once |
| `c` | Create a git checkpoint, then allow |
| `d` | Deny command |

Every action is also clickable in the footer.

### Theming

The TUI uses your terminal's own foreground/background and the ANSI color
palette. hegn does not paint a custom full-screen background, so existing
terminal themes (dark or light) apply unchanged. Only selected rows, badges and
diagnostic cells use a small ANSI background color.

---

## Security model

1. **Isolation** — commands execute inside a MicroVM, not on the host. The
   workspace is shared read-write through a per-project `virtiofsd` that is
   sandboxed and canonicalizes its shared directory so symlinks cannot widen
   the export. If no VM can be started, hegn refuses to launch work.
2. **Interception** — in the TUI terminal view (unless raw mode is on), input is
   buffered into a command line and passed to `tirith-core` plus hegn's own
   workspace guardrails before any bytes reach the guest serial port.
3. **Policy** — `enforce` blocks high/critical findings and prompts for medium
   ones; `warn` reports everything; `off` disables `tirith-core` but keeps
   hegn's destructive-workspace checks.
4. **Recovery** — a blocked-but-allowed command can be preceded by a git
   checkpoint, and VM snapshots allow instant rollback beyond git.

No sandbox is a substitute for reviewing what you run. `F2` raw passthrough
deliberately bypasses inspection. `hegn attach` is raw as well and marks this
in its status bar as `INPUT DIRECT`; use `hegn check` for scripted
pre-execution checks or the TUI for intercepted input.

Persistent serial logs are treated as untrusted data. They live in a private
directory, are opened without following symlinks, and `hegn logs show` strips
terminal escape sequences by default. `--raw` exists for debugging and should
only be used with logs you trust.

VM images are upstream Firecracker CI artifacts pinned by HTTPS URL and SHA-256.
`hegn images download` refuses empty checksums unless `--allow-unverified` is
explicit, verifies the stream before the atomic rename, and never replaces an
existing image without `--force`. `hegn images verify` re-hashes installed
files. Project import only creates a symlink and refuses targets that would
expose `/`, `$HOME`, a projects-root ancestor, or hegn state to the guest.

---

## Architecture

```
src/
  main.rs        binary entry point and exit-code handling
  cli.rs         clap command surface; non-interactive parity with the TUI
  config.rs      config schema, XDG paths, env overrides, validation
  project.rs     project discovery, fuzzy filter, git summaries, scaffolding
  security.rs    tirith-core integration + workspace guardrails
  firecracker.rs typed REST client over the Firecracker Unix socket
  virtiofs.rs    hardened virtiofsd argv/process construction
  sandbox.rs     MicroVM lifecycle orchestration and status reporting
  snapshot.rs    snapshot metadata store (RAM + CPU files)
  images.rs      pinned upstream VM assets, download, SHA-256 verification
  doctor.rs      shared host prerequisite checks (CLI + TUI modal)
  logs.rs        persistent serial logs, private files, tail and pruning
  console.rs     Firecracker serial console transport + tmux-style attach bar
  terminal.rs    ANSI-aware scrollback buffer for the serial console
  state.rs       persisted recent-project state
  app.rs         TUI state machine, key handling, modals, effects
  ui.rs          ratatui rendering
  tui.rs         terminal lifecycle and event loop
```

The crate is a library (`lib.rs`) with a thin binary, so every module is unit
tested directly. See [`project.md`](project.md) for the specification and
[`docs/architecture.md`](docs/architecture.md) for the detailed architecture,
data flows, safety invariants, and image-update pipeline.

---

## Dependencies

| Crate | Version | Used for |
| --- | --- | --- |
| `ratatui` | 0.30 | TUI widgets and layout |
| `crossterm` | 0.29 | Terminal backend, input events, raw mode |
| `tokio` | 1 (full) | Async runtime for Firecracker API calls and snapshots |
| `clap` | 4 (derive, env) | CLI parsing and help |
| `serde` | 1 (derive) | Config, metadata and API serialization |
| `serde_json` | 1 | Firecracker REST payloads and `--json` output |
| `toml` | 0.9 | `config.toml` and `.hegn.toml` |
| `tirith-core` | 0.4.2 | In-memory command inspection and policy engine |
| `anyhow` | 1 | Context-rich application errors |
| `thiserror` | 2 | Typed errors in library-facing modules |
| `chrono` | 0.4 (serde) | Snapshot/session timestamps |
| `dirs` | 6 | XDG config/state/data directories |
| `libc` | 0.2 | Unix process and permission primitives |
| `unicode-width` | 0.2 | Correct terminal column widths for the TUI |

Dev-dependencies: `tempfile` (3) for isolated config/state/snapshot fixtures.

Runtime tools (not Rust dependencies): `firecracker`, `virtiofsd`, `git`, and
optionally `lazygit` and `$EDITOR`.

---

## Development

```bash
cargo fmt                 # format
cargo clippy --all-targets -- -D warnings
cargo test                # unit and integration tests
cargo build --release --locked
```

CI (`.github/workflows/rust.yml`) pins the toolchain to `1.99.1` and runs
`cargo fmt --check`, clippy with warnings denied, the full test suite, and a
release build. Tagged `v*` pushes publish binaries, an AppImage, and the AUR
packages `hegn` and `hegn-bin` via `generate-pkgbuild*.bash`.

See [`AGENTS.md`](AGENTS.md) for the repository working agreement.

---

## License

Licensed under either of MIT or Apache-2.0 at your option
(`license = "MIT OR Apache-2.0"` in `Cargo.toml`). Contributions are accepted
under the same terms.
