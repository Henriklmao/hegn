# Snapshots

A snapshot saves the complete state of a running VM: RAM, CPU and devices. You
can restore it later and continue exactly where you stopped. hegn has two kinds
of snapshots:

- **manual snapshots** that you create on purpose,
- **automatic checkpoints** that hegn creates in the background and uses to resume
  quickly.

- [Manual snapshots](#manual-snapshots)
- [Automatic resume](#automatic-resume)
- [Guest safeguards](#guest-safeguards)
- [What a snapshot does not cover](#what-a-snapshot-does-not-cover)
- [Configuration](#configuration)

---

## Manual snapshots

```bash
hegn snapshot list
hegn snapshot create before-refactor
hegn snapshot restore before-refactor
hegn snapshot delete before-refactor
```

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
3. keeps only the newest `snapshot_keep` checkpoints per instance.

When you start an instance again, hegn first restores the newest automatic
checkpoint. Only if that fails does it fall back to a cold boot. A normal stop or
a crash therefore costs seconds instead of a full boot.

- Manual snapshots are never loaded automatically and never pruned.
- If a checkpoint cannot be restored, the reason is written to `run/hegn.log`.
  The instance then cold-boots.

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

Stop the instance cleanly before you upgrade the kernel or do anything that must
survive a rollback.

---

## Configuration

```toml
periodic_snapshots      = true
snapshot_interval_secs  = 300  # 30–86400
snapshot_keep           = 2    # 1–32 per instance
auto_resume             = true
```

With the defaults, each running instance keeps two checkpoints. Budget about
twice the guest RAM per running instance for them. See
[Resource usage](resources.md) for the numbers.
