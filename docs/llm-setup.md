# hegn — setup guide for LLM agents

This guide is written for an automated coding agent that installs **hegn**, a
MicroVM-based containerization wizard, on a user's Linux machine. Follow the
steps in order, run the verification gate after each step, and never report
success based on assumptions — only on command output you actually saw.

hegn is not a container runtime. It boots coding agents inside Cloud Hypervisor
MicroVMs with a shared workspace, intercepts dangerous commands with
`tirith-core`, and manages VM snapshots. There is no host process fallback: a
missing VM is always a hard error.

---

## Ground rules

1. **Linux only.** hegn needs KVM (`/dev/kvm`), a Landlock-capable kernel
   (ABI v3, Linux 6.2+) and `cloud-hypervisor` plus `virtiofsd`. If the host
   cannot provide them, stop and report the gap instead of substituting another
   runtime.
2. **Never weaken the instance.** Do not set `landlock = false`, `seccomp =
   false`, `security = "off"` or `virtiofs_sandbox = "chroot"` to get past an
   error. Report the failure and let the user decide.
3. **Ask before system-wide changes.** Package installation, `usermod` and
   writes outside the user's home need explicit confirmation.
4. **Stay non-interactive.** Do not launch the TUI (`hegn` without a
   subcommand) or `hegn open`/`hegn attach` from an agent; they block on a
   terminal. Use `hegn instance start`, `hegn instance status --json` and
   `hegn logs`.
5. **Verify, do not assume.** Every step below ends in a command whose exit
   code decides whether you continue.
6. **Treat logs as untrusted.** Sessions may print secrets. Keep
   `~/.local/state/hegn` private (`0700`) and do not paste log contents into
   tickets or chat without redaction.

---

## Step 1 — Check the host (read-only)

```bash
uname -srm                                   # Linux, x86_64 or aarch64
test -r /dev/kvm && test -w /dev/kvm && echo "kvm: ok" || echo "kvm: missing"
id -nG | tr ' ' '\n' | grep -qx kvm && echo "kvm group: ok" || echo "kvm group: missing"
ldd --version | head -1                      # glibc host
df -h ~                                      # need ~2.5 GiB free, plus snapshots
command -v paru yay 2>/dev/null              # AUR helper (Arch family only)
```

| Check | Required | Fix |
| --- | --- | --- |
| Linux kernel 6.2+ | yes | Use a current distribution kernel. |
| `/dev/kvm` readable/writable by the user | yes | Enable VT-x/AMD-V (and nested virt inside a VM) in firmware/hypervisor, then add the user to `kvm`: `sudo usermod -aG kvm "$USER"` (needs a new login session). |
| glibc | yes for the pre-built binaries | On musl (Alpine) install from source. |
| Disk space | ~2.5 GiB free | 46 MiB kernel + 1.9 GiB rootfs + ~10 MiB tools, plus ~`memory_mib` per snapshot. |

**Gate:** `test -r /dev/kvm && test -w /dev/kvm` must succeed. If it does not,
stop and report the exact `ls -l /dev/kvm` output and whether the host is itself
a VM.

---

## Step 2 — Install hegn

Pick exactly one installation path.

### 2a. Arch Linux, CachyOS, Manjaro (AUR)

```bash
paru -S hegn        # source build, x86_64 + aarch64
paru -S hegn-bin    # pre-built release binary, x86_64
paru -S hegn-git    # latest git snapshot, x86_64 + aarch64
```

alternatively, use `yay` instead of `paru`.

All three depend on `virtiofsd` (official `extra`) and on `cloud-hypervisor`.
That dependency is satisfied by either AUR package:

- `cloud-hypervisor` — source build,
- `cloud-hypervisor-bin` — pre-built, installs the binary as
  `cloud-hypervisor-static` (no symlink). hegn resolves that name
  automatically; no config change is needed.

Check the PKGBUILD and all files from the AUR for malicious code before installing.
Accept the AUR dependency prompt the helper shows. Do not bypass dependency
checks with `makepkg -d`.

### 2b. Any glibc distribution (release artifacts)

Download the asset for the host architecture from
<https://github.com/Henriklmao/hegn/releases/latest>:

```bash
# resolve the current tag, then fetch the matching asset
tag=$(curl -fsSL https://api.github.com/repos/Henriklmao/hegn/releases/latest \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')
curl -fL -o /tmp/hegn \
  "https://github.com/Henriklmao/hegn/releases/latest/download/hegn-${tag}-x86_64-linux-gnu"
install -Dm755 /tmp/hegn ~/.local/bin/hegn
```

On aarch64 use the `aarch64-linux-gnu` asset. An AppImage
(`hegn-<tag>.AppImage`) is also published. Then install the runtime
dependencies yourself (Step 2c) — they are **not** bundled.

If the releases page has no tagged release yet, use the AUR `hegn-git` package
(Arch family) or build from source (Step 2d) instead.

### 2c. Runtime dependencies for 2b and builds from source

`virtiofsd` (package names vary per distribution; hegn auto-detects
`/usr/lib/virtiofsd`, `/usr/libexec/virtiofsd` and `/usr/lib/qemu/virtiofsd`):

