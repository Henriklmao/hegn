<div align="center">

<!-- Platzhalter: Icon unter docs/assets/icon.png ablegen -->
<img src="docs/assets/icon.png" width="128" alt="hegn icon">

# hegn

**Run your coding agents in their own virtual machines.**

[![CI](https://github.com/Henriklmao/hegn/actions/workflows/rust.yml/badge.svg)](https://github.com/Henriklmao/hegn/actions/workflows/rust.yml)
[![Release](https://img.shields.io/github/v/release/Henriklmao/hegn)](https://github.com/Henriklmao/hegn/releases)
[![License: GPL v3](https://img.shields.io/badge/license-GPLv3-blue.svg)](LICENSE)

</div>

<!-- Platzhalter-Screenshots: durch echte Vorschaubilder ersetzen -->
<p align="center">
  <img src="docs/assets/preview-dashboard.png" alt="hegn dashboard with the project picker" width="90%">
  <br>
  <img src="docs/assets/preview-session.png" alt="hegn attached to a coding agent in a VM" width="90%">
</p>

*hegn* is Norwegian/Danish for "fence" or "enclosure". That is what it does: it
keeps your coding agents and research projects in a secure enclosure. Each
project runs in its own small virtual machine, so an agent can work on your code
without reaching the rest of your system.

---

## Table of contents

- [Why hegn?](#why-hegn)
- [Features](#features)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Documentation](#documentation)
- [Requirements](#requirements)
- [Status](#status)
- [Contributing](#contributing)
- [License](#license)

---

## Why hegn?

Coding agents run commands, read files and install packages. You should not have
to trust them with your whole machine. hegn gives every project a boundary:

- **Real isolation.** Each project runs in a [Cloud Hypervisor](https://www.cloudhypervisor.org/)
  VM with its own kernel and disk. The agent sees only its project folder.
- **No VM, no work.** hegn never falls back to running things on your host. If the
  VM cannot start, hegn tells you what to fix.
- **Checked commands.** Commands typed in the hegn terminal are inspected before
  they reach the VM. Risky ones can be blocked or need your approval.
- **Save and resume.** A running VM can be snapshotted and restored in seconds.

---

## Features

- **One VM per project**, with several VMs running side by side.
- **Multi-agent clones.** Give parallel agents their own git clones, so they do not
  overwrite each other's work.
- **Ready-made harnesses** for OpenCode, Claude Code, Codex, pi, Prime Agent, Aider
  and Gemini CLI. Each one installs its tools automatically on first boot.
- **Profiles** pass API keys, config files and skills into the VM. Skills are
  copied, never linked, so the guest cannot change your real skill folders.
- **Snapshots** save and restore the full VM state. Automatic checkpoints make
  restarts fast.
- **Guarded network.** The guest can reach the public web and DNS. The local
  network and your host stay closed.
- **Command guardrails** based on [`tirith-core`](https://crates.io/crates/tirith-core)
  and hegn's own checks.
- **Terminal UI and CLI.** Everything in the TUI also works as a command, with
  `--json` output for scripts.
- **Detachable sessions.** Detach from a running agent and attach again later, like
  `tmux`.
- **Private logs** of every session, with terminal control sequences removed when
  you read them.

See [Features in detail](docs/README.md) for the full list.

---

## Installation

On Arch Linux, install from the AUR:

```bash
paru -S hegn-bin   # pre-built binary
# or
paru -S hegn       # build from source
```

Other distributions can use the release binaries or build from source. Details,
upgrade notes and an install guide for LLM agents are in
[Installation](docs/installation.md).

---

## Quick start

```bash
# 1. Check the host and prepare everything hegn needs (asks for sudo when required)
hegn doctor --fix

# 2. Create a project with a harness
hegn project create my-app --harness opencode

# 3. Open the terminal UI and start the VM from there
hegn
```

From the command line, the same steps look like this:

```bash
hegn instance start my-app
hegn attach my-app
```

Press `Ctrl-B` to detach. The VM keeps running. The full walkthrough is in
[Getting started](docs/getting-started.md).

---

## Documentation

| Topic | What you find there |
| --- | --- |
| [Installation](docs/installation.md) | Requirements, install methods, upgrading |
| [Getting started](docs/getting-started.md) | First project, first VM, harness presets |
| [Projects and instances](docs/projects-and-instances.md) | Importing projects, clones, deleting, read-only mode |
| [Harnesses and profiles](docs/harnesses.md) | API keys, skills, launch commands and terminal integrations |
| [Sessions, attach and logs](docs/sessions-and-logs.md) | Attaching, the status bar, session logs |
| [Configuration](docs/configuration.md) | `config.toml`, per-project settings, the settings editor |
| [TUI guide](docs/tui.md) | All keyboard shortcuts and screens |
| [Snapshots](docs/snapshots.md) | Manual snapshots and automatic resume |
| [Networking](docs/networking.md) | The guarded network, local LLM servers, firewalls |
| [VM images](docs/vm-images.md) | Kernel and rootfs, checksums, guest login |
| [Security](docs/security.md) | How isolation works and what it does not cover |
| [Troubleshooting](docs/troubleshooting.md) | Common problems and where the logs are |
| [CLI reference](docs/cli-reference.md) | All commands, flags and exit codes |
| [Resource usage](docs/resources.md) | Memory, disk and CPU cost, measured numbers |
| [Architecture](docs/architecture.md) | Design, modules and invariants for contributors |
| [LLM setup guide](docs/llm-setup.md) | Step-by-step installation for coding agents |

---

## Requirements

- Linux with KVM (`/dev/kvm`)
- Cloud Hypervisor and virtiofsd
- A kernel with Landlock (Linux 6.2 or newer)
- git

`hegn doctor` checks all of these. The complete list is in
[Installation](docs/installation.md#requirements).

---

## Status

hegn is early (`0.1.0`). The CLI, the security engine, snapshots, the serial
console and the TUI work today. Expect changes to the configuration format until
the first stable release.

---

## Contributing

Contributions are welcome. If you have an idea, found a bug, or want to improve
the docs, open an issue or a pull request. The working agreement for this
repository is in [`AGENTS.md`](AGENTS.md).

---

## License

hegn is licensed under the GNU General Public License v3.0. See [`LICENSE`](LICENSE).
Contributions are accepted under the same terms.
