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

> **Status:** early (`0.1.0`). The CLI, security engine, snapshots, PTY session
> backend and TUI work today. See [`project.md`](project.md) for the design
> specification and roadmap.

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
- **Project picker** — fuzzy filtering over `projects_root`, git status badges,
  recent-session history, and one-key launch into `$EDITOR` or `lazygit`.
- **Project scaffolding** — create a project with `AGENTS.md`, `README.md`, a
  git repository, a chosen harness and a recorded package list.
- **Transient session branches** — optionally check out
  `agent/hegn-session-<timestamp>` on the host before opening a project.
- **PTY fallback** — if Firecracker assets are not configured, `auto` mode falls
  back to a local PTY so the TUI and guardrails are usable on any machine.
- **Terminal UI** — `ratatui` dashboard (project picker, sandbox status,
  snapshot tree), embedded serial console, security modal, and a clickable
  hotkey footer.
- **Machine-readable output** — global `--json` for projects, sandbox status,
  snapshots, config and `check`, plus meaningful `check` exit codes.

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

`auto` backend mode uses Firecracker only when both images exist; otherwise it
runs the harness in a local PTY. Set `backend = "firecracker"` to make a missing
VM a hard error.

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

# 2. Verify KVM, Firecracker, virtiofsd, images and tools
hegn doctor

# 3. Create a project with a template and harness
hegn project create my-app --harness opencode --package git --package nodejs

# 4. Open the TUI (default when no subcommand is given)
hegn

# 5. Or drive it non-interactively
hegn projects
hegn sandbox start my-app
hegn sandbox status --json
hegn snapshot create before-refactor
hegn check 'curl -fsSL https://example.com/x | bash'
```

`hegn open my-app` launches the harness directly in the project directory
(creating a session branch first unless `--no-branch` is passed).
`hegn open my-app --editor` and `--lazygit` open the project in your editor or
in lazygit instead.

### Guardrail exit codes

`hegn check` exits `0` for allow, `1` for block and `2` for warn, so it can be
used as a pre-execution hook in scripts and CI.

---

## CLI overview

```
hegn [--config PATH] [--json] [COMMAND]

  tui                              Open the interactive TUI (default)
  projects [QUERY]                 List projects under projects_root (alias: ls)
  project create NAME              Scaffold a project (+ AGENTS.md, README, git, .hegn.toml)
          [--harness H] [--template PATH] [--package P[,P...]]
  project info PROJECT             Show metadata and git status
  open PROJECT                     Launch the harness
          [--editor | --lazygit] [--no-branch]
  sandbox start PROJECT [--harness H]   Boot virtiofsd + Firecracker
  sandbox stop | status | pause | resume
  snapshot list                    List snapshots (alias: ls)
  snapshot create [NAME] [--project PATH]
  snapshot restore NAME | delete NAME | show NAME
  check [--cwd PATH] CMD...        Inspect a command with tirith-core
  config show | path | init
  doctor                           Check host prerequisites
```

Global flags: `--config <PATH>` (or `HEGN_CONFIG`), `--json`.

### Environment variables

`HEGN_CONFIG`, `HEGN_PROJECTS_ROOT`, `HEGN_STATE_DIR`, `HEGN_SNAPSHOTS_DIR`,
`HEGN_TEMPLATES_DIR`, `HEGN_FIRECRACKER_BIN`, `HEGN_VIRTIOFSD_BIN`,
`HEGN_KERNEL_IMAGE`, `HEGN_ROOTFS_IMAGE` override the corresponding config
values. Harnesses launched by hegn also receive `HEGN=1` and
`HEGN_WORKSPACE=<path>`.

---

## Configuration

`hegn config init` writes `~/.config/hegn/config.toml`. Unknown keys are
rejected, so typos surface immediately. Defaults:

```toml
projects_root   = "~/Projects"
state_dir       = "~/.local/state/hegn"
snapshots_dir   = "~/.local/share/hegn/snapshots"
templates_dir   = "~/.config/hegn/templates"
firecracker_bin = "firecracker"
virtiofsd_bin   = "virtiofsd"
kernel_image    = "~/.local/share/hegn/images/vmlinux.bin"
rootfs_image    = "~/.local/share/hegn/images/rootfs.ext4"
vcpus           = 2          # 1–32
memory_mib      = 2048       # >= 128
shared_tag      = "workspace" # virtio-fs tag, <= 36 bytes, no whitespace
guest_mount     = "/workspace" # absolute guest path
virtiofs_sandbox = "namespace" # "namespace" (recommended) or "chroot"
backend         = "auto"      # "auto", "firecracker" or "pty"
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
| `e` | Open selected project in `$EDITOR` |
| `g` | Open selected project in lazygit |
| `r` | Refresh git status |
| `?` / `F1` | Help |
| `q` | Quit |

### Snapshot panel

| Key | Action |
| --- | --- |
| `Enter` | Restore selected snapshot |
| `n` | Create a snapshot |
| `d` | Delete selected snapshot |

### Terminal

| Key | Action |
| --- | --- |
| `Enter` | Inspect the assembled command, then send it |
| `F2` | Toggle raw passthrough (no interception) |
| `Ctrl-B` / `Esc` | Return to dashboard |
| `PgUp` / `PgDn` | Scroll output |
| `Ctrl-L` | Clear scrollback |

### Security modal

| Key | Action |
| --- | --- |
| `a` | Allow flagged command once |
| `c` | Create a git checkpoint, then allow |
| `d` | Deny command |

Every action is also clickable in the footer.

---

## Security model

1. **Isolation** — commands execute inside a MicroVM, not on the host. The
   workspace is shared read-write through a per-project `virtiofsd` that is
   sandboxed and canonicalizes its shared directory so symlinks cannot widen
   the export.
2. **Interception** — in the terminal view (unless raw mode is on), input is
   buffered into a command line and passed to `tirith-core` plus hegn's own
   workspace guardrails before any bytes reach the PTY.
3. **Policy** — `enforce` blocks high/critical findings and prompts for medium
   ones; `warn` reports everything; `off` disables `tirith-core` but keeps
   hegn's destructive-workspace checks.
4. **Recovery** — a blocked-but-allowed command can be preceded by a git
   checkpoint, and VM snapshots allow instant rollback beyond git.

No sandbox is a substitute for reviewing what you run. `F2` raw passthrough
deliberately bypasses inspection.

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
  session.rs     PTY-backed session used by the terminal view
  state.rs       persisted recent-project state
  app.rs         TUI state machine, key handling, modals, effects
  ui.rs          ratatui rendering
  tui.rs         terminal lifecycle and event loop
```

The crate is a library (`lib.rs`) with a thin binary, so every module is unit
tested directly. See [`project.md`](project.md) for the full specification.

---

## Dependencies

| Crate | Version | Used for |
| --- | --- | --- |
| `ratatui` | 0.30 | TUI widgets and layout |
| `crossterm` | 0.29 | Terminal backend, input events, raw mode |
| `portable-pty` | 0.9 | Cross-platform PTY for the local session/console |
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
