# hegn documentation

This is the full documentation for hegn. The [project README](../README.md) has
the short version.

## User guides

| Document | What's in it |
| --- | --- |
| [Installation](installation.md) | Requirements, install methods, upgrading |
| [Getting started](getting-started.md) | First VM, first project, harness presets |
| [Projects, VMs and workspaces](projects-and-vms.md) | Membership, imports, forks, deleting, read-only exports |
| [Harnesses and profiles](harnesses.md) | Harness presets, profiles, skill sync, launch commands |
| [Sessions, attach and logs](sessions-and-logs.md) | SSH sessions, the serial console, session logs |
| [Configuration](configuration.md) | `config.toml`, `vm.toml`, project `hegn.toml`, environment variables |
| [TUI guide](tui.md) | Screens and keyboard shortcuts |
| [CLI reference](cli-reference.md) | All commands, flags and exit codes |
| [Snapshots](snapshots.md) | Manual snapshots, boot fingerprints and automatic resume |
| [Networking](networking.md) | Control TAP for SSH, guarded network, local LLM endpoints, host firewalls |
| [VM images](vm-images.md) | Pinned images, verification, guest login |
| [Security](security.md) | Isolation, guardrails, limits |
| [Troubleshooting](troubleshooting.md) | Common problems and log locations |
| [Resource usage](resources.md) | Memory, disk and CPU cost |
| [Migrating to v2](migration-v2.md) | Hard cut from one-project-per-VM, legacy state |
| [LLM setup guide](llm-setup.md) | Installation steps for coding agents |

## Developer documentation

| Document | What's in it |
| --- | --- |
| [Architecture](architecture.md) | Design goals, modules, lifecycle flows, safety invariants, CI |
| [Multi-project VMs](design/multi-project-vms.md) | Design record for many projects per VM, SSH sessions, clones/worktrees, snapshots and migration |

## Features in detail

- **Project, VM, workspace:** a project is a host directory. A VM is a named tool
  profile and runtime that hosts zero or more projects. A workspace is the
  concrete checkout exported for a project: the project itself, a clone, or a
  worktree. See [Projects, VMs and workspaces](projects-and-vms.md).
- **Isolation:** each VM runs in Cloud Hypervisor with its own kernel and root
  filesystem. The hypervisor runs with seccomp and Landlock. See
  [Security](security.md).
- **One export per project:** a sandboxed `virtiofsd` shares each project
  separately. Projects are mounted at `/projects/<slug>`; `/workspace` is a
  guest-side symlink to the VM's primary project. Never export a parent
  directory to get more projects.
- **Membership is private:** assignments, read-only flags and fork lineage live
  in `<state_dir>/projects.toml`, outside every exported directory. A repo-local
  `hegn.toml` only carries launch and presentation preferences.
- **Multiple VMs:** VMs are independent. Each has its own settings, sockets,
  private rootfs and snapshots. Session logs are shared and can be filtered per
  VM. A project belongs to at most one VM.
- **Parallel workspaces:** independent git clones are the default fork. Nested
  git worktrees are opt-in and share git state with their parent.
- **Harness profiles and skills:** API keys, files and skills are copied into
  each VM. See [Harnesses and profiles](harnesses.md#harness-profiles).
- **Settings editor:** `F4` (global), `F5` (VM) and `F6` (project) open
  interactive editors. See [Configuration](configuration.md#settings-editor).
- **Snapshots:** save and restore the full VM state, plus automatic checkpoints.
  A boot fingerprint invalidates membership or resource changes. See
  [Snapshots](snapshots.md).
- **SSH by default:** every VM has a control TAP for SSH, with one guest PTY per
  session. The serial console remains the boot and recovery channel
  (`--serial`). The control link has no egress. See
  [Sessions, attach and logs](sessions-and-logs.md) and
  [Networking](networking.md).
- **Guarded network:** the data TAP reaches only the public web and DNS. See
  [Networking](networking.md).
- **Command guardrails:** checks for homoglyphs, secret exfiltration, encoded
  payloads and destructive operations. Every `/projects/*` root and `/workspace`
  is treated as workspace. See [Security](security.md#command-guardrails).
- **Project picker:** fuzzy filter, git status and VM badges, session history,
  and one-key launch into `$EDITOR` or `lazygit`. See [TUI guide](tui.md).
- **Project import and scaffolding:** import existing repositories by symlink, or
  create a project with `AGENTS.md`, `README.md`, git and a VM assignment. See
  [Projects, VMs and workspaces](projects-and-vms.md#importing-a-project).
- **Pinned VM images:** the official upstream kernel and cloud image, verified with
  SHA-256. See [VM images](vm-images.md).
- **Machine-readable output:** `--json` for most commands, plus meaningful exit
  codes. See [CLI reference](cli-reference.md).
