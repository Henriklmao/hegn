# VM images

hegn does not build its own guest image. It uses the **official Cloud Hypervisor
images**: a Linux kernel and a cloud image of Ubuntu 22.04 (Jammy). Each download
is checked with a SHA-256 hash before it is used.

- [Where the pins come from](#where-the-pins-come-from)
- [Commands](#commands)
- [How downloads are checked](#how-downloads-are-checked)
- [Automatic pin updates](#automatic-pin-updates)
- [Guest login](#guest-login)

---

## Where the pins come from

The URLs and SHA-256 hashes of the kernel and the cloud image are pinned in
`src/vm/images/assets.rs`. There is one set per architecture. You can override
them in `config.toml` (see [Configuration](configuration.md)).

---

## Commands

```bash
hegn images status          # path, state, size
hegn images download        # fetch missing images, verify, install atomically
hegn images download --force
hegn images verify          # re-hash installed files (exit 1 on mismatch)
```

`hegn images download` only fetches images that are missing. Existing images are
replaced only with `--force`. VMs that already have their own rootfs copy
keep it. See [On-disk layout](projects-and-vms.md#on-disk-layout).

---

## How downloads are checked

- The file is streamed into a temporary file with `0600` permissions.
- hegn computes the hash while it downloads.
- The file is renamed into place only after the hash matches.

If a custom configuration has no SHA-256 value, downloads refuse to run. Use
`--allow-unverified` only if you really want that.

---

## Automatic pin updates

The `vm-images` GitHub Actions workflow runs every week. It finds the newest
upstream kernel and cloud image, recalculates the hashes and opens a pull request
with the new pins. hegn does not publish an image of its own.

---

## Guest login

hegn configures two ways into the guest:

- **SSH** is the default for `hegn attach`, `hegn open`, `launch` and
  `hegn vm shell`. Every VM gets its own Ed25519 client key under
  `<vms_dir>/<vm>/ssh/` and its own `known_hosts`. cloud-init installs a
  public-key-only sshd drop-in, authorizes the key for `ssh_user` and
  regenerates the guest host keys on each run.
- **The serial console** remains the boot, provisioning and recovery channel
  (`hegn attach <vm> --serial`). The seed enables serial autologin there.

`ssh_user` defaults to `root`, or `hegn` when
`guest_credential_hardening = true`. Setting it to `root` together with
hardening is rejected. Only the chosen account receives the key. Password,
keyboard-interactive, agent and TCP forwarding are disabled.

The official cloud image already ships OpenSSH server and `ssh-keygen`; custom
`rootfs_image` builds must include both. The host needs the OpenSSH client
(`ssh`, `ssh-keygen`); `hegn doctor` checks it.
