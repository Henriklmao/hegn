# Resource usage, overhead and storage

What hegn costs compared with working directly on the host: processes, RAM,
disk, I/O and start-up latency. The numbers below were measured on x86_64 with
`hegn` 0.1.0, Cloud Hypervisor 53.0 (static), virtiofsd 1.14.0, a 2 vCPU /
2048 MiB guest and the pinned cloud image. Treat them as order-of-magnitude
values; [measure on your machine](#measure-on-your-machine) before sizing a
build box.

## Summary

- **RAM:** a running VM uses the configured `memory_mib` (default 2 GiB), plus
  about 5 MiB for the console relay and a few MiB of VMM bookkeeping. Every
  exported project adds one `virtiofsd` of about 12 MiB. The TUI adds about
  26 MiB while it is open. A harness profile that exports files or skills starts
  one additional `virtiofsd` for the whole VM.
- **Scaling:** VMs are independent and run at the same time. Within a VM, cost
  scales with the number of exported projects, not with a second rootfs.
- **Disk:** the pinned images take about 2.2 GiB and are shared by all VMs. The
  binary is about 19 MiB. Each VM gets one private rootfs; projects do not get
  their own. A snapshot is about as large as the guest RAM. Automatic
  checkpoints keep the newest `snapshot_keep` per VM (2 by default), so budget
  about 2× guest RAM per running VM. Session logs are pruned by count.
- **Speed:** computation runs at near-native speed under KVM. Many small file
  operations are slower, because every project is exported through virtio-fs
  with `--cache=never`.

## What runs where

hegn never runs project work on the host. A session consists of these host
processes plus the guest:

| Process | Role | Measured RSS |
| --- | --- | --- |
| `hegn` (TUI or CLI) | state machine, rendering, VMM API client | ~26 MiB for the TUI (10 000-line serial scrollback); a CLI run is short-lived |
| `hegn __console-relay` | detached serial relay, keeps the session log growing | ~5 MiB |
| `virtiofsd` (parent + worker), one per project | shares one project directory into the guest (`--sandbox=namespace --cache=never`) | ~12 MiB total each |
| `virtiofsd` (profile share) | optional daemon for the VM's copied config/skill payload | ~12 MiB total when the profile has content |
| `cloud-hypervisor(-static)` | the VMM and all guest RAM | ~404 MiB resident with a 2 GiB guest, ~2.1 GiB virtual |
| `hegn attach` | optional second client on the console socket | ~7 MiB |

The VMM maps the configured guest memory up front, so its virtual size is
`memory_mib` plus a few MiB, while the resident set grows towards
`memory_mib` as the guest touches pages. An idle guest therefore costs much
less than its configured RAM; a guest that runs a full build converges on
`memory_mib`.

Practical sizing rule for each running VM with `N` exported projects:

```text
per VM ≈ memory_mib + N × ~12 MiB (project virtiofsd)
         + ~5 MiB (relay) + VMM bookkeeping
         + ~12 MiB when a profile share is active
         + ~26 MiB if the TUI stays open
```

| `memory_mib` | Host cost per VM, 1 project (worst case) | Typical idle (estimate) |
| --- | --- | --- |
| 1024 | ~1.1 GiB | ~0.3–0.5 GiB |
| 2048 (default) | ~2.1 GiB | ~0.4–0.6 GiB |
| 4096 | ~4.1 GiB | grows with guest use |
| 8192 | ~8.1 GiB | grows with guest use |

Adding a project to a VM costs one `virtiofsd` (about 12 MiB), not another
rootfs or another guest. Several VMs can run concurrently; the cost multiplies
per VM. `hegn benchmark` lists running VMs and totals their RAM.

## CPU overhead

- **Compute is near native.** Guest vCPUs are host threads running under KVM,
  with no instruction emulation. Machine-heavy work (compilers, tests, language
  servers) runs at native speed, minus the cost of one extra context switch
  per trap.
- **Idle is cheap.** An idle guest parks its vCPU threads in the kernel; the
  VMM consumes well under a core.
- **File I/O has a real cost.** Each project is a `virtiofsd` FUSE export with
  `--cache=never`, so guest file operations are round trips to the host:
  large sequential reads/writes come close to native, while workloads with
  many small files (`git status` on a huge tree, `npm install`,
  `node_modules` churn) are noticeably slower than on the host. A VM with more
  members runs more daemons, but each member's I/O is independent.
- **Terminal output** is relayed over a Unix socket. Dumping hundreds of
  megabytes to the serial console is bounded by the relay and the TUI
  scrollback, not by the guest.
- **Command interception** (`tirith-core`) runs in-process on the submitting
  side before a line reaches the guest. It adds a per-command check, not a
  per-syscall one; `hegn check` is the scripted equivalent.

## Start, attach, stop, snapshot

| Operation | Cost |
| --- | --- |
| `hegn vm start` | spawns one virtiofsd per member, boots the VMM and waits for the Cloud Hypervisor API socket (15 s cap; usually well under a second on a local SSD), then starts the relay (5 s cap) |
| Guest login prompt | a few seconds after boot on the pinned kernel |
| First-boot cloud-init | runs in the background; measured ~91 s until "cloud-init finished" in the sample guest (it probes for a network that does not exist). The console is usable before that |
| `hegn attach` | connects to an existing socket; effectively instant |
| `hegn vm stop` | SIGTERM to the VMM, 2 s grace, then SIGKILL; the relay, autosave worker and every member virtiofsd are terminated and the run directory is cleaned |
| `hegn vm apply` | full stop plus cold start (or a fingerprint-matching resume); kills running agents |
| `hegn snapshot create` | pauses the VM, writes guest RAM and device state, resumes; proportional to `memory_mib` and disk write speed |

Because `vm stop` signals the VMM rather than asking the guest to shut down,
dirty pages in the guest page cache may be lost. Finish or sync in-flight work
before stopping; snapshots do **not** preserve disk contents.

## Disk usage

| Item | Size (measured) | Notes |
| --- | --- | --- |
| Guest kernel | 46 MiB | `vmlinux-x86_64`, pinned and SHA-256 verified |
| Guest rootfs | 1.9 GiB base, 16 GiB virtual per VM | the pinned qcow2 is copied (reflink where supported) per VM and grown sparsely for the harness packages; only written blocks consume host space |
| `images/` total | 2.2 GiB | after the pre-Cloud-Hypervisor migration a checkout may still contain the old `rootfs.ext4` (300 MiB) and `vmlinux.bin` (42 MiB); they can be deleted |
| `hegn` binary | 19 MiB | stripped release build |
| `cloud-hypervisor-static` | 7 MiB | upstream static binary |
| `virtiofsd` | 2.4 MiB | distribution package |
| Run directory | ~64 KiB on disk | sparse 4 MiB cloud-init seed disk (`cloud-init.img`) plus sockets, pid files and logs |
| Session logs | ~45 KiB per interactive session | line-based; verbose builds can produce far more. Retention is by count (default 50), `hegn logs prune --keep N` |
| Snapshot | ≈ `memory_mib` + a few KiB | `memory-ranges` holds guest RAM (sparse where the guest never touched pages); `state.json`/`config.json` are small. Disk contents are not copied |
| Project workspace | 0 | shared read-write through virtio-fs, never copied |
| Clone workspace | size of one checkout | `<vms_dir>/<vm>/workspaces/<name>`; shares the VM's rootfs and tools |
| Development checkout | ~9.3 GiB `target/` (this repository) | debug and release artifacts; `cargo clean` reclaims it |

Minimum free space for a usable install: **~2.5 GiB** (images plus tools), plus
whatever the guest writes into its rootfs, plus snapshots. The 16 GiB VM size is
sparse; a fully provisioned OpenCode guest typically uses a few GiB. Budget
**5 GiB** if snapshots of the default 2 GiB guest are part of the workflow.

### Lifecycle of the rootfs

Each VM gets one writable rootfs copy on first start. Guest changes (installed
packages, edited dotfiles, experiment leftovers) persist in that VM and are
shared by every project assigned to it; they are not shared with other VMs.
Snapshots capture RAM, not the disk. To get a clean rootfs back, use
`hegn vm rebuild <vm>`; the next start copies the pristine pinned image again.
`hegn images download --force` only replaces the shared base image.

## Compared with working natively

| Dimension | Native host | hegn |
| --- | --- | --- |
| Start of a task | process start (ms) | VM boot plus harness start (seconds) |
| Compute | baseline | near-native via KVM |
| Many-small-file I/O | baseline | slower (virtio-fs, `cache=never`) |
| Large sequential I/O | baseline | close to native |
| Memory | project process only | `memory_mib` + relay + N × virtiofsd, TUI optional |
| Disk | project only | +2.2 GiB images, + one rootfs and snapshots per VM, + logs |
| Network | available | none by design unless `network = "guarded"` |
| Guest state | host state | persistent rootfs shared by the VM's projects |
| Isolation | none | Cloud Hypervisor seccomp + Landlock, sandboxed virtiofsd, `tirith-core` interception |
| Rollback | git | git checkpoints plus VM snapshots (RAM/state, not disk) |

The trade is predictable: startup and small-file I/O get slower, and a fixed
disk/memory budget is reserved on the host, in exchange for a VM boundary
around every command and snapshot-based rollback.

## Measure on your machine

`hegn benchmark` performs the measurements this document is based on:

```bash
hegn benchmark               # running VM; offers to boot one when idle
hegn benchmark my-project    # boot (or reuse) that project's VM
hegn benchmark --clean       # throwaway project + VM, measured, then removed
hegn benchmark --json        # machine-readable report
```

Without arguments and without a running VM, the command asks on an
interactive terminal whether it should create a temporary test project. It
never prompts when stdin is not a terminal or `--json` is set, so scripts and
agents get the passive report instead.

`--clean` creates `<projects_root>/hegn-benchmark-<timestamp>`, boots a
one-member VM for it, measures it, stops the VM and deletes the project
directory again; the report records the cleanup and refuses to delete anything
that is not one of its own projects. `hegn benchmark <project>` leaves the VM
running so you can attach to it afterwards, and reports only that VM while
noting any others that are running. With several VMs active, the plain
invocation lists each one and totals their RAM; JSON exposes them as
`sessions`.

```text
hegn benchmark

VM        oc test (opencode)
Project   /home/you/Projects/hegn/oc test
Uptime    16m 30s
Guest     2 vCPU, 2.0 GiB configured (populated on demand)

PROCESS                PID        RSS    VIRTUAL
cloud-hypervisor    540642  404.4 MiB    2.0 GiB
virtiofsd           540638   11.8 MiB    2.1 GiB
console-relay       540643    5.2 MiB   23.3 MiB
host total                  421.3 MiB

DISK           ALLOCATED    APPARENT   FILES  PATH
images           2.1 GiB     2.2 GiB       4  /home/you/.local/share/hegn/images
snapshots            0 B         0 B       0  /home/you/.local/share/hegn/snapshots
VMs              1.9 GiB     2.0 GiB      12  /home/you/.local/share/hegn/vms
  dev            1.9 GiB     2.0 GiB      12  /home/you/.local/share/hegn/vms/dev
logs           116.0 KiB   105.2 KiB       4  /home/you/.local/state/hegn/logs
state          184.0 KiB     4.1 MiB      16  /home/you/.local/state/hegn
legacy v1      300.0 MiB   320.0 MiB      42  /home/you/.local/share/hegn/instances
disk total       4.0 GiB

Notes:
- guest RAM is populated on demand: 2 vCPU, 2.0 GiB configured
```

The `VMs` row is the active `vms_dir`. Each valid v2 VM below it gets a subrow
with its own name, allocated/apparent bytes and file count. A v1 instance
directory is reported separately as `legacy v1`, with one or more paths, because
it is not an active VM. `disk total` sums the active rows only; legacy v1 is
excluded and hegn never removes it. A VM with several projects shows one
`virtiofsd` row per member in the process section.

For scripts, `--json` reports `sessions[]` (each with `vm`),
`vm_disk[]` (per-VM usage), `vm_disk_allocated_bytes` (sum of active VMs),
`legacy_instances` (paths and bytes, or `null`), and for `--clean` a `cleanup`
object with `vm`, `vm_stopped` and `removed`.

What the columns mean:

- **RSS / VIRTUAL** — resident and virtual memory of the VM process and its
  descendants. `virtiofsd` re-executes itself into a sandboxed worker, so the
  role covers both processes. The TUI and an attached client are not recorded
  in `session.json`; add roughly 26 MiB and 7 MiB when they are open.
- **ALLOCATED / APPARENT** — blocks actually used on disk versus the sum of
  file sizes. They differ for sparse files such as `cloud-init.img` and the
  qcow2 image, and allocated is the number that matters for free space.
- **FILES** — number of regular files; symlinks are neither followed nor
  counted.
- **Notes** — stale session metadata, stopped processes and the configured
  guest RAM are reported here instead of failing the command.

Without a running VM the command still reports disk usage and the configured
guest cost. For deeper checks, inspect the raw sources:

```bash
hegn vm list --json                # state, vcpus, memory_mib, member counts
hegn vm show <vm> --json           # membership and config path
hegn images status                 # kernel/rootfs state and size
hegn doctor                        # resolved binaries and versions
hegn logs list                     # session logs and sizes

ps -o pid,rss,vsz,args -C cloud-hypervisor-static,virtiofsd,hegn
du -sh ~/.local/share/hegn/images ~/.local/share/hegn/snapshots \
       ~/.local/state/hegn/logs ~/.local/share/hegn/vms
df -h ~/.local/share/hegn
free -m
```

## Reducing the footprint

- Lower `memory_mib` (minimum 128) and `vcpus` (minimum 1) in `vm.toml` for
  small VMs; a shell-based VM does not need 2 GiB.
- Keep the TUI closed for long unattended runs; `hegn vm start`,
  `hegn vm list --json` and `hegn logs` need a few MiB, not ~26 MiB.
- Group projects that share a toolchain in one VM instead of starting several
  VMs. Each extra project costs one `virtiofsd`, not another guest.
- Delete snapshots you no longer need: `hegn snapshot list`,
  `hegn snapshot delete <name>`. Each one costs roughly the guest RAM.
- Prune logs: `hegn logs prune --keep 20`.
- Remove the pre-Cloud-Hypervisor leftovers (`rootfs.ext4`, `vmlinux.bin`)
  from `~/.local/share/hegn/images/` after migrating.
- Reset a bloated rootfs with `hegn vm rebuild <vm>`.
- Delete old VMs with `hegn vm delete <vm> --yes --delete-files` after
  unassigning anything you want to keep.