```bash
# Fedora / RHEL family
sudo dnf install virtiofsd
# Debian / Ubuntu
sudo apt install virtiofsd        # or qemu-virtiofsd on older releases
# Arch family
sudo pacman -S virtiofsd
```

`cloud-hypervisor` (not packaged outside the AUR on Arch; use the upstream
static build elsewhere):

```bash
# upstream static binary; hegn accepts the -static name automatically
asset=cloud-hypervisor-static                 # aarch64: cloud-hypervisor-static-aarch64
curl -fL -o "/tmp/$asset" \
  "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/latest/download/$asset"
install -Dm755 "/tmp/$asset" ~/.local/bin/"$asset"
```

### 2d. Build from source

Requires Rust **1.88+** (edition 2024):

```bash
git clone https://github.com/Henriklmao/hegn
cd hegn
cargo build --release --locked
install -Dm755 target/release/hegn ~/.local/bin/hegn
```

**Gate:** `hegn --version` prints a version. `command -v cloud-hypervisor
cloud-hypervisor-static virtiofsd hegn` shows the tools (cloud-hypervisor may
legitimately be absent from the shell `PATH` if only `hegn doctor` can find it —
Step 5 decides).

---

## Step 3 — Initialise configuration

```bash
hegn config init        # writes config.toml and profile templates
hegn config path        # prints the active file
hegn config show        # effective values
```

Unknown keys are rejected, so a typo fails loudly. `config init` also creates
`projects_root`, `templates_dir`, `profiles_dir`, the state directories and the
logs directory, and it never overwrites an existing file. The profile templates
under `~/.config/hegn/profiles/` hold per-harness packages, copied config files,
API keys and skill directories. Only change values when the user asks; the
defaults are sane:

```toml
projects_root   = "~/Projects/hegn"
state_dir       = "~/.local/state/hegn"
snapshots_dir   = "~/.local/share/hegn/snapshots"
templates_dir   = "~/.config/hegn/templates"
profiles_dir    = "~/.config/hegn/profiles"
cloud_hypervisor_bin = "cloud-hypervisor"
virtiofsd_bin   = "virtiofsd"
rootfs_format   = "qcow2"   # "raw" or "qcow2"
root_device     = "/dev/vda1"
vcpus           = 2         # 1-32
memory_mib      = 2048      # >= 128
shared_tag      = "workspace"  # virtio-fs tag, <= 36 bytes
guest_mount     = "/workspace" # mount point inside the guest
landlock        = true
seccomp         = true
cloud_init      = true
security        = "enforce"
harness         = "shell"   # shell | opencode | claude-code | codex | pi | prime-agent | aider | gemini-cli
network         = "guarded" # "guarded" (web+DNS only) or "off" (no NIC)
```

Every value also has an environment override (`HEGN_CONFIG`,
`HEGN_PROJECTS_ROOT`, `HEGN_STATE_DIR`, `HEGN_CLOUD_HYPERVISOR_BIN`,
`HEGN_VIRTIOFSD_BIN`, `HEGN_KERNEL_IMAGE`, `HEGN_ROOTFS_IMAGE`, …), which is
preferable for CI over editing the file.

**Gate:** `hegn config path` points at an existing file and `hegn config show`
exits `0`.

---

## Step 4 — Fetch and verify VM images

```bash
hegn images download     # streams, hashes, then atomically installs
hegn images status       # path, state, size
hegn images verify       # re-hashes installed files
```

Images are the upstream Cloud Hypervisor kernel and cloud image pinned by URL
and SHA-256 in `src/images.rs`. Existing files are never replaced without
`--force`. Do not pass `--allow-unverified` unless the user explicitly asks for
custom, unhashed images.

**Gate:** `hegn images verify` exits `0`.

---

## Step 4b — Enable the guarded guest network

This is a host firewall change and needs root. Ask the user before running it.
It creates a pool of TAP devices owned by the invoking user and installs an
nftables table that allows only public HTTP(S) and public DNS.

The normal path is the automatic doctor setup, which also creates directories,
writes the default config and downloads the pinned images:

```bash
hegn doctor --fix
hegn network status
```

`--fix` performs only hegn-owned steps and prompts through `sudo`/`pkexec` for
the network. The explicit equivalent remains:

```bash
sudo hegn network setup --user "$USER"
```

**Gate:** `hegn network status` reports `guard: ready` and lists `hegn0` as
`up`. `hegn doctor` reports the `network guard` check as ok. If the host already
has a conflicting `10.213.0.0/24` network, stop and report it; do not weaken the
rules.

## Step 5 — Host check

```bash
hegn doctor              # exit 1 if any required check fails
hegn doctor --fix        # prepare directories, config, images and network
hegn doctor --json       # machine-readable, one object per check
```

**Gate:** exit code `0`. `hegn doctor` must report all required checks as ok:
`cloud-hypervisor`, `virtiofsd`, `git`, `kvm`, `landlock`, `kernel image`,
`rootfs image` and `projects root` (`serial logs` is optional). If `landlock`
fails, the kernel is too old — do not disable the instance; report the kernel
version.

