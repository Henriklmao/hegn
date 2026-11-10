# hegn documentation

This is the full documentation for hegn. The [project README](../README.md) has
the short version.

## User guides

| Document | What's in it |
| --- | --- |
| [Installation](installation.md) | Requirements, install methods, upgrading |
| [Getting started](getting-started.md) | First project, first VM, harness presets |
| [Projects and instances](projects-and-instances.md) | Projects, imports, clones, deleting, read-only mode |
| [Harnesses and profiles](harnesses.md) | Harness presets, profiles, skill sync, launch commands |
| [Sessions, attach and logs](sessions-and-logs.md) | Attaching, the attach bar, session logs |
| [Configuration](configuration.md) | `config.toml`, `hegn.toml`, settings editor, environment variables |
| [TUI guide](tui.md) | Screens and keyboard shortcuts |
| [CLI reference](cli-reference.md) | All commands, flags and exit codes |
| [Snapshots](snapshots.md) | Manual snapshots and automatic resume |
| [Networking](networking.md) | Guarded network, local LLM endpoints, host firewalls |
| [VM images](vm-images.md) | Pinned images, verification, guest login |
| [Security](security.md) | Isolation, guardrails, limits |
| [Troubleshooting](troubleshooting.md) | Common problems and log locations |
| [Resource usage](resources.md) | Memory, disk and CPU cost |
| [LLM setup guide](llm-setup.md) | Installation steps for coding agents |

## Developer documentation

| Document | What's in it |
| --- | --- |
| [Architecture](architecture.md) | Design goals, modules, lifecycle flows, safety invariants, CI |

## Features in detail

- **Isolation:** each project runs in a Cloud Hypervisor VM with its own kernel
  and root filesystem. The hypervisor runs with seccomp and Landlock. See
  [Security](security.md).
- **Shared workspace:** a sandboxed `virtiofsd` shares only the selected project.
  The workspace is mounted at `/workspace`, or at `guest_mount` if you set one.
- **Multiple instances:** every instance has its own settings, sockets, log and
  private rootfs copy. See [Projects and instances](projects-and-instances.md).
- **Multi-agent clones:** independent git clones let parallel agents work without
  overwriting each other. See [Projects and instances](projects-and-instances.md#multi-agent-clones).
- **Harness profiles and skills:** API keys, files and skills are copied into each
  VM. See [Harnesses and profiles](harnesses.md#harness-profiles).
- **Settings editor:** `F4` (global), `F5` (instance) and `F6` (project) open
  interactive editors. See [Configuration](configuration.md#settings-editor).
- **Snapshots:** save and restore the full VM state, plus automatic checkpoints.
  See [Snapshots](snapshots.md).
- **Guarded network:** the guest reaches only the public web and DNS. See
  [Networking](networking.md).
- **Command guardrails:** checks for homoglyphs, secret exfiltration, encoded
  payloads and destructive operations. See [Security](security.md#command-guardrails).
- **Project picker:** fuzzy filter, git status badges, session history, and
  one-key launch into `$EDITOR` or `lazygit`. See [TUI guide](tui.md).
- **Project import and scaffolding:** import existing repositories by symlink, or
  create a project with `AGENTS.md`, `README.md`, git and a harness. See
  [Projects and instances](projects-and-instances.md#importing-a-project).
- **Pinned VM images:** the official upstream kernel and cloud image, verified with
  SHA-256. See [VM images](vm-images.md).
- **Machine-readable output:** `--json` for most commands, plus meaningful exit
  codes. See [CLI reference](cli-reference.md).
