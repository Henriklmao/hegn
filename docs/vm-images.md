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
replaced only with `--force`. Instances that already have their own rootfs copy
keep it. See [Instance layout](projects-and-instances.md#instance-layout-on-disk).

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

The seed disk that hegn creates enables a serial root login through cloud-init.
This is convenient for a local tool, but it is less strict.

Set `guest_credential_hardening = true` to change this. The console then logs in
as the unprivileged `hegn` user, and `sudo` asks for a password. Set that password
inside the guest first.
