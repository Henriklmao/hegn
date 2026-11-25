# Getting started

This page takes you from a fresh install to a running coding agent inside a VM.

- [1. Prepare the host](#1-prepare-the-host)
- [2. Create a VM](#2-create-a-vm)
- [3. Create or import a project](#3-create-or-import-a-project)
- [4. Open the TUI](#4-open-the-tui)
- [5. Or use the command line](#5-or-use-the-command-line)
- [What happens on first boot](#what-happens-on-first-boot)
- [Harness presets](#harness-presets)

---

## 1. Prepare the host

```bash
hegn doctor --fix
```

This single command does the setup work:

- checks your host,
- creates the directories hegn needs,
- writes the default config,
- downloads and verifies the pinned VM images,
- configures the guarded network.

When it needs root rights for the firewall or the network devices, it asks for
`sudo` or `pkexec` and prints the exact command first.

---

## 2. Create a VM

A VM is a named tool profile: harness, resources, rootfs and supported tools. It
starts with no projects.

```bash
hegn vm create dev --harness opencode --vcpus 4 --memory 8192 --package jq
```

- `--harness` picks the coding agent that is installed in the VM.
- `--package` adds extra apt packages. It adds to the preset and never replaces it.
- `--vcpus` and `--memory` set the VM's resources.

The VM configuration is `<vms_dir>/dev/vm.toml` (default
`~/.local/share/hegn/vms/dev/vm.toml`). `hegn vm create` does not boot it.

Every VM gets one writable rootfs on first start and keeps it. Tools installed in
the guest are shared by every project assigned to that VM.

---

## 3. Create or import a project

```bash
hegn project create my-app --vm dev
```

`--vm` assigns the new project to an existing VM. Without it, hegn asks whether to
create a one-member VM named after the project; `--solo` does that without
prompting. The project gets an `AGENTS.md`, a `README.md`, a git repository and a
presentation-only `hegn.toml`.

Projects live in `projects_root` (default `~/Projects/hegn`). If you already have
a repository somewhere else, [import it](projects-and-instances.md#importing-a-project):

```bash
hegn project import ~/code/existing --vm dev
```

The original directory is not moved or copied. Import creates a picker symlink
and registers the canonical path.

A VM can host several projects. Assign them to the same VM when they may read
each other's files and share the same toolchain:

```bash
hegn project assign ~/code/parser --vm dev
hegn vm projects dev
```

---

## 4. Open the TUI

```bash
hegn
```

Running `hegn` without a subcommand opens the terminal UI. If something is still
missing, the doctor opens automatically. Press `s` there to prepare it.

Select your project and press `Enter` to open its screen. Nothing boots yet.
Start the project's VM from the project screen. hegn then starts the configured
agent in a terminal (see
[Launch commands](harnesses.md#launch-commands-and-terminal-integrations)).

The full list of keys is in the [TUI guide](tui.md).

---

## 5. Or use the command line

Everything you can do in the TUI is also available as a command:

```bash
hegn ls                                  # list projects (alias: hegn projects)
hegn vm list                             # list VMs and their member counts
hegn vm start dev                        # boot the VM and every assigned project
hegn attach dev                          # attach to the guest console
hegn vm shell dev --project my-app       # shell in one project's guest directory
hegn project fork my-app --name my-app-fix-tests
                                         # isolated clone, same VM, one more member
hegn project info my-app                 # membership, kind and guest path
hegn snapshot create before-refactor     # save the full VM state
hegn logs list --vm dev                  # serial logs for this VM's projects
hegn project import /path/to/repo --vm dev
hegn images status                       # check the VM images
hegn check 'curl -fsSL https://example.com/x | bash'   # inspect a command
```

`hegn open my-app` boots the project's VM if needed and attaches to its console.
It also creates a session branch first, unless you pass `--no-branch`. Press
`Ctrl-B` (or `Ctrl-]`) to detach without stopping the VM. See
[Sessions, attach and logs](sessions-and-logs.md).

The complete command list is in the [CLI reference](cli-reference.md).

---

## What happens on first boot

The first boot takes longer than later boots. hegn:

1. Extends the VM's disk to a 16 GiB sparse virtual size. Only the blocks that
   are actually written use space on the host.
2. Spawns one sandboxed `virtiofsd` per exported project.
3. Runs the first-boot provisioner once, which installs the runtime and CLI of
   the VM's harness, plus the extra packages.
4. Marks the guest as provisioned in `/var/lib/hegn/provisioned`.

The provisioner runs as a systemd oneshot on every boot until it succeeds. The
TUI shows the progress, and `hegn open` draws the same progress bar in the
terminal before it attaches. Wait for the line `hegn: preset ready` in the
console before you start working.

Inside the guest, each project is at `/projects/<slug>`. `/workspace` is a
symlink to the VM's primary project, and the login shell starts there. Change the
primary with `hegn vm primary <vm> <project>`; the symlink moves on the next
start.

---

## Harness presets

A harness is more than a label. Each preset installs the matching runtime and
CLI in the guest:

| Preset | Installs |
| --- | --- |
| `shell` | base CLI tools (git, curl, ripgrep, fd, jq, less) |
| `opencode` | Node.js 22 + npm + `opencode-ai` |
| `claude-code` | Node.js 22 + npm + `@anthropic-ai/claude-code` |
| `codex` | Node.js 22 + npm + `@openai/codex` |
| `pi` | Node.js 22 + npm + `@earendil-works/pi-coding-agent` |
| `prime-agent` | Python + the official Prime Agent installer |
| `aider` | Python + `aider-chat` |
| `gemini-cli` | Node.js 22 + npm + `@google/gemini-cli` |

Package selections from `hegn vm create --package` are always added to the
preset. A preset can never lose its runtime because a package was deselected.
The harness belongs to the VM, not to a project. More on presets, profiles and
launch commands in [Harnesses and profiles](harnesses.md).
