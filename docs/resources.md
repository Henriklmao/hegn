# Resource usage, overhead and storage

What hegn costs compared with working directly on the host: processes, RAM,
disk, I/O and start-up latency. The numbers below were measured on x86_64 with
`hegn` 0.1.0, Cloud Hypervisor 53.0 (static), virtiofsd 1.14.0, a 2 vCPU /
2048 MiB guest and the pinned cloud image. Treat them as order-of-magnitude
values; [measure on your machine](#measure-on-your-machine) before sizing a
build box.

## What runs where

hegn never runs project work on the host. A session consists of these host
processes plus the guest:

| Process | Role | Measured RSS |
| --- | --- | --- |
| `hegn` (TUI or CLI) | state machine, rendering, VMM API client | ~26 MiB for the TUI (10 000-line serial scrollback); a CLI run is short-lived |
| `hegn __console-relay` | detached serial relay, keeps the session log growing | ~5 MiB |
| `virtiofsd` (parent + worker) | shares the project directory into the guest (`--sandbox=namespace --cache=never`) | ~12 MiB total |
| `cloud-hypervisor(-static)` | the VMM and all guest RAM | ~404 MiB resident with a 2 GiB guest, ~2.1 GiB virtual |
| `hegn attach` | optional second client on the console socket | ~7 MiB |

The VMM maps the configured guest memory up front, so its virtual size is
`memory_mib` plus a few MiB, while the resident set grows towards
`memory_mib` as the guest touches pages. An idle guest therefore costs much
less than its configured RAM; a guest that runs a full build converges on
`memory_mib`.

Practical sizing rule for the host:

```text
worst case ≈ memory_mib + ~50 MiB (virtiofsd + relay + VMM bookkeeping)
             + ~26 MiB if the TUI stays open
```

| `memory_mib` | Host worst case | Typical idle (estimate) |
| --- | --- | --- |
| 1024 | ~1.1 GiB | ~0.3–0.5 GiB |
| 2048 (default) | ~2.1 GiB | ~0.4–0.6 GiB |
| 4096 | ~4.1 GiB | grows with guest use |
| 8192 | ~8.1 GiB | grows with guest use |

Only **one** sandbox can run at a time. `hegn sandbox start` refuses to start a
second VM, so the footprint above is the whole host cost of a session.

## CPU overhead

- **Compute is near native.** Guest vCPUs are host threads running under KVM,
  with no instruction emulation. Machine-heavy work (compilers, tests, language
  servers) runs at native speed, minus the cost of one extra context switch
  per trap.
- **Idle is cheap.** An idle guest parks its vCPU threads in the kernel; the
  VMM consumes well under a core.
- **File I/O has a real cost.** The workspace is a `virtiofsd` FUSE export with
  `--cache=never`, so every guest file operation is a round trip to the host:
  large sequential reads/writes come close to native, while workloads with
  many small files (`git status` on a huge tree, `npm install`,
  `node_modules` churn) are noticeably slower than on the host.
- **Terminal output** is relayed over a Unix socket. Dumping hundreds of
  megabytes to the serial console is bounded by the relay and the TUI
  scrollback, not by the guest.
- **Command interception** (`tirith-core`) runs in-process on the submitting
  side before a line reaches the guest. It adds a per-command check, not a
  per-syscall one; `hegn check` is the scripted equivalent.
- **No network device.** The guest has no NIC by design, so there is no host
  network overhead — and no in-guest downloads either. Install tooling in the
  shared rootfs once, or keep dependencies in the workspace.

## Start, attach, stop, snapshot

| Operation | Cost |
| --- | --- |
| `hegn sandbox start` | spawns virtiofsd, boots the VMM and waits for the Cloud Hypervisor API socket (15 s cap; usually well under a second on a local SSD), then starts the relay (5 s cap) |
| Guest login prompt | a few seconds after boot on the pinned kernel |
| First-boot cloud-init | runs in the background; measured ~91 s until "cloud-init finished" in the sample guest (it probes for a network that does not exist). The console is usable before that |
| `hegn attach` | connects to an existing socket; effectively instant |
| `hegn sandbox stop` | SIGTERM to the VMM, 2 s grace, then SIGKILL; the relay and virtiofsd are terminated and the run directory is cleaned |
| `hegn snapshot create` | pauses the VM, writes guest RAM and device state, resumes; proportional to `memory_mib` and disk write speed |

Because `sandbox stop` signals the VMM rather than asking the guest to shut
down, dirty pages in the guest page cache may be lost. Finish or sync
in-flight work before stopping; snapshots do **not** preserve disk contents.

## Disk usage

| Item | Size (measured) | Notes |
| --- | --- | --- |
| Guest kernel | 46 MiB | `vmlinux-x86_64`, pinned and SHA-256 verified |
| Guest rootfs | 1.9 GiB | qcow2 cloud image, shared by all projects, writable and persistent |
| `images/` total | 2.2 GiB | after the pre-Cloud-Hypervisor migration a checkout may still contain the old `rootfs.ext4` (300 MiB) and `vmlinux.bin` (42 MiB); they can be deleted |
| `hegn` binary | 19 MiB | stripped release build |
| `cloud-hypervisor-static` | 7 MiB | upstream static binary |
| `virtiofsd` | 2.4 MiB | distribution package |
| Run directory | ~64 KiB on disk | sparse 4 MiB cloud-init seed disk (`cloud-init.img`) plus sockets, pid files and logs |
| Session logs | ~45 KiB per interactive session | line-based; verbose builds can produce far more. Retention is by count (default 50), `hegn logs prune --keep N` |
| Snapshot | ≈ `memory_mib` + a few KiB | `memory-ranges` holds guest RAM (sparse where the guest never touched pages); `state.json`/`config.json` are small. Disk contents are not copied |
| Project workspace | 0 | shared read-write through virtio-fs, never copied |
| Development checkout | ~9.3 GiB `target/` (this repository) | debug and release artifacts; `cargo clean` reclaims it |

Minimum free space for a usable install: **~2.5 GiB** (images plus tools), plus
whatever the guest writes into its rootfs, plus snapshots. Budget **5 GiB** if
snapshots of the default 2 GiB guest are part of the workflow.

### Lifecycle of the rootfs

The qcow2 image is mounted read-write and reused by every project. Guest
changes (installed packages, edited dotfiles, experiment leftovers) persist and
grow the image; snapshots capture RAM, not the disk. To get a clean rootfs back,
re-download the pinned image:

```bash
hegn images download --force   # replaces the qcow2 with the pristine pinned image
```

That discards everything installed in the guest.

## Compared with working natively

| Dimension | Native host | hegn |
| --- | --- | --- |
| Start of a task | process start (ms) | VM boot plus harness start (seconds) |
| Compute | baseline | near-native via KVM |
| Many-small-file I/O | baseline | slower (virtio-fs, `cache=never`) |
| Large sequential I/O | baseline | close to native |
| Memory | project process only | `memory_mib` + ~50 MiB, TUI optional |
| Disk | project only | +2.2 GiB images, + snapshots (≈ guest RAM each), + logs |
| Network | available | none by design |
| Guest state | host state | persistent shared rootfs across projects |
| Isolation | none | Cloud Hypervisor seccomp + Landlock, sandboxed virtiofsd, `tirith-core` interception |
| Rollback | git | git checkpoints plus VM snapshots (RAM/state, not disk) |

The trade is predictable: startup and small-file I/O get slower, and a fixed
disk/memory budget is reserved on the host, in exchange for a VM boundary
around every command and snapshot-based rollback.

## Measure on your machine

```bash
hegn sandbox status --json        # vcpus, memory_mib, pids, running flags
hegn images status                # kernel/rootfs state and size
hegn doctor                       # resolved binaries and versions
hegn logs list                    # session logs and sizes

# host cost of a running session
ps -o pid,rss,vsz,args -C cloud-hypervisor-static,virtiofsd,hegn

# disk
du -sh ~/.local/share/hegn/images ~/.local/share/hegn/snapshots \
       ~/.local/state/hegn/logs ~/.local/state/hegn/run
df -h ~/.local/share/hegn
free -m
```

## Reducing the footprint

- Lower `memory_mib` (minimum 128) and `vcpus` (minimum 1) in `config.toml`
  for small projects; a shell-based session does not need 2 GiB.
- Keep the TUI closed for long unattended runs; `hegn sandbox start`,
  `hegn sandbox status --json` and `hegn logs` need a few MiB, not ~26 MiB.
- Delete snapshots you no longer need: `hegn snapshot list`,
  `hegn snapshot delete <name>`. Each one costs roughly the guest RAM.
- Prune logs: `hegn logs prune --keep 20`.
- Remove the pre-Cloud-Hypervisor leftovers (`rootfs.ext4`, `vmlinux.bin`)
  from `~/.local/share/hegn/images/` after migrating.
- Reset a bloated rootfs with `hegn images download --force` when guest state
  no longer matters.
- In a development checkout, `cargo clean` frees several GiB of `target/`.
