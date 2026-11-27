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

- checks your host, including the OpenSSH client (`ssh`, `ssh-keygen`),
- repairs root-owned or unreadable state left by an earlier `sudo` run,
- creates the directories hegn needs,
- writes the default config,
- downloads and verifies the pinned VM images,
- configures the control network for SSH and the guarded data network.

When it needs root rights for the firewall or the network devices, it asks for
`sudo` or `pkexec` and prints the exact command first.

---

## 2. Create a VM

A VM is a named tool profile: a harness, resources and a private rootfs. It starts
with no projects.

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
prompting. The project gets an `AGENTS.md`, a `README.md` and a git repository. A
repo-local `hegn.toml` is optional; it is written when you edit the project's
launch, integration or auto-start preferences.

Projects live in `projects_root` (default `~/Projects/hegn`). If you already have
a repository somewhere else, [import it](projects-and-vms.md#importing-a-project):

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
hegn projects                            # list projects with VM and primary markers (alias: ls)
hegn vm list                             # list VMs and their member counts
hegn vm start dev                        # boot the VM and every assigned project
hegn attach dev                          # SSH session into the guest
hegn attach dev --serial                 # serial console (boot/recovery)
hegn vm shell dev --project my-app       # SSH shell in one project's guest directory
hegn attach dev --command 'git status'   # one-off SSH exec in the project's guest dir
hegn project fork my-app --name my-app-fix-tests
                                         # isolated clone, same VM, one more member
hegn project info my-app                 # membership, kind and guest path
hegn snapshot create before-refactor --vm dev  # save the full VM state
hegn logs list --vm dev                  # serial logs for this VM's projects
hegn project import /path/to/repo --vm dev
hegn images status                       # check the VM images
hegn check 'curl -fsSL https://example.com/x | bash'   # inspect a command
```

`hegn open my-app` boots the project's VM if needed, waits for provisioning and
SSH readiness, and opens an SSH session. Once the VM has booted, it also checks
out a session branch in the project, unless you pass `--no-branch` or set
`auto_branch = false`. `exit`/`Ctrl-D` ends the SSH session; the VM keeps
running. `hegn attach my-app --serial` shows the serial console, where
`Ctrl-B` (or `Ctrl-]`) detaches. Several SSH sessions can run in parallel. See
[Sessions, attach and logs](sessions-and-logs.md).

The complete command list is in the [CLI reference](cli-reference.md).

---

## What happens on first boot

The first boot takes longer than later boots. hegn:

1. Extends the VM's disk to a 16 GiB sparse virtual size. Only the blocks that
   are actually written use space on the host.
2. Creates the per-VM SSH client key on the host and attaches the always-present
   control TAP for SSH.
3. Spawns one sandboxed `virtiofsd` per exported project.
4. Configures the guest control interface, sshd and the authorized key, and
   generates fresh guest host keys. cloud-init runs once more whenever the seed
   changes.
5. Runs the first-boot provisioner, which installs the runtime and CLI of the
   VM's harness, plus the extra packages.
6. Marks the guest as provisioned in `/var/lib/hegn/provisioned`, tagged with
   the cloud-init instance id. The provisioner runs again whenever that id
   changes (for example after `hegn vm apply`); an unchanged seed stays
   idempotent.

The TUI shows the progress, and `hegn open` waits for the provisioner stages in
the serial log before it probes SSH readiness. `hegn vm apply` waits for
cloud-init to finish, SSH to accept the new guest host key, and the harness
provisioner's ready marker before it returns, then clears `known_hosts` once
more so the next attach accepts the current key. A plain `hegn attach` requires
the VM to be running and sshd to be ready. Wait for the line
`hegn: preset ready` in the serial console before you start working.

The guest is configured once more after upgrading because the seed version and
instance id change. `hegn vm apply <vm>` regenerates the seed; a reboot inside
the guest alone does not.

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
