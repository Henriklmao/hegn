# Configuration

hegn has three levels of configuration:

| File | Scope | Created by |
| --- | --- | --- |
| `~/.config/hegn/config.toml` | Global settings for all projects | `hegn config init` |
| `~/.config/hegn/profiles/<harness>.toml` | API keys, files and skills per harness | `hegn config init` |
| `hegn.toml` in each instance and project | Settings for one VM or project | `hegn project create` and instance creation |

- [Global config (`config.toml`)](#global-config-configtoml)
- [Per-instance and per-project settings](#per-instance-and-per-project-settings)
- [Settings editor](#settings-editor)
- [AGENTS.md templates](#agentsmd-templates)
- [Environment variables](#environment-variables)

Unknown keys are rejected, so typos show up right away. When you add an option,
update both the schema and this page.

---

## Global config (`config.toml`)

`hegn config init` writes `~/.config/hegn/config.toml` with these defaults:

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
memory_mib      = 2048       # 128–262144 (256 GiB)
max_rootfs_gib  = 64         # 16–1024; rootfs growth ceiling
disk_usage_warn_percent = 80 # doctor warns above this usage
shared_tag      = "workspace" # virtio-fs tag, <= 36 bytes, no whitespace
guest_mount     = "/workspace" # absolute guest path
virtiofs_sandbox = "namespace" # "namespace" (recommended) or "chroot"
backend         = "auto"      # "auto" or "cloud-hypervisor"; both need a VM
cloud_init      = true        # NoCloud seed for the official cloud image
guest_credential_hardening = false # opt-in: non-root console, passworded sudo
harness         = "shell"     # shell | opencode | claude-code | codex | pi |
                              # prime-agent | aider | gemini-cli
network         = "guarded"   # "guarded" (web+DNS only) or "off" (no NIC)
extra_packages  = []
auto_branch     = true
# Rolling checkpoints let a restart resume instead of cold booting.
periodic_snapshots     = true
snapshot_interval_secs = 300 # 30–86400
snapshot_keep          = 2   # 1–32 automatic checkpoints per instance
auto_resume            = true
security        = "enforce"   # "off", "warn" or "enforce"

# Launch the agent (or another command) after Enter boots a VM.
[launch]
default         = "harness"  # "harness", "shell", or a name from commands
integration     = "auto"     # auto | embedded | external | tmux | herdr
auto_start      = true

# Optional extra commands. They are typed into the guest shell.
[[launch.commands]]
name            = "review"
command         = "opencode"
integration     = "tmux"
bar             = false      # full-screen commands should own every row

# Optional: host-loopback services the guest may reach (local LLM servers).
# The guest URL is http://<tap-ip>:<port>; the host must listen on
# 127.0.0.1:<port>. After changing this list, run `sudo hegn network reload`
# (or `sudo hegn network setup` for a full reinstall).
# [[local_endpoints]]
# name = "ollama"   # guest env: HEGN_ENDPOINT_OLLAMA (and OLLAMA_HOST)
# port = 11434
```

Some key groups have their own pages:

- Images and checksums: [VM images](vm-images.md)
- Snapshot keys: [Snapshots](snapshots.md)
- `network`, `security` and `local_endpoints`: [Networking](networking.md) and [Security](security.md)
- `harness`, `launch`: [Harnesses and profiles](harnesses.md)

The package list is a list of *extras*. The harness preset and the packages from
the instance's harness profile are always added on top.

---

## Per-instance and per-project settings

Every instance has a `hegn.toml` in its directory (see
[Instance layout](projects-and-instances.md#instance-layout-on-disk)). Set
`read_only_workspace = true` there to mount the project read-only.

Every project has a `hegn.toml` in its folder. It is written by
`hegn project create` and records the harness, the packages and the template:

```toml
launch = "review"
integration = "herdr"
auto_start = true
```

---

## Settings editor

`F4` (global settings) and `F5` (selected instance) open an interactive editor.
`F6` in the project screen edits the project's `hegn.toml`. The editor
works on the schema, so you do not have to edit the file by hand.

Rows are grouped by runtime, storage, sharing, network and security, tools,
directories and packages. The instance editor also shows read-only rows for its
project, workspace, rootfs and TAP slot.

Seccomp and Landlock have no configuration keys. hegn never weakens the Cloud
Hypervisor sandbox.

| Input | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k`, wheel, click | Select a row |
| `←`/`→`, `h`/`l`, `‹`/`›`, click | Change a choice, step a number, toggle a switch |
| `Enter` | Toggle, change, start typing a value, or open a profile template |
| `Enter` / `Esc` while typing | Apply / discard the value |
| `Home` / `End`, `PgUp` / `PgDn` | Jump to the first or last row, page through rows |
| `[` / `]` | Switch pages (General / Profiles / Local LLM; instance: General) |
| `Ctrl-S` | Validate and save |
| `r` | Reload the file from disk (discard unsaved changes) |
| `e` | Open the raw TOML in `$EDITOR` (requires a clean draft) |
| `Esc` | Close. With unsaved changes, press it twice to discard them. |

Saving rewrites the file from the schema, so **comments in the file are not
kept**. If you need comments or keys that the editor does not show, use `e` (raw
editor) or `hegn instance config <name> --edit`.

Input is checked right away. Numbers must be within their range, `~` is expanded
in paths, and package names must be valid Debian names. The same validation runs
again before the file is saved.

The **Profiles** page lists every harness profile. It shows the template path or
`(missing)`. `Enter` creates a missing template and opens it in `$EDITOR`. Profile
files can contain API keys, so they stay `0600` in a `0700` directory.

---

## AGENTS.md templates

Put your own template in `~/.config/hegn/templates/AGENTS.md`. `hegn project
create` copies it into every new project. Use `--template <PATH>` to use another
file.

---

## Environment variables

These variables override the matching config values:

| Variable | Overrides |
| --- | --- |
| `HEGN_CONFIG` | path to `config.toml` |
| `HEGN_PROJECTS_ROOT` | `projects_root` |
| `HEGN_STATE_DIR` | `state_dir` |
| `HEGN_SNAPSHOTS_DIR` | `snapshots_dir` |
| `HEGN_TEMPLATES_DIR` | `templates_dir` |
| `HEGN_PROFILES_DIR` | `profiles_dir` |
| `HEGN_INSTANCES_DIR` | `instances_dir` |
| `HEGN_CLOUD_HYPERVISOR_BIN` | `cloud_hypervisor_bin` |
| `HEGN_VIRTIOFSD_BIN` | `virtiofsd_bin` |
| `HEGN_KERNEL_IMAGE`, `HEGN_ROOTFS_IMAGE` | `kernel_image`, `rootfs_image` |
| `HEGN_ROOTFS_FORMAT`, `HEGN_ROOT_DEVICE` | `rootfs_format`, `root_device` |
| `HEGN_KERNEL_URL`, `HEGN_KERNEL_SHA256` | `kernel_url`, `kernel_sha256` |
| `HEGN_ROOTFS_URL`, `HEGN_ROOTFS_SHA256` | `rootfs_url`, `rootfs_sha256` |

Harnesses that hegn starts also receive `HEGN=1` and `HEGN_WORKSPACE=<path>`.
