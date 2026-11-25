# Configuration

hegn has four configuration layers:

| File | Scope | Created by |
| --- | --- | --- |
| `~/.config/hegn/config.toml` | Global defaults for all VMs and projects | `hegn config init` |
| `~/.config/hegn/profiles/<harness>.toml` | API keys, files and skills per harness | `hegn config init` |
| `<vms_dir>/<vm>/vm.toml` | One VM's harness, resources and runtime policy | `hegn vm create` |
| `hegn.toml` in a project | Presentation preferences for one project | Optional; edited by hand or `F6` |

Membership is not configuration. Assignments, read-only flags and fork lineage
live in the private registry `<state_dir>/projects.toml`, which hegn writes under
a lock and never reads from inside a project.

- [Global config (`config.toml`)](#global-config-configtoml)
- [VM config (`vm.toml`)](#vm-config-vmtoml)
- [Project settings (`hegn.toml`)](#project-settings-hegntoml)
- [Project registry](#project-registry)
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
vms_dir         = "~/.local/share/hegn/vms"
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
vcpus           = 2          # 1–32; seeded into new VMs
memory_mib      = 2048       # 128–262144 (256 GiB); seeded into new VMs
max_rootfs_gib  = 64         # 16–1024; rootfs growth ceiling
disk_usage_warn_percent = 80 # doctor warns above this usage
share_tag_prefix = "hegn-prj" # tags are <prefix>-<index>, <= 36 bytes total
guest_projects_base = "/projects" # guest directory for project mounts
max_projects_per_vm = 16      # 1–32 exported projects per VM
virtiofs_sandbox = "namespace" # "namespace" (recommended) or "chroot"
backend         = "auto"      # "auto" or "cloud-hypervisor"; both need a VM
cloud_init      = true        # NoCloud seed for the official cloud image
guest_credential_hardening = false # opt-in: non-root console, passworded sudo
harness         = "shell"     # default for new VMs
network         = "guarded"   # "guarded" (web+DNS only) or "off" (no NIC)
extra_packages  = []          # seeded into new VMs
auto_branch     = true
# Rolling checkpoints let a restart resume instead of cold booting.
periodic_snapshots     = true
snapshot_interval_secs = 300 # 30–86400
snapshot_keep          = 2   # 1–32 automatic checkpoints per VM
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

`vms_dir` holds one directory per VM with `vm.toml`, runtime files, the private
rootfs and managed clone workspaces. `state_dir` holds `projects.toml` and the
logs. Neither is ever exported to a guest.

### Deprecated keys

The old one-project-per-VM keys still parse as aliases:

| Deprecated | Replacement |
| --- | --- |
| `instances_dir` | `vms_dir` |
| `sandboxes_dir` | `vms_dir` |
| `shared_tag` | `share_tag_prefix` |
| `guest_mount` | `guest_projects_base` |

hegn prints a warning to stderr for each deprecated key found in `config.toml`.
`hegn doctor` also lists them as the optional `deprecated config keys` check
(marked `!`). Read the values correctly, but rewrite the file with the new
names. `vms_dir` must be an absolute path and must not contain a comma.

The old v1 default `guest_mount = "/workspace"` is migrated to `/projects` with
this exact warning:

```text
warning: legacy guest_mount = "/workspace" is reserved for the primary /workspace symlink; using guest_projects_base = "/projects" instead. Set guest_projects_base explicitly to silence this warning.
```

The migration only applies to the legacy key with the old `/workspace` value.
An explicit `guest_projects_base = "/workspace"` is rejected: `/workspace` and
paths under it are reserved for the primary symlink.

`share_tag_prefix` must leave room for `-<index>` within the 36-byte virtio-fs
tag limit and must not be the reserved profile tag `hegn-profile`.

`guest_projects_base` must be an absolute guest path with simple components.
A base other than `/projects` is allowed; projects then mount below that base,
and `/workspace` points at the primary project below it. `/workspace` and paths
under it are reserved for the primary symlink and cannot be the project base.

Some key groups have their own pages:

- Images and checksums: [VM images](vm-images.md)
- Snapshot keys: [Snapshots](snapshots.md)
- `network`, `security` and `local_endpoints`: [Networking](networking.md) and [Security](security.md)
- `harness`, `launch`: [Harnesses and profiles](harnesses.md)

The package list is a list of *extras*. The harness preset and the packages from
the VM's harness profile are always added on top.

---

## VM config (`vm.toml`)

`hegn vm create` writes `<vms_dir>/<name>/vm.toml` (mode `0600`) from the global
defaults. `hegn vm config <name>` prints it; `--edit` opens it in `$EDITOR`.

```toml
version = 2
name = "dev"
harness = "opencode"
vcpus = 8
memory_mib = 16384
extra_packages = ["rustup"]
# rootfs_image = "/abs/optional.qcow2"   # omit for a private per-VM copy
primary = "parser"             # picker name; drives /workspace
security = "enforce"
periodic_snapshots = true
snapshot_interval_secs = 300
snapshot_keep = 2
auto_resume = true
network_index = 3              # assigned once, kept across restores
created_at = "2026-11-24T10:00:00Z"
```

These are VM settings: `security` and the snapshot policy fields are stored per
VM in `vm.toml` and copied from the global defaults when the VM is created.
They are effective at runtime: changing them in `vm.toml` takes effect on the
next start, and `hegn vm apply <vm>` restarts a running VM to apply them.
`vm.toml` deliberately does **not** list projects. Boot resolves the VM name to
members by scanning the private registry, so membership has one source of truth.

`primary` must name a current exported member. Changing it requires a restart
(`hegn vm apply`) because `/workspace` is a boot-time symlink. When the first
project is assigned to an empty VM, it becomes primary automatically.

An explicit `rootfs_image` must be absolute, must not contain a comma, and must
be unique across VMs. Two VMs may not boot from the same writable disk.

Harness and package changes do not rewrite an existing rootfs. Install tools
inside the guest or run `hegn vm rebuild <name>`.

---

## Project settings (`hegn.toml`)

A project may carry a `hegn.toml` for presentation preferences only:

```toml
launch = "review"
integration = "herdr"
auto_start = true
```

It cannot change membership, read-only status, resources or the tool profile.
Those live in the private registry and `vm.toml`.

v1 project files that contain `harness`, `packages` or `template` still parse,
but those keys are ignored for control. Move harness and package choices to the
VM.

---

## Project registry

```text
<state_dir>/projects.toml        # 0600, atomic tmp+rename, flock for writers
```

```toml
version = 2

[[project]]
name = "parser"
path = "/home/me/code/parser"    # canonicalized
vm = "dev"                       # absent = unassigned
read_only = false
kind = "external"                # external | clone | worktree
managed = false                  # true only for hegn-created clone dirs
hidden = false
created_at = "2026-11-24T10:00:00Z"

[[project]]
name = "parser-fix-tests"
path = "/home/me/.local/share/hegn/vms/dev/workspaces/parser-fix-tests"
vm = "dev"
kind = "clone"
parent = "parser"
managed = true
```

The registry decides which host directories are exported and whether they are
writable. It lives outside every exported directory, is not agent-writable and
cannot be changed through `hegn.toml`. Use the CLI (`hegn project assign`,
`unassign`, `fork`, `delete`) rather than editing it by hand.

---

## Settings editor

`F4` (global settings), `F5` (selected VM) and `F6` (project presentation) open
an interactive editor. The editor works on the schema, so you do not have to edit
the file by hand.

Rows are grouped by runtime, storage, sharing, network and security, tools,
directories and packages. The VM editor also shows read-only rows for its
resources, rootfs and TAP slot. The project editor only edits `launch`,
`integration` and `auto_start`.

Seccomp and Landlock have no configuration keys. hegn never weakens the Cloud
Hypervisor sandbox.

| Input | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k`, wheel, click | Select a row |
| `←`/`→`, `h`/`l`, `‹`/`›`, click | Change a choice, step a number, toggle a switch |
| `Enter` | Toggle, change, start typing a value, or open a profile template |
| `Enter` / `Esc` while typing | Apply / discard the value |
| `Home` / `End`, `PgUp` / `PgDn` | Jump to the first or last row, page through rows |
| `[` / `]` | Switch pages (General / Profiles / Local LLM; VM: General) |
| `Ctrl-S` | Validate and save |
| `r` | Reload the file from disk (discard unsaved changes) |
| `e` | Open the raw TOML in `$EDITOR` (requires a clean draft) |
| `Esc` | Close. With unsaved changes, press it twice to discard them. |

Saving rewrites the file from the schema, so **comments in the file are not
kept**. If you need comments or keys that the editor does not show, use `e` (raw
editor) or `hegn vm config <name> --edit`.

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
| `HEGN_VMS_DIR` | `vms_dir` |
| `HEGN_INSTANCES_DIR` | `vms_dir` (deprecated fallback) |
| `HEGN_CLOUD_HYPERVISOR_BIN` | `cloud_hypervisor_bin` |
| `HEGN_VIRTIOFSD_BIN` | `virtiofsd_bin` |
| `HEGN_KERNEL_IMAGE`, `HEGN_ROOTFS_IMAGE` | `kernel_image`, `rootfs_image` |
| `HEGN_ROOTFS_FORMAT`, `HEGN_ROOT_DEVICE` | `rootfs_format`, `root_device` |
| `HEGN_KERNEL_URL`, `HEGN_KERNEL_SHA256` | `kernel_url`, `kernel_sha256` |
| `HEGN_ROOTFS_URL`, `HEGN_ROOTFS_SHA256` | `rootfs_url`, `rootfs_sha256` |
| `HEGN_NETWORK` | `network` |
