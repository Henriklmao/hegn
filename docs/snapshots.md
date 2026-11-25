# Snapshots

A snapshot saves the complete state of a running VM: RAM, CPU and devices. You
can restore it later and continue exactly where you stopped. hegn has two kinds
of snapshots:

- **manual snapshots** that you create on purpose,
- **automatic checkpoints** that hegn creates in the background and uses to resume
  quickly.

Snapshots belong to a **VM**, not to a project. A snapshot covers every exported
member of that VM.

- [Manual snapshots](#manual-snapshots)
- [Automatic resume](#automatic-resume)
- [Boot fingerprints](#boot-fingerprints)
- [Legacy snapshots](#legacy-snapshots)
- [Guest safeguards](#guest-safeguards)
- [What a snapshot does not cover](#what-a-snapshot-does-not-cover)
- [Configuration](#configuration)

---

## Manual snapshots

```bash
hegn snapshot list
hegn snapshot create before-refactor --instance dev
hegn snapshot restore before-refactor --instance dev
hegn snapshot delete before-refactor
hegn snapshot show before-refactor
```

Without an owner flag, `hegn snapshot create` uses the only running VM.
`hegn snapshot restore` defaults to the VM recorded in the snapshot metadata.
`hegn snapshot list` shows the owning VM and whether the snapshot is `v2` or
`legacy`.

`hegn snapshot create` pauses the VM. Cloud Hypervisor writes `config.json`,
`state.json` and `memory-ranges` into a private directory. Then the VM resumes.

Cloud Hypervisor stores device paths in the snapshot: the rootfs, the serial and
virtio-fs sockets, and the Landlock rule. Restore therefore needs the same
`state_dir` and `snapshots_dir`. If those paths have moved, the restore fails
safely instead of starting an inconsistent VM.

---

## Automatic resume

While a VM runs, a detached worker process (`run/autosave.pid`):

1. pauses the VM every `snapshot_interval_secs`,
2. writes a checkpoint tagged `autosave`,
3. keeps only the newest `snapshot_keep` checkpoints per VM.

When you start the VM again, hegn restores the newest automatic checkpoint whose
boot fingerprint matches. If none matches, or the restore fails, hegn cold boots
and writes the reason to `run/hegn.log`. A normal stop or a crash therefore costs
seconds instead of a full boot.

- Manual snapshots are never loaded automatically and never pruned.
- Legacy snapshots are never selected for automatic resume.
- A paused VM is not resumed just to take an automatic checkpoint.

---

## Boot fingerprints

A v2 snapshot records:

```text
vm: "dev"
boot_fingerprint: "sha256:..."
membership: [ { name, path, tag, guest_path, read_only, kind } ... ]
```

`membership` lists the members that were actually exported. Worktrees are not
listed: they ride their parent's share and do not change the device tree, so
adding a worktree does not invalidate a snapshot.

`boot_fingerprint` hashes everything that changes the VMM device tree:

- ordered membership (name, canonical path, tag, guest mount, read-only, kind),
- `vcpus`, `memory_mib`, rootfs path and format,
- `network_index`, profile-share presence,
- the `primary` project that drives `/workspace`.

Semantics:

- **Automatic resume:** a checkpoint whose fingerprint differs is skipped with a
  logged reason, and the VM cold boots. hegn never restores a VM whose embedded
  mounts no longer exist.
- **Manual restore:** a fingerprint mismatch is refused with a message naming the
  changed fields and suggesting `hegn vm apply`. There is no `--force` in v1 of
  this model; a restored VM cannot be reconciled with added or removed shares
  after the fact.
- **Restore flow:** hegn re-spawns one `virtiofsd` per recorded member on the
  same indexed sockets, regenerates the seed from the recorded membership,
  restores the VMM, and rewrites the session record.

Assigning or unassigning a project, changing the primary, or changing VM
resources therefore makes older snapshots non-resumable. Take a new snapshot
after `hegn vm apply` if you need a restorable checkpoint of the new layout.

---

## Legacy snapshots

Snapshots written by hegn v1 have no fingerprint and no membership list. They
still appear in `hegn snapshot list`, marked `legacy`, with the label
`legacy v1 snapshot (no boot fingerprint; not restorable)`.

hegn never restores them and never deletes them automatically. Delete one
explicitly with `hegn snapshot delete <name>` if you no longer need the files.

---

## Guest safeguards

A restored VM would normally carry old state into the new session. Two safeguards
prevent that:

- **Random numbers.** Cloud Hypervisor gets a `virtio-rng` device backed by the
  host's `/dev/urandom`. A restored VM therefore does not replay the random
  generator state that was saved in the snapshot.
- **Clock and entropy daemon.** The guest runs `hegn-maintenance`. Every 15
  seconds it reads the host time from the guest RTC (`hwclock --hctosys`) and mixes
  fresh bytes from `/dev/hwrng` into `/dev/urandom`. This works with
  `network = "off"` and with the guarded network, which does not allow NTP.

Images that are booted with `cloud_init = false` must provide an equivalent guest
unit. The host-backed random number device still applies.

---

## What a snapshot does not cover

Automatic checkpoints restore the guest's RAM and CPU state. They do **not**
restore the writable rootfs. They make resuming fast, but they are not a
crash-consistent backup of the disk.

Stop the VM cleanly before you upgrade the kernel or do anything that must
survive a rollback. A snapshot also does not capture host-side project files:
those are the same directories the VM exported before and after the restore.

---

## Configuration

Global defaults live in `config.toml`; new VMs copy them into `vm.toml`:

```toml
periodic_snapshots      = true
snapshot_interval_secs  = 300  # 30–86400
snapshot_keep           = 2    # 1–32 per VM
auto_resume             = true
```

With the defaults, each running VM keeps two checkpoints. Budget about twice the
guest RAM per running VM for them. See [Resource usage](resources.md) for the
numbers.
