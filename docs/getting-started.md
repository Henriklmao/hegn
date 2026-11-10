# Getting started

This page takes you from a fresh install to a running coding agent inside a VM.

- [1. Prepare the host](#1-prepare-the-host)
- [2. Create a project](#2-create-a-project)
- [3. Open the TUI](#3-open-the-tui)
- [4. Or use the command line](#4-or-use-the-command-line)
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

## 2. Create a project

```bash
hegn project create my-app --harness opencode --package jq
```

- `--harness` picks the coding agent that is installed in the VM.
- `--package` adds extra apt packages. It adds to the preset and never replaces it.

The new project gets an `AGENTS.md`, a `README.md`, a git repository and a
`hegn.toml` that records the harness and the packages.

Projects live in `projects_root` (default `~/Projects/hegn`). If you already have
a repository somewhere else, you can [import it](projects-and-instances.md#importing-a-project)
instead.

---

## 3. Open the TUI

```bash
hegn
```

Running `hegn` without a subcommand opens the terminal UI. If something is still
missing, the doctor opens automatically. Press `s` there to prepare it.

Select your project and press `Enter` to open its screen. Nothing boots yet. In
the instances section, press `Enter` again to boot the VM. hegn then starts the
configured agent in a terminal (see
[Launch commands](harnesses.md#launch-commands-and-terminal-integrations)).

The full list of keys is in the [TUI guide](tui.md).

---

## 4. Or use the command line

Everything you can do in the TUI is also available as a command:

```bash
hegn ls                                  # list projects (alias: hegn projects)
hegn instance list                       # list instances
hegn instance start my-app               # boot the VM
hegn attach my-app                       # attach to the guest console
hegn instance clone my-app --count 2     # create two independent agent clones
hegn instance status my-app --json       # machine-readable status
hegn snapshot create before-refactor     # save the full VM state
hegn logs                                # list session logs
hegn project import /path/to/repo        # register an existing repository
hegn images status                       # check the VM images
hegn check 'curl -fsSL https://example.com/x | bash'   # inspect a command
```

`hegn open my-app` boots the VM if needed and attaches to its console. It also
creates a session branch first, unless you pass `--no-branch`. Press `Ctrl-B`
(or `Ctrl-]`) to detach without stopping the VM. See
[Sessions, attach and logs](sessions-and-logs.md).

The complete command list is in the [CLI reference](cli-reference.md).

---

## What happens on first boot

The first boot takes longer than later boots. hegn:

1. Extends the instance's disk to a 16 GiB sparse virtual size. Only the blocks
   that are actually written use space on the host.
2. Runs the first-boot provisioner, which installs the runtime and CLI of the
   chosen harness, plus the extra packages.
3. Marks the guest as provisioned in `/var/lib/hegn/provisioned`.

The provisioner runs as a systemd oneshot on every boot until it succeeds. The
TUI shows the progress, and `hegn open` draws the same progress bar in the
terminal before it attaches. Wait for the line `hegn: preset ready` in the
console before you start working.

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

Package selections from the create form or `--package` are always added to the
preset. A preset can never lose its runtime because a package was deselected.
More on presets, profiles and launch commands in [Harnesses and profiles](harnesses.md).
