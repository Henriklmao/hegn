# hegn

**Cloud Hypervisor instance manager and TUI for agentic coding.**

>***hegn*** *is Norwegian/Danish for "fence" or "enclosure" – and that is exactly what this project provides: a secure, hardware-isolated enclosure for all your coding agents and research projects.*

`hegn` runs coding agents (OpenCode, Claude Code, Codex, pi, prime-agent, Aider,
Antigravity Cli) inside hardware-isolated Cloud Hypervisor VMs. Each session gets a
dedicated `virtiofsd` share containing exactly one project directory, command
input is inspected by [`tirith-core`](https://crates.io/crates/tirith-core)
before it reaches the guest, and the whole VM can be snapshotted and restored in
milliseconds.

The package ships a single binary, `hegn`. Run it without a subcommand to open
the TUI; every TUI action also has a scripting-friendly CLI counterpart.

> **Status:** early (`0.1.0`). The CLI, security engine, snapshots, Cloud
> Hypervisor serial console and TUI work today. See
> [`docs/architecture.md`](docs/architecture.md) for the design.

---

## No VM, no work

hegn does not run coding agents, shells or other project work on the host as a
fallback. Every terminal and `hegn open` session is attached to a real
Cloud Hypervisor guest serial console. If the kernel, rootfs, Cloud Hypervisor
binary, virtiofsd or KVM is missing, the command fails and tells you what to
fix.

---

## Features

- **Hardware isolation** — each project runs in a Cloud Hypervisor VM with its
  own kernel and root filesystem, configured through a hardened Cloud Hypervisor
  command line and controlled over its REST API on a Unix socket.
- **Native virtio-fs** — Cloud Hypervisor implements the virtio-fs device
  directly, so no patched hypervisor build is needed for workspace sharing.
  The generated cloud-init seed mounts the tag at `/workspace` (configurable via
  `guest_mount`).
- **Multiple instances** — run several VMs at the same time. Every instance has
  its own `hegn.toml`, runtime sockets, serial log and private rootfs copy under
  `instances_dir`.
- **Multi-agent clones** — clone an instance into independent git workspaces so
  parallel agents cannot overwrite each other's commits; merge or open pull
  requests when they finish.
- **Harness profiles** — `hegn config init` writes one profile template per
  harness under `~/.config/hegn/profiles/`. A profile can add apt packages,
  copy configuration files and API keys into the guest, and export environment
  variables. Host skills are **copied** into a private per-instance bundle (never
  linked), so a poisoned guest cannot modify the host's real skill directories;
  `hegn skills sync-back` copies guest-created or changed skills back on demand.
- **Config menus** — `F4` opens an interactive settings editor for the global
  config, `F5` for the selected instance's `hegn.toml`. It has a **Profiles**
  tab that opens the per-harness TOML (creating a missing template first).
  Navigate with the arrow keys or the mouse, change values in place, and save
  with `Ctrl-S`. A raw `$EDITOR` round-trip stays available for comments and
  advanced keys.
- **Modal terminal** — Vim-style `i`/`a` insert mode and `Esc` to return to
  normal mode, so scrolling and shortcuts never leak keystrokes into the guest.
- **Safe deletion** — deleting an instance asks twice, offers to keep clone
  files, and never deletes the original project of a normal instance.
- **Defence in depth** — Cloud Hypervisor runs with seccomp filters and
  Landlock filesystem sandboxing; `virtiofsd` always runs with
  `--sandbox=namespace` (or `chroot`).
- **Guarded networking** — guests only reach the public web: host nftables
  allows public HTTP(S) + DNS and drops the LAN, IPv6 and all guest→host
  traffic. Host firewalls (ufw, firewalld, plain iptables) are exempted for the
  TAP pool so their routed policy cannot silently black-hole the guest. Presets
  install their real CLI (Node 22, Python, etc.) in the guest on first boot.
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
  Cloud Hypervisor kernel and cloud image, verifies SHA-256 and installs them
  with private permissions. A weekly CI job refreshes the pins; hegn never
  builds or publishes its own guest image.
- **Doctor modal** — host failures open the doctor automatically on first launch
  and after VM start/attach failures. Press `s` to confirm one automatic setup:
  create directories, write the default config, download/verify images and
  configure the guarded network (sudo/pkexec prompt). `F3` opens it manually.
- **Transient session branches** — optionally check out
  `agent/hegn-session-<timestamp>` on the host once the VM is booted.
- **Mandatory VM** — a missing Cloud Hypervisor VM is always a hard error. hegn
  never falls back to a host shell or local PTY.
- **Detachable serial console** — Cloud Hypervisor exposes the guest serial
  port on a private Unix socket. A detached relay keeps the persistent log
  growing, so the TUI, `hegn open` and `hegn attach` can detach and re-attach
  without restarting the VM. `hegn attach` (alias `hegn a`) works like `tmux attach` and keeps a
  status bar on the bottom line with the project name, harness, VM pid and log.
- **Private session logs** — boot and session output survive `hegn instance
  stop` in `state_dir/logs/` (`0700` directory, `0600` files, `O_NOFOLLOW`).
  `hegn logs list|path|show|prune` manages them.
- **Terminal UI** — `ratatui` dashboard (project picker, instance status,
  snapshot tree), embedded guest serial console, security modal, and a clickable
  hotkey footer.
- **Machine-readable output** — global `--json` for projects, instance status,
  snapshots, logs, images, doctor, config and `check`, plus meaningful `check`
  and `images verify` exit codes.

---

## Requirements

| Requirement | Notes |
| --- | --- |
| Linux with KVM | `/dev/kvm` must exist and be accessible to your user (`kvm` group). x86_64 and aarch64 targets are published. |
| [Cloud Hypervisor](https://www.cloudhypervisor.org/) | `cloud-hypervisor` **or** `cloud-hypervisor-static` on `PATH` (AUR's `cloud-hypervisor-bin` installs the static name), or set `cloud_hypervisor_bin`. Native virtio-fs support is required; hegn enables seccomp and Landlock. |
| Landlock | A host kernel with Landlock ABI v3 (Linux 6.2 or newer). `hegn doctor` probes it; set `landlock = false` only if you accept the reduced isolation. |
| [virtiofsd](https://gitlab.com/virtio-fs/virtiofsd) | `virtiofsd` on `PATH` (or set `virtiofsd_bin`). Distribution install locations such as `/usr/lib/virtiofsd` (Arch), `/usr/libexec/virtiofsd` (Fedora) and `/usr/lib/qemu/virtiofsd` (Debian) are detected automatically. |
| Kernel + rootfs images | A guest kernel and qcow2/raw root filesystem. Defaults under `~/.local/share/hegn/images/`; see `kernel_image`, `rootfs_image`, `rootfs_format` and `root_device`. |
| Free disk | ~2.5 GiB for the pinned images and tools, plus ~`memory_mib` per snapshot and log space. |
| `git` | Used for project status, session branches and checkpoints. |
| `lazygit`, `$EDITOR` | Optional, for the `g` and `e` shortcuts. |

`hegn doctor` checks all of the above and reports what is missing.

Both backend modes require a real Cloud Hypervisor VM. A missing kernel,
rootfs, Cloud Hypervisor binary, virtiofsd, or KVM device is a hard error; there
is no local PTY fallback.

---

## Resource footprint

A running session costs the configured `memory_mib` (default 2 GiB) plus roughly
50 MiB for `virtiofsd`, the console relay and VMM bookkeeping, and ~26 MiB more
while the TUI is open. When a harness profile exports files, skills or
environment, a second `virtiofsd` adds about another 12 MiB. Base instances and
clones can run concurrently, so the cost scales per instance. On disk, the pinned
images take ~2.2 GiB (shared by every project), the binary ~19 MiB, and a
snapshot about as much as the guest RAM; session logs are pruned by count.

Compute runs near-native under KVM, while many-small-file I/O is slower because
the workspace is exported through virtio-fs with `--cache=never`. Measured
numbers, the native comparison and tuning tips are in
[`docs/resources.md`](docs/resources.md).

---

## Installation

### Install with an LLM agent

Hand the installation to your coding agent:

```text
Install Hegn, a MicroVM base containerization wizard according to the following guide: https://github.com/Henriklmao/hegn/blob/main/docs/llm-setup.md
```

The guide walks the agent through host checks, installation on Arch (AUR) or any
glibc distribution, VM images, `hegn doctor` and a smoke test.

### Arch Linux (AUR)

```bash
# build from source
paru -S hegn
# or
yay -S hegn

# pre-built binary
paru -S hegn-bin
# or 
yay -S hegn-bin
```

latest git snapshot is available via

```bash
paru -S hegn-git
# or
yay -S hegn-git
```

### AppImage / pre-built binary

Tagged releases publish `hegn-<tag>-x86_64-linux-gnu` binaries and
`hegn-<tag>.AppImage` on the
[releases page](https://github.com/Henriklmao/hegn/releases).

### Upgrading from an older hegn

The VM backend and config schema changed. Before starting the new build, stop
any VM started by an older version and remove its `state_dir/run/` directory.
Then update `config.toml`:

- replace the old VMM binary key with `cloud_hypervisor_bin = "cloud-hypervisor"`
  (the static AUR binary `cloud-hypervisor-static` is detected automatically)
- set `rootfs_format` (`raw` or `qcow2`) and `root_device` (`/dev/vda1` for the
  official cloud image, `/dev/vda` for a whole-disk ext4 image)
- `landlock`, `seccomp` and `cloud_init` default to `true`
- re-run `hegn images download` if the old kernel/rootfs paths are still
  configured

An unreadable `session.json` is treated as a hard error rather than overwritten,
so a forgotten old VMM cannot be leaked.

### From source

Requires Rust **1.88+** (edition 2024).

```bash
git clone https://github.com/Henriklmao/hegn
cd hegn
cargo build --release --locked
install -Dm755 target/release/hegn ~/.local/bin/hegn
```

---

## Quick start

```bash
# 1. One command: check the host and prepare everything hegn owns —
#    config, directories, pinned images and the guarded network.
#    It prompts for sudo when it needs the firewall/TAPs.
hegn doctor --fix

# 2. Create a project with a template and harness.
#    --package adds to the preset; it never replaces it.
hegn project create my-app --harness opencode --package jq

# 3. Open the TUI (default when no subcommand is given). If something is still
#    missing, the doctor opens automatically; press s to prepare it.
hegn

# 4. Or drive it non-interactively
hegn ls # alternatively `hegn projects`
hegn instance list
hegn instance start my-app
hegn attach my-app
hegn instance clone my-app --count 2   # multi-agent clones
hegn instance status my-app --json
hegn snapshot create before-refactor
hegn logs
hegn project import /path/to/existing/repo
hegn images status
hegn check 'curl -fsSL https://example.com/x | bash'
```

`hegn open my-app` boots the Cloud Hypervisor VM if needed and attaches its serial
console (creating a session branch first unless `--no-branch` is passed). The
start runs on a worker thread, so the TUI immediately shows a spinner and keeps
responding while the VM boots. Press `Ctrl-B` (or `Ctrl-]`) to detach without stopping the VM. The project harness is recorded in
the session metadata and shown in the status line; launch it from the guest
shell. `hegn open my-app --editor` and
`--lazygit` are host-side project tools and do not start an instance.

### Harness presets and first-boot provisioning

A harness is more than a label. Each preset installs its own runtime and CLI in
the guest on first boot (tracked by `/var/lib/hegn/provisioned`):

| Preset | Installs |
| --- | --- |
| `shell` | base CLI tools (git, curl, ripgrep, fd, jq, less) |
| `opencode` | Node.js 22 + `opencode-ai` |
| `claude-code` | Node.js 22 + `@anthropic-ai/claude-code` |
| `codex` | Node.js 22 + `@openai/codex` |
| `pi` | Node.js 22 + `@earendil-works/pi-coding-agent` |
| `prime-agent` | Python + the official Prime Agent installer |
| `aider` | Python + `aider-chat` |
| `gemini-cli` | Node.js 22 + `@google/gemini-cli` |

Provisioning runs as a systemd oneshot on every boot until it succeeds. Node
presets upgrade Jammy's old package to Node.js 22 from NodeSource inside the
guest. Package selections from the create form/`--package` are additive: the
preset can never lose its runtime because a user deselected a box. The install
log appears on the serial console and in the journal; attach and wait for
`hegn: preset ready` on the first boot. Presets that need packages require the
guarded network, because installation happens inside the guest.

### Harness profiles and skill sync

`hegn config init` writes one TOML template per agent harness to
`~/.config/hegn/profiles/` (`opencode.toml`, `prime-agent.toml`,
`claude-code.toml`, …). The files are private (`0600`) because they may contain
API keys. Existing profiles are never overwritten.

```toml
# ~/.config/hegn/profiles/opencode.toml
version = 1
packages = ["tmux", "gh"]              # extra apt packages for this instance

[environment]
OPENAI_API_KEY = "sk-..."              # exported to login shells and the agent

[[files]]                              # copied on every start
source = "~/.config/opencode/opencode.json"
target = "/root/.config/opencode/opencode.json"
mode = "0600"

[[files]]
source = "~/.config/opencode/auth.json"
target = "/root/.config/opencode/auth.json"

[[skills]]                             # copied, never linked
source = "~/.config/opencode/skills"
target = "/root/.config/opencode/skills"
sync_back = true

[[skills]]
source = "~/.agents/skills"
target = "/root/.agents/skills"
```

Both `source` paths and guest `target` paths are validated. Guest targets must
stay under `/root`, `/home`, `/etc`, `/usr/local`, `/opt` or `/workspace`.
Missing configured sources are reported in the instance's `run/hegn.log` and do
not block a start; a default skill directory that does not exist is skipped
silently.

At start hegn materializes the profile into
`<instances_dir>/<name>/profile/bundle/` and exports that **copy** through a
second, sandboxed `virtiofsd` at `/run/hegn/profile`. Configuration files and
environment variables are applied on every boot; skills are seeded on the first
start and then live in the per-instance copy. Package additions are installed on
the next boot without re-running the harness installer. The guest's skill
directory is a link to that copy, not to the host directory, so guest edits
cannot poison the host.

```bash
hegn profile list                       # template paths and state
hegn profile show opencode              # effective profile as TOML
hegn profile path opencode --edit       # edit the template in $EDITOR
hegn profile init                       # write only missing templates

hegn profile sync my-app                # refresh files/env/skills for next boot
hegn skills sync my-app                 # merge host skill changes into the copy
hegn skills sync-back my-app            # copy guest changes back to the host
hegn skills sync-back my-app --dry-run  # show what would change
```

`sync-back` only copies regular files, skips symlinks and special files, and
refuses to run against a live VM unless `--force` is passed. Deleted host files
are never removed by a sync-back.

### Re-attaching like tmux

```bash
hegn attach          # attach to the single running VM
hegn a my-app        # attach, but only if the running VM belongs to my-app
hegn attach --no-bar # give full-screen guest programs the whole terminal
```

`hegn attach` never boots or restarts a VM. It fails if there is no running
instance. With several VMs running, pass the instance name (or a project name that
uniquely identifies the base instance); without an argument it attaches only when
exactly one instance is running. The
bottom line is reserved for a status bar and repainted after every burst of
guest output, so it stays pinned even when a full-screen guest moves the cursor
or resets the scroll region:

```
 hegn ▸ my-app · OpenCode · pid 4242 · log session-…-my-app-4242.log   INPUT DIRECT · Ctrl-B/Ctrl-] detach
```

`Ctrl-B` detaches without stopping the VM. When hegn runs inside tmux (where
`Ctrl-B` is the outer prefix), use `Ctrl-]` instead. Both keys are shown in the
bar. Input is forwarded directly to the
guest UART; the `INPUT DIRECT` marker makes that bypass of `tirith-core`
visible. Use the project terminal for intercepted line input, or `hegn check`
in scripts. The guest console geometry cannot be changed over the legacy
serial port, so the bar uses one host row and full-screen guest programs keep
their own size; pass `--no-bar` (also accepted by `hegn open`) when a guest TUI
needs every row.

### Multiple instances and multi-agent mode

hegn can run several Cloud Hypervisor VMs at the same time. Each instance has its
own directory below `instances_dir`:

```text
~/.local/share/hegn/instances/<name>/
  hegn.toml     per-instance settings
  run/          API/serial/relay/virtiofs sockets, pids, lock
  profile/      copied profile, API keys and skills (never host links)
  rootfs.qcow2  private rootfs copy (created on first start)
  workspace/    only for cloned instances
```

A normal instance exports the original project through virtiofs. A **clone**
(`hegn instance clone`, or `M` on the project screen) gets an independent
git clone and its own rootfs copy, so multiple agents can work on the same
repository without overwriting each other's commits. Merge or open pull requests
from the clones when they are done. Multi-agent clones are intentionally scoped
to the project: open the project in the TUI and press `M` in its terminal. They
then appear in the instance list, which is filtered to the open project, and are
attached with `Enter` like any other instance. Clones are real git clones and contain committed history only: commit
or stash uncommitted work before creating them. Cloning a repository with no
commits yields an empty clone; hegn surfaces git's warning as a toast instead of
letting it corrupt the TUI.

```bash
hegn instance create ~/Projects/hegn/my-app --name my-app
hegn instance start my-app
hegn instance list
hegn instance clone my-app --count 3          # my-app-agent-1..3, booted
hegn instance clone my-app --name review --no-start
hegn instance stop my-app
hegn instance delete my-app-agent-1 --delete-files
```

`hegn instance delete` asks twice: first whether to delete the instance, then
whether to delete its cloned workspace files. A normal instance's workspace is
the original project and is **never** deleted. `--keep-files` unregisters the
instance and keeps the clone under `<instances_dir>/<name>-files-<timestamp>/`.
`--delete-files` removes the clone.

Deleting never boots the VM and runs no host checks (no images, firewall, KVM or
relay required), so an instance whose start failed can always be removed. The
same modal is available from the project screen. With `--yes --keep-files` the
CLI can delete unattended in CI.

On the dashboard, `d` deletes a whole project after showing its git status. It
asks whether to delete the project files too: `y` removes the picker entry and
the project's instances but keeps the files (the entry is hidden from future
scans), while `f` also removes the directory. Imported projects only lose the
symlink in `projects_root`; their original directory is never deleted. The
dashboard drops the project as soon as the deletion finishes. The CLI equivalent
is:

```bash
hegn project delete my-app --keep-files --yes
hegn project delete my-app --delete-files --yes
hegn project restore my-app          # show a kept project again
```

Per-instance settings live in `<instance>/hegn.toml`. In the TUI, `F4` opens the
global settings editor and `F5` opens the selected instance's editor. Both are
interactive modals: use `↑`/`↓` or the mouse to select a row, `←`/`→`, `‹`/`›`
or a click to change a value, and `Enter` to toggle or type. The global editor's
**Profiles** page lists every harness profile; `Enter` creates the template if
needed and opens it in `$EDITOR`. `Ctrl-S` saves,
`r` reloads the file from disk, and `e` (or *Edit file*) opens the raw TOML in
`$EDITOR` when you need comments or keys the editor does not expose. Saving
rewrites the file from the schema, so hand-written comments are not preserved;
the raw editor is the comment-preserving path. From the CLI:

```bash
hegn instance config my-app          # print hegn.toml
hegn instance config my-app --path   # print the path
hegn instance config my-app --edit   # open it in $EDITOR
```

Every instance gets a private rootfs copy on first start (reflink on btrfs/XFS,
otherwise a full copy). This is required for concurrent VMs: sharing one
writable disk would corrupt it. Existing instances keep their copy until they
are deleted; `hegn images download` only refreshes the base image.

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

hegn uses **upstream Cloud Hypervisor images** and does not build its own guest
image. The Cloud Hypervisor Linux kernel and cloud-image URLs are pinned
together with SHA-256 in `src/images.rs` (per architecture) and can be
overridden in `config.toml`. The seed disk generated by hegn enables a serial
root login through cloud-init.

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

The repository's `vm-images` GitHub Actions workflow runs weekly, discovers the
newest upstream kernel and cloud image, recomputes the digests, and opens a
pull request with the updated pins. It does not publish a hegn-owned image.

### Snapshots

`hegn snapshot create` pauses the VM, asks Cloud Hypervisor to write
`config.json`, `state.json` and `memory-ranges` into a private directory, then
resumes it. Cloud Hypervisor snapshots embed device paths (rootfs, serial and
virtio-fs sockets, plus the Landlock snapshot rule). Restore them from the same
`state_dir`/`snapshots_dir`; if those paths move, restore fails closed instead of
booting an inconsistent VM.

```bash
hegn snapshot list
hegn snapshot create before-refactor
hegn snapshot restore before-refactor
hegn snapshot delete before-refactor
```

### Start diagnostics

A failed start leaves diagnostics in the run directory:

```bash
# <instances_dir> defaults to ~/.local/share/hegn/instances
cat ~/.local/share/hegn/instances/<name>/run/hegn.log            # lifecycle + start errors
cat ~/.local/share/hegn/instances/<name>/run/cloud-hypervisor.log # VMM log
cat ~/.local/share/hegn/instances/<name>/run/console-relay.log    # relay stderr
hegn logs list                               # serial logs, including failed starts
hegn logs show <name>                        # VMM API/guest output
```

The CLI error includes the `hegn.log` path and the last lines of the VMM or
relay log, so a boot failure is not reported as a generic relay error. Failed
start serial logs are not deleted, so the Cloud Hypervisor messages that caused
the failure remain available.

### Session logs

Every VM session writes its serial output to
`~/.local/state/hegn/logs/session-<timestamp>-<project>-<pid>.log`. The log is
created private and appended by the detached console relay for the whole VM
lifetime, so stopping the VM does not erase the boot log.

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
is not truncated while the relay owns it; retention is by count (default 50)
and `hegn logs prune` can be run at any time. Pruning always protects the log of
the currently running session.

### "Landlock is not active" / Cloud Hypervisor fails to start

hegn enables Cloud Hypervisor's Landlock sandbox by default. If the host kernel
was built without Landlock (or securityfs is unavailable), the VMM refuses to
start. Run `hegn doctor`: it reports the `landlock` check. Install a kernel with
Landlock enabled; if you must run without this layer, set `landlock = false` in
`config.toml` (explicitly not recommended).

The TUI doctor modal also opens automatically after a start failure so the
remaining host checks and image state are visible.

### Guest has no network ("Temporary failure in name resolution")

A host firewall installed *after* `hegn network setup`, or a `ufw reset`, can
leave its own `forward` rules in place without hegn's TAP exemption. netfilter
runs every `forward` base chain, so the host firewall then drops the traffic
hegn already accepted. Re-run the setup once; it is idempotent and re-adds the
exemption:

```bash
sudo hegn network setup --user "$USER"
hegn doctor            # `network guard` names the firewall it routed through
```

The kernel log shows which rule set dropped a packet:
`hegn-egress-drop` means hegn's own allow-list denied it (ICMP, the LAN, IPv6
and guest→host traffic are blocked by design — test with `curl` or `apt`, not
`ping`), while `[UFW BLOCK]`/`[UFW DOCKER BLOCK]` means the host firewall did.

### Guardrail exit codes

`hegn check` exits `0` for allow, `1` for block and `2` for warn, so it can be
used as a pre-execution hook in scripts and CI.

---

## CLI overview

```bash
hegn [--config PATH] [--json] [COMMAND]

  tui                              Open the interactive TUI (default)
  projects [QUERY]                 List projects under projects_root (alias: ls)
  project create NAME              Scaffold a project (+ AGENTS.md, README, git, hegn.toml)
          [--harness H] [--template PATH] [--package P[,P...]]
  project import PATH [--name N]   Symlink an existing directory into projects_root
  project info PROJECT             Show metadata and git status
  project delete PROJECT [--delete-files | --keep-files] [--yes]
                                   Remove a project (+ its instances); --keep-files hides it
  project restore PROJECT          Show a project removed with --keep-files
  open PROJECT                     Boot/attach the VM serial console
          [--editor | --lazygit] [--no-branch] [--no-bar]
  attach [PROJECT|INSTANCE] [--no-bar]  Attach to a running VM (alias: a)
  logs [list|path|show|prune]      Manage persistent serial logs (alias: log)
  instance list                     List instances (alias: ls)
  instance create PROJECT [--name N] [--harness H] [--clone]
  instance clone SOURCE [--name N] [--count N] [--harness H] [--no-start]
  instance start INSTANCE [--harness H] [--clone]
  instance stop [INSTANCE]
  instance status [INSTANCE]
  instance pause | resume INSTANCE
  instance config INSTANCE [--path | --edit]
  instance delete INSTANCE [--delete-files | --keep-files] [--yes]
  snapshot list                    List snapshots (alias: ls)
  snapshot create [NAME] [--instance INSTANCE]
  snapshot restore NAME [--instance INSTANCE] | delete NAME | show NAME
  check [--cwd PATH] CMD...        Inspect a command with tirith-core
  config show | path | init
  profile list | show HARNESS | path HARNESS [--edit] | init
  profile sync INSTANCE             Refresh an instance's profile payload
  skills sync INSTANCE              Copy host skills into the instance copy
  skills sync-back INSTANCE [--dry-run] [--force]
  images download [--force]        Fetch missing pinned kernel/rootfs images
          [--allow-unverified]
  images status | verify           Show or re-hash the configured images
  doctor                           Check host prerequisites (exit 1 if required checks fail)
  benchmark [PROJECT] [--clean]    Measure host cost; boots a VM on demand
```

Global flags: `--config <PATH>` (or `HEGN_CONFIG`), `--json`.

### Environment variables

`HEGN_CONFIG`, `HEGN_PROJECTS_ROOT`, `HEGN_STATE_DIR`, `HEGN_SNAPSHOTS_DIR`,
`HEGN_TEMPLATES_DIR`, `HEGN_PROFILES_DIR`, `HEGN_INSTANCES_DIR`,
`HEGN_CLOUD_HYPERVISOR_BIN`, `HEGN_VIRTIOFSD_BIN`,
`HEGN_KERNEL_IMAGE`, `HEGN_ROOTFS_IMAGE`, `HEGN_ROOTFS_FORMAT`,
`HEGN_ROOT_DEVICE`, `HEGN_KERNEL_URL`,
`HEGN_KERNEL_SHA256`, `HEGN_ROOTFS_URL`, `HEGN_ROOTFS_SHA256` override the
corresponding config values. Harnesses launched by hegn also receive `HEGN=1`
and `HEGN_WORKSPACE=<path>`.

---

## Configuration

`hegn config init` writes `~/.config/hegn/config.toml` plus one profile
template per agent harness under `~/.config/hegn/profiles/`. Unknown keys are
rejected, so typos surface immediately. Defaults:

```toml
projects_root   = "~/Projects/hegn"
state_dir       = "~/.local/state/hegn"
snapshots_dir   = "~/.local/share/hegn/snapshots"
instances_dir   = "~/.local/share/hegn/instances"
templates_dir   = "~/.config/hegn/templates"
profiles_dir    = "~/.config/hegn/profiles"
cloud_hypervisor_bin = "cloud-hypervisor"
# Bare names are resolved via PATH. cloud-hypervisor-static (AUR) is accepted
# automatically. virtiofsd falls back to /usr/lib/virtiofsd (Arch),
# /usr/libexec/virtiofsd (Fedora) and /usr/lib/qemu/virtiofsd (Debian).
virtiofsd_bin   = "virtiofsd"
kernel_image    = "~/.local/share/hegn/images/vmlinux-x86_64"
rootfs_image    = "~/.local/share/hegn/images/jammy-server-cloudimg-amd64-custom-20241017-0.qcow2"
rootfs_format   = "qcow2"     # "raw" or "qcow2"
root_device     = "/dev/vda1" # use /dev/vda for a whole-disk ext4 image
# The image pin below shows x86_64; aarch64 ships different defaults.
kernel_url      = "https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v7.2.8-20261009/vmlinux-x86_64"
kernel_sha256   = "edfeb8a8a6c04ff2979e9f1c700feaa29b8f564f952261834ef0b7d2e673a853"
rootfs_url      = "https://ch-images.azureedge.net/jammy-server-cloudimg-amd64-custom-20241017-0.qcow2"
rootfs_sha256   = "e05cb642d633e20e8e1390be23856744d3867744764bf3000ad0a9c22dd794e8"
vcpus           = 2          # 1–32
memory_mib      = 2048       # >= 128
shared_tag      = "workspace" # virtio-fs tag, <= 36 bytes, no whitespace
guest_mount     = "/workspace" # absolute guest path
virtiofs_sandbox = "namespace" # "namespace" (recommended) or "chroot"
backend         = "auto"      # "auto" or "cloud-hypervisor"; both need a VM
landlock        = true        # Cloud Hypervisor Landlock sandbox
seccomp         = true        # Cloud Hypervisor seccomp filters
cloud_init      = true        # NoCloud seed for the official cloud image
harness         = "shell"     # shell | opencode | claude-code | codex | pi |
                              # prime-agent | aider | gemini-cli
network         = "guarded"   # "guarded" (web+DNS only) or "off" (no NIC)
extra_packages  = []
auto_branch     = true
security        = "enforce"   # "off", "warn" or "enforce"

# Optional: host-loopback services the guest may reach (local LLM servers).
# The guest URL is http://<tap-ip>:<port>; the host must listen on
# 127.0.0.1:<port>. Re-run `sudo hegn network setup` after changing this list.
# [[local_endpoints]]
# name = "ollama"   # guest env: HEGN_ENDPOINT_OLLAMA (and OLLAMA_HOST)
# port = 11434
```

The package list is a list of *extras*; the harness preset and the packages from
the instance's harness profile are always unioned in.

Project-level defaults live in `hegn.toml` inside each project (written by
`hegn project create`) and record the harness, package list and template used.

### Settings editor

`F4` (global) and `F5` (selected instance) open a modal that edits the same
schema interactively instead of printing the file. Rows are grouped by runtime,
storage, sharing, network/security, tools, directories and packages; the
instance editor also shows its project, workspace, rootfs and TAP slot as
read-only rows. `landlock` and `seccomp` are deliberately not editable — hegn
never weakens the Cloud Hypervisor instances.

| Input | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k`, wheel, click | Select a row |
| `←`/`→`, `h`/`l`, `‹`/`›`, click | Cycle a choice, step a number, toggle a switch |
| `Enter` | Toggle, cycle, start typing a value, or open a profile template |
| `Enter` / `Esc` while typing | Apply / discard the value |
| `Home` / `End`, `PgUp` / `PgDn` | Jump to the first / last row, page through rows |
| `[` / `]` | Switch settings pages (General / Profiles / Local LLM; instance: General) |
| `Ctrl-S` | Validate and save |
| `r` | Reload the file from disk (discard unsaved changes) |
| `e` | Open the raw TOML in `$EDITOR` (requires a clean draft) |
| `Esc` | Close; with unsaved changes, press twice to discard |

Inline edits are validated immediately: numbers are range-checked, paths get
`~` expanded, package names must be valid Debian identifiers, and the same
schema validation that guards `config.toml` runs again before the atomic save.

The **Profiles** page is global (profiles are selected by an instance's harness).
It shows the template path or `(missing)`, creates a missing template on
`Enter`, and returns to the same tab after the editor exits. Profile files can
contain API keys; they stay `0600` in a `0700` directory.

The guarded network is a one-time privileged host setup:

```bash
sudo hegn network setup --user "$USER"   # TAP pool + nftables table + /run marker
hegn network status                      # marker, taps and reservations
hegn network setup --dry-run --user "$USER"  # inspect the exact script
sudo hegn network teardown               # remove taps and rules
```

Setup also exempts the TAP pool from the active host firewall (ufw `route`
rules, firewalld direct rules, or `FORWARD` accepts for plain iptables), because
a firewall with its own routed drop policy would otherwise discard the traffic
hegn allowed. The exemption is a blanket per-interface accept in *that* firewall
only; the HTTP(S)/DNS allow-list stays in hegn's table, so the guest's reach is
unchanged. Re-run setup after installing, enabling or resetting a host firewall,
and after upgrading from a hegn release whose marker is a previous version.

When hegn invokes `sudo` or `pkexec` itself (`hegn doctor --fix`, the doctor's
`Set up everything` action), it prints the exact command line with shell quoting
before the password prompt, so it can be inspected first.

`hegn instance start` is fail-closed: with `network = "guarded"` it refuses to
boot unless `/run/hegn/network.json` exists and was written by root. Rules live
on the host, so root in the guest cannot remove them.

### Local LLM endpoints

Ollama, LM Studio, llama.cpp and vLLM usually listen on the host's loopback.
Inside the guest, `127.0.0.1` is the guest itself, so hegn exposes selected host
ports on the instance's own TAP host address:

```toml
[[local_endpoints]]
name = "ollama"   # guest env: HEGN_ENDPOINT_OLLAMA (plus OLLAMA_HOST)
port = 11434      # host target: 127.0.0.1:11434
```

`hegn network setup` installs a DNAT rule per TAP plus one narrowly scoped
input accept. The guest uses `http://10.213.0.<n>:11434`; host loopback is never
bridged, other host interfaces stay unreachable, and ports below 1024 are
rejected. Endpoints require `network = "guarded"`. After editing the list,
re-run `sudo hegn network setup` — `hegn instance start` fails closed with a hint
until the root-owned marker matches. With `cloud_init = true`, the URLs are
exported in the guest through `/etc/profile.d/hegn-endpoints.sh`; every endpoint
is also available as `HEGN_ENDPOINT_<NAME>`. `hegn instance status` prints the
guest URLs.

### AGENTS.md templates

Place templates in `~/.config/hegn/templates/`. `hegn project create` copies
`~/.config/hegn/templates/AGENTS.md` automatically when it exists; pass
`--template <PATH>` to use a different file.

---

## TUI keybindings

### Dashboard

The dashboard is a project picker plus a compact preview of the selected
project. It never boots a VM.

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | Move selection |
| `Enter` / `o` | Open the project screen (does not boot) |
| `/` | Fuzzy filter projects |
| `b` / `2` | Open the project screen on instances (if a project is selected) |
| `s` / `3` | Open the project screen on snapshots (if a project is selected) |
| `c` | Create project |
| `h` / `←` | Import an existing directory (symlink, no copy) |
| `d` | Delete project: shows git status, then asks whether to delete its files |
| `F4` | Global settings editor (mouse supported, `Ctrl-S` to save) |
| `e` | Open selected project in `$EDITOR` |
| `g` | Open selected project in lazygit |
| `r` | Refresh projects and instances |
| `F3` | Doctor modal (host checks, image download) |
| `?` / `F1` | Help |
| `q` | Quit |

### Project screen

The folder-style title bar shows `hegn ▸ <project>`, the path, and the VM
state. The project page is composed of four numbered sections: `1` instances,
`2` snapshots, `3` health, `4` git. The terminal is a one-line strip until you
open it.

| Key | Action |
| --- | --- |
| `1` / `2` / `3` / `4` | Instances / snapshots / health / git |
| `Tab` | Cycle project sections |
| `↑`/`↓`, `j`/`k` | Move inside the focused section |
| `Enter` (instances) | Attach the selected instance, or boot the default one |
| `Enter` (snapshots) | Restore the selected snapshot |
| `i` / `a` / `t` | Open/focus the terminal (starts an instance if needed) |
| `Esc` | Insert → normal; normal → section headers (pane stays at 55%) |
| `←` | Back to the dashboard (the VM keeps running) |
| `F2` | Leave the TUI for the real `hegn attach` client |
| `Ctrl-B` / `Ctrl-]` | Back to the dashboard from every mode |
| `n` | Start a new instance (instances) / new snapshot (snapshots) |
| `M` | Multi-agent clones for this project |
| `S` | Stop the selected instance |
| `d` | Delete the selected instance or snapshot |
| `F5` | Instance settings editor |
| `e` / `g` | Editor / lazygit for the project |
| `F4` | Global settings editor |
| `?` / `F1` | Help |

Opening the terminal moves it to the right at 55%. Pressing a section number
expands that section and shrinks the terminal to 30%; all four sections stay
visible, with the unselected ones collapsed to a compact header row. Focusing
the terminal again restores 55%. The collapsed strip shows the selected instance
and whether it is running.

Destructive actions are never confirmed with `Enter`. Deleting an instance,
snapshot or project and allowing a flagged command require the explicit `y`
(or `a` for the security prompt) key; `Enter` leaves the confirmation modal open.
The project delete modal shows the project's branch, sync state and changed-file
counts first. `y` removes the project from the dashboard and keeps its files;
`f` also deletes the project directory. Imported projects only lose their picker
symlink — the original directory is never touched.

Every keybinding hint inside a modal is rendered as a clickable button, just
like the footer buttons. Hovering one inverts its colors for immediate feedback;
keyboard input clears the hover state.

### Help modal

Open it with `?` or `F1`. `/` starts a case-insensitive search across section
titles, keys and descriptions; every typed term must match. `Esc` stops
searching and keeps the results, the next `Esc` clears the query, and a final
`Esc` closes the modal.

On narrow terminals the key column collapses: the key sits on its own line and
the description below it. When the list is taller than the window a scrollbar
appears on the right; drag it with the mouse or use `↑`/`↓`, `j`/`k` and
`PgUp`/`PgDn`. Hovering the thumb inverts it, like every other clickable
element.

| Key | Action |
| --- | --- |
| `/` | Search the keybinding list |
| `Esc` | Stop searching, then clear the query, then close |
| `↑`/`↓`, `j`/`k` | Scroll the list |
| `PgUp` / `PgDn` | Scroll one page |

The doctor and security modals scroll the same way (mouse wheel and drag
always work, in every modal that overflows). All modals size themselves to the
terminal and keep their footer buttons inside the window, wrapping them onto
another row when necessary.

### Snapshot panel

| Key | Action |
| --- | --- |
| `Enter` | Restore selected snapshot |
| `n` | Create a snapshot |
| `d` | Delete selected snapshot |

### Terminal (guest serial console)

The terminal is a pane inside the project screen. It opens in **normal** mode,
where keys are hegn shortcuts and nothing reaches the guest. Press `i` or `a`
to enter **insert** mode and type commands; `Esc` returns to normal and then to
the section headers while the pane stays open.

| Key | Action |
| --- | --- |
| `i` / `a` / `t` | Open or focus the terminal and enter insert mode |
| `Esc` | Insert → normal; normal → section headers |
| `←` | Return to the dashboard (the VM keeps running) |
| `M` | Multi-agent clones for the open project |
| `Enter` | In insert mode: inspect the command with tirith-core, then send it |
| `F2` | Leave the TUI for the real `hegn attach` client |
| `Ctrl-B` / `Ctrl-]` | Return to the dashboard from every mode |
| `PgUp` / `PgDn` | Scroll output |
| `Ctrl-L` | Clear scrollback |

The output pane shows the live scrollback. Serial CRLF endings are handled
correctly, so every line the guest printed remains visible; `PgUp` scrolls back
through it.

### Doctor modal

| Key | Action |
| --- | --- |
| `s` | Set up everything shown as missing (after a confirmation) |
| `d` | Download and verify missing VM images (background) |
| `r` | Re-run all checks |
| `Esc` / `q` | Close |

The doctor opens automatically on the first TUI launch when setup is available
or a required check fails, and after a VM start/attach failure. `s` is the single setup entry point:
it creates missing directories and config, downloads the pinned images, and
applies the guarded network through `sudo`/`pkexec` when needed. Every step is
idempotent. Image downloads run in the background; while they stream, a
notification in the bottom-right corner shows the asset, a progress bar and the
percentage (or the received MiB when the server does not advertise a length). The same workflow is available non-interactively as
`hegn doctor --fix`. Optional tools (lazygit, `$EDITOR`) never make the report
fail and are not installed automatically.

### Security modal

| Key | Action |
| --- | --- |
| `a` | Allow flagged command once |
| `c` | Create a git checkpoint, then allow |
| `d` / `Esc` | Deny command |

`Enter` never allows a flagged command; it leaves the prompt open. Every action
is clickable both as a button inside the modal and in the footer. Hovering a
button with the mouse inverts its colors for immediate feedback; keyboard input
clears the hover state.

### Theming

The TUI uses your terminal's own foreground/background and the ANSI color
palette. hegn does not paint a custom full-screen background, so existing
terminal themes (dark or light) apply unchanged. Only selected rows, badges and
diagnostic cells use a small ANSI background color.

---

## Security model

1. **Isolation** — commands execute inside a Cloud Hypervisor VM, not on the host. The
   workspace is shared read-write through a per-project `virtiofsd` that is
   sandboxed and canonicalizes its shared directory so symlinks cannot widen
   the export. If no VM can be started, hegn refuses to launch work.
2. **VMM hardening** — Cloud Hypervisor runs with seccomp filters and Landlock
   filesystem sandboxing. The VMM gets exactly the kernel, disks, serial and
   virtio-fs sockets, plus a staging directory it must write snapshots into.
   `virtiofsd` runs with `--sandbox=namespace` (or `chroot`) and
   `--cache=never`. All control and console sockets live in a `0700` run
   directory; the relay socket is `0600`. A finished snapshot is moved out of
   the VMM's staging directory and its files are tightened to `0600`, so the
   VMM never has write access to completed checkpoints.
3. **Console access** — the official cloud image is bootstrapped with a private
   NoCloud seed that enables an automatic root login on the serial console. The
   serial and relay sockets are only reachable by the owning user.
4. **Network guard** — with `network = "guarded"`, each VM gets a dedicated TAP
   from a host-created pool. Host nftables rules allow only outbound TCP 80/443
   and DNS to `1.1.1.1`/`9.9.9.9`; RFC1918, CGNAT, link-local, multicast,
   anti-spoofed sources and all IPv6 are dropped, and all guest→host traffic is
   dropped. The guest's only route is the host `/30` gateway, and NAT exposes
   nothing inbound. Rules are host-side, so guest root cannot disable them. If
   the host runs a firewall that also filters `forward` (ufw, firewalld, plain
   iptables), setup exempts the TAP pool from that firewall too — a blanket
   per-interface accept there, while hegn's table remains the only allow-list.
   `network = "off"` removes the NIC entirely.
5. **Interception** — in the project terminal, input is buffered into a command
   line and passed to `tirith-core` plus hegn's own workspace guardrails before
   any bytes reach the guest serial port.
6. **Policy** — `enforce` blocks high/critical findings and prompts for medium
   ones; `warn` reports everything; `off` disables `tirith-core` but keeps
   hegn's destructive-workspace checks.
7. **Recovery** — a blocked-but-allowed command can be preceded by a git
   checkpoint, and VM snapshots allow instant rollback beyond git.

No instance is a substitute for reviewing what you run. `F2` and `hegn attach`
hand the terminal directly to the guest and mark this as `INPUT DIRECT`; use
`hegn check` for scripted pre-execution checks or the in-TUI terminal for
intercepted input.

The default Cloud Hypervisor cloud image is writable and reused between
sessions, so guest changes to the root filesystem persist. The workspace is the
only project directory exposed through virtio-fs; a second share exposes only
the instance's private profile/skill copy. If you need a truly ephemeral
root, point `rootfs_image` at a per-session copy or an overlay image that you
create and dispose of outside hegn.

Persistent serial logs are treated as untrusted data. They live in a private
directory, are opened without following symlinks, and `hegn logs show` strips
terminal escape sequences by default. `--raw` exists for debugging and should
only be used with logs you trust.

Harness profiles are host-owned input, but they are applied as root inside the
guest. hegn therefore validates package names, environment names/values, modes
and guest targets before writing the apply script, and the profile bundle is
exported from a private per-instance directory through its own sandboxed
`virtiofsd`. Skills are copied into that directory instead of mounting the
host's real skill directories, so a compromised guest only ever edits the
throw-away copy. `hegn skills sync-back` is an explicit, copy-only operation
that skips symlinks and special files.

VM images are upstream Cloud Hypervisor artifacts pinned by HTTPS URL and SHA-256.
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
  profile.rs     per-harness profiles, copied skill bundles and sync-back
  security.rs    tirith-core integration + workspace guardrails
  cloud_hypervisor.rs typed Cloud Hypervisor REST client
  virtiofs.rs    hardened virtiofsd argv/process construction
  instance.rs     VM lifecycle orchestration and status reporting
  snapshot.rs    snapshot metadata store (RAM + CPU directory)
  images.rs      pinned upstream VM assets, download, SHA-256 verification
  doctor.rs      shared host prerequisite checks (CLI + TUI modal)
  logs.rs        persistent serial logs, private files, tail and pruning
  cloud_init.rs  NoCloud seed for official cloud images
  relay.rs       detached serial relay + persistent log
  console.rs     serial console transport + tmux-style attach bar
  attach_bar.rs  pinned status bar overlay + sequence-safe output gate
  terminal.rs    ANSI-aware scrollback buffer for the serial console
  state.rs       persisted recent-project state
  app.rs         TUI state machine, key handling, modals, effects
  ui.rs          ratatui rendering
  tui.rs         terminal lifecycle and event loop
```

`vendor/tirith-core/` is a local copy of the newest upstream release with only
its vulnerable transitive dependencies patched; see
`vendor/tirith-core/PATCHES.md`. The crate is a library (`lib.rs`) with a thin
binary, so every module is unit tested directly. See [`docs/architecture.md`](docs/architecture.md) for the
detailed architecture, data flows, safety invariants, and image-update
pipeline.

---

## Dependencies

| Crate | Version | Used for |
| --- | --- | --- |
| `ratatui` | 0.30 | TUI widgets and layout |
| `crossterm` | 0.29 | Terminal backend, input events, raw mode |
| `tokio` | 1 (full) | Async runtime for Cloud Hypervisor API calls and snapshots |
| `clap` | 4 (derive, env) | CLI parsing and help |
| `serde` | 1 (derive) | Config, metadata and API serialization |
| `serde_json` | 1 | Cloud Hypervisor REST payloads and `--json` output |
| `toml` | 0.9 | `config.toml` and `hegn.toml` |
| `tirith-core` | 0.4.2 | In-memory command inspection and policy engine |
| `anyhow` | 1 | Context-rich application errors |
| `thiserror` | 2 | Typed errors in library-facing modules |
| `chrono` | 0.4 (serde) | Snapshot/session timestamps |
| `dirs` | 6 | XDG config/state/data directories |
| `libc` | 0.2 | Unix process and permission primitives |
| `unicode-width` | 0.2 | Correct terminal column widths for the TUI |

Dev-dependencies: `tempfile` (3) for isolated config/state/snapshot fixtures.

Runtime tools (not Rust dependencies): `cloud-hypervisor` or
`cloud-hypervisor-static`, `virtiofsd`, `git`,
and optionally `lazygit` and `$EDITOR`.

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

Licensed under GPLv3. Contributions are accepted under the same terms.

---

## Contributions

Contributions are always welcome! If you have any ideas for new features, improvements, or bug fixes, feel free to open an issue or submit a pull request.
