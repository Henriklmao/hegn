# Installation

This page covers what hegn needs on your machine, how to install it, and how to
upgrade from an older version.

- [Requirements](#requirements)
- [Install with an LLM agent](#install-with-an-llm-agent)
- [Arch Linux (AUR)](#arch-linux-aur)
- [AppImage and pre-built binaries](#appimage-and-pre-built-binaries)
- [Build from source](#build-from-source)
- [Upgrading from an older hegn](#upgrading-from-an-older-hegn)
- [Next steps](#next-steps)

---

## Requirements

| Requirement | Notes |
| --- | --- |
| Linux with KVM | `/dev/kvm` must exist and be accessible to your user (`kvm` group). x86_64 and aarch64 targets are published. |
| [Cloud Hypervisor](https://www.cloudhypervisor.org/) | `cloud-hypervisor` **or** `cloud-hypervisor-static` on `PATH` (AUR's `cloud-hypervisor-bin` installs the static name), or set `cloud_hypervisor_bin`. Native virtio-fs support is required; hegn enables seccomp and Landlock. |
| Landlock | A host kernel with Landlock ABI v3 (Linux 6.2 or newer). `hegn doctor` checks this; hegn refuses to start without it. |
| [virtiofsd](https://gitlab.com/virtio-fs/virtiofsd) | `virtiofsd` on `PATH` (or set `virtiofsd_bin`). Distribution install locations such as `/usr/lib/virtiofsd` (Arch), `/usr/libexec/virtiofsd` (Fedora) and `/usr/lib/qemu/virtiofsd` (Debian) are detected automatically. |
| Kernel + rootfs images | A guest kernel and a qcow2 or raw root filesystem. Defaults live under `~/.local/share/hegn/images/`. See [VM images](vm-images.md). |
| Free disk space | About 2.5 GiB for the pinned images and tools, plus about the size of guest RAM per snapshot and some log space. See [Resource usage](resources.md). |
| `git` | Used for project status, session branches and checkpoints. |
| `lazygit`, `$EDITOR` | Optional. Used by the `g` and `e` shortcuts. |

Run `hegn doctor` after installing. It checks all of the above and tells you
what is missing.

Hegn always needs a real Cloud Hypervisor VM. If the kernel, rootfs, Cloud
Hypervisor binary, virtiofsd or KVM device is missing, hegn stops with an error
that tells you what to fix. There is no fallback to running things on the host.

---

## Install with an LLM agent

You can hand the whole installation to a coding agent:

```text
Install Hegn, a MicroVM base containerization wizard according to the following guide: https://github.com/Henriklmao/hegn/blob/main/docs/llm-setup.md
```

The guide walks the agent through host checks, installation on Arch (AUR) or any
glibc distribution, VM images, `hegn doctor` and a smoke test. See
[`llm-setup.md`](llm-setup.md).

---

## Arch Linux (AUR)

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

The latest git snapshot is available as well:

```bash
paru -S hegn-git
# or
yay -S hegn-git
```

---

## AppImage and pre-built binaries

Tagged releases publish `hegn-<tag>-x86_64-linux-gnu` binaries and
`hegn-<tag>.AppImage` on the
[releases page](https://github.com/Henriklmao/hegn/releases).

---

## Build from source

You need Rust **1.89 or newer** (edition 2024).

```bash
git clone https://github.com/Henriklmao/hegn
cd hegn
cargo build --release --locked
install -Dm755 target/release/hegn ~/.local/bin/hegn
```

---

## Upgrading from an older hegn

The VM backend and the config schema changed. Before you start the new build:

1. Stop any VM that was started by an older version.
2. Remove its `state_dir/run/` directory.
3. Update `config.toml`:
   - Replace the old VMM binary key with `cloud_hypervisor_bin = "cloud-hypervisor"`.
     The static AUR binary `cloud-hypervisor-static` is detected automatically.
   - Set `rootfs_format` (`raw` or `qcow2`) and `root_device`. Use `/dev/vda1`
     for the official cloud image and `/dev/vda` for a whole-disk ext4 image.
   - `cloud_init` now defaults to `true`. Seccomp and Landlock are always on.
   - Remove the `landlock` and `seccomp` keys. They are no longer configurable,
     and an old `config.toml` that still contains them is rejected.
   - Run `hegn images download` again if the old kernel or rootfs paths are
     still configured.

An unreadable `session.json` is treated as a hard error and is never silently
overwritten. This way a VMM from an older version cannot be leaked.

---

## Next steps

Continue with [Getting started](getting-started.md) to create your first project
and boot your first VM.