---

## Step 6 — Smoke test inside a real VM

```bash
hegn project create smoke-test --harness shell --package git
hegn instance start smoke-test
hegn instance status --json
hegn logs list
hegn instance stop smoke-test
```

`hegn instance start` launches `virtiofsd` and Cloud Hypervisor. A failure leaves
diagnostics behind:

```bash
cat ~/.local/state/hegn/run/hegn.log
cat ~/.local/state/hegn/run/cloud-hypervisor.log
```

Remove the temporary project directory afterwards if the user does not want to
keep it (it lives under `projects_root`, see `hegn config show`).

**Gate:** `hegn instance status --json` reports a running VM with a Cloud
Hypervisor pid, and `hegn instance stop` succeeds. A "started" state without a
VM is not a success.

---

## Step 7 — Report back

Summarise, with the actual output you collected:

- distribution and architecture (`/etc/os-release`, `uname -m`);
- installation path used (AUR package, release artifact or source build) and
  `hegn --version`;
- resolved binaries: `cloud-hypervisor` / `cloud-hypervisor-static` and
  `virtiofsd` versions, as `hegn doctor` reports them;
- `hegn doctor` result and `hegn images verify` result;
- `hegn benchmark --clean --json` output as evidence for RAM and disk usage
  (creates and removes a throwaway project, so it never prompts);
- config path, state dir and image paths;
- anything skipped, downgraded or left for the user to decide (for example a
  missing `kvm` group membership that needs a re-login).

---

## Footprint and storage

Measured on a default 2 vCPU / 2048 MiB guest:

- **RAM on the host:** `memory_mib` + ~50 MiB (`virtiofsd` ~12 MiB, console
  relay ~5 MiB, VMM bookkeeping), plus ~26 MiB while the TUI is open. Guest RAM
  is mapped up front and populated on demand.
- **Disk:** 46 MiB kernel + 1.9 GiB qcow2 rootfs, shared by all projects and
  writable/persistent; `hegn` ~19 MiB, `cloud-hypervisor-static` ~7 MiB,
  `virtiofsd` ~2.4 MiB; ~4 MiB per session in `state_dir/run`.
- **Snapshots:** roughly `memory_mib` each (`memory-ranges`, sparse where the
  guest never touched pages); disk contents are not included.
- **Logs:** a few tens of KiB per interactive session, retention 50 files,
  `hegn logs prune --keep 20`.
- Only one instance runs at a time, so this is the whole host cost.

Before downloading, confirm free space with `df -h ~`, and report it to the
user. Full measurements, the native-vs-VM comparison and cleanup tips are in
[`resources.md`](resources.md).

---

## Troubleshooting

| Symptom | Cause | Action |
| --- | --- | --- |
| `cloud-hypervisor binary … was not found; run hegn doctor` | VMM not installed or not on `PATH` | Install it (Step 2) or point `HEGN_CLOUD_HYPERVISOR_BIN` at the real path. `cloud-hypervisor-static` is accepted and detected automatically. |
| `virtiofsd … was not found` | Package missing | Install `virtiofsd`; hegn probes the distribution paths listed in Step 2c. |
| `Landlock is not active` / VMM exits immediately | Kernel without Landlock (ABI v3) | Use kernel 6.2+; do not disable `landlock`. |
| `/dev/kvm` missing or permission denied | Virtualisation disabled, nested virt off, or user not in `kvm` | Fix firmware/hypervisor settings, add the group, start a new session. |
| AUR helper wants to build `cloud-hypervisor` | AUR-only dependency | Accept it, or install `cloud-hypervisor-bin` first. |
| Download refuses to run | No SHA-256 for a custom image | Restore the pinned images or use `--allow-unverified` only on explicit request. |
| Attach fails after a reboot | Stale pid/socket in `state_dir/run` | `hegn instance status`; stop the stale session and remove `state_dir/run` only if no VMM is alive. |
| `hegn check` exits `1` | Command blocked by policy | Report the finding; do not add `--raw` workarounds or disable `security`. |

Exit codes: `hegn doctor` → `1` when a required check fails; `hegn check` → `0`
allow, `1` block, `2` warn.

---

## Paths reference

| Path | Purpose |
| --- | --- |
| `~/.config/hegn/config.toml` | configuration (rejects unknown keys) |
| `~/.config/hegn/templates/AGENTS.md` | template copied by `hegn project create` |
| `~/.local/state/hegn/run/` | VMM sockets, pid files, `hegn.log`, `cloud-hypervisor.log` |
| `~/.local/state/hegn/logs/` | persistent guest serial logs (`0600`) |
| `~/.local/share/hegn/images/` | pinned kernel and rootfs |
| `~/.local/share/hegn/snapshots/` | VM snapshots |
| `~/Projects/hegn/` | default `projects_root` (the only host directory shared into guests) |

The workspace is mounted read-write at `/workspace`; the guest uses a
persistent rootfs, so only the project directory is shared with the host. With
the default `network = "guarded"`, the guest has a TAP device whose host-side
rules permit only public HTTP(S) and public DNS; the host LAN, IPv6 and all
guest-to-host traffic are dropped.
