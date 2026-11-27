# Projects, VMs and workspaces

hegn separates three things:

| Concept | Role | Cardinality |
| --- | --- | --- |
| **Project** | A host directory you work in; the primary navigation object | many |
| **VM** | A named tool profile and runtime: harness, packages, CPU/RAM, rootfs, network, snapshots, SSH access | many |
| **Workspace** | A concrete checkout exported to a VM: the project itself, a clone, or a git worktree | one per project membership |

A project belongs to **at most one VM**. A VM hosts **zero or more projects**.
Every exported project gets its own sandboxed `virtiofsd` and its own guest
mount, so tools are paid for once per VM while projects stay separate exports.

The VM is the trust boundary. Projects in the same VM share a guest user, a
toolchain, an SSH account and each other's mounted files. Read-only prevents
writes, not reads. See [Security](security.md#projects-in-one-vm-are-mutually-visible).

- [Projects](#projects)
- [VMs](#vms)
- [Assigning and detaching projects](#assigning-and-detaching-projects)
- [Importing a project](#importing-a-project)
- [Guest layout](#guest-layout)
- [Parallel workspaces](#parallel-workspaces)
- [Deleting projects](#deleting-projects)
- [Deleting a VM](#deleting-a-vm)
- [Read-only projects](#read-only-projects)
- [On-disk layout](#on-disk-layout)

---

## Projects

The project picker lists every registered project under `projects_root`
(default `~/Projects/hegn`), plus imported projects outside it. It has fuzzy
filtering, git status, VM badges, session history and one-key launch into
`$EDITOR` or `lazygit`.

Create a project and assign it to an existing VM:

```bash
hegn vm create dev --harness opencode
hegn project create my-app --vm dev
```

Or create a one-member VM without a prompt:

```bash
hegn project create my-app --solo
```

`--vm` and `--solo` conflict. Without either, an interactive `hegn project
create` asks whether to create a one-member VM; a non-interactive run requires
one of the flags.

`hegn project create` copies `~/.config/hegn/templates/AGENTS.md` if it exists.
Use `--template <PATH>` to pick a different file. See
[Configuration](configuration.md#agentsmd-templates).

Inspect membership and the guest path:

```bash
hegn project info my-app
hegn project info my-app --json
```

A repo-local `hegn.toml` records only presentation preferences:

```toml
launch = "review"
integration = "herdr"
auto_start = true
```

Membership and read-only status live in the private registry. Resources and the
harness live in the VM's `vm.toml`. A file inside the project cannot change what
is exported or how it is mounted.

---

## VMs

```bash
hegn vm create dev --harness opencode --vcpus 8 --memory 16384 --package rustup
hegn vm list
hegn vm show dev
hegn vm config dev             # print vm.toml
hegn vm config dev --edit      # edit it in $EDITOR
```

A VM has no projects when it is created. It can exist while you prepare it, but
it cannot boot until at least one project is assigned.

```bash
hegn vm start dev
hegn vm stop dev
hegn vm restart dev
hegn vm apply dev     # restart to pick up config or membership changes
hegn vm rebuild dev   # new private rootfs from the base image
```

The harness, extra packages, vCPUs, memory, security and snapshot policy belong
to the VM. Changing `harness` or `extra_packages` does not rewrite an existing
rootfs: install tools inside the guest or rebuild the rootfs with
`hegn vm rebuild`. Manual guest installs persist in the shared rootfs.

`hegn vm apply` is an explicit restart because it kills running agents. Use it
after assigning or unassigning a project while the VM is running.

---

## Assigning and detaching projects

```bash
hegn project assign ~/code/parser --vm dev
hegn project assign ~/code/cli --vm dev --read-only
hegn vm projects dev
hegn project unassign ~/code/cli
```

- A project can be assigned to one VM at a time. Assigning it elsewhere moves
  it.
- The first project assigned to an empty VM becomes `primary` automatically. In
  the TUI that first assignment also applies the VM immediately, so it boots and
  starts harness provisioning; later assignments ask before applying.
- `hegn vm primary dev parser` chooses which project `/workspace` points at.
  The change takes effect on the next start; a running VM warns until
  `hegn vm apply`.
- Worktrees inherit their parent's VM and cannot be assigned or unassigned
  independently.
- A project assigned while the VM runs stays unmounted until the next start.
  Attach still works and prints a warning.

hegn refuses to boot a VM when:

- a member directory is missing,
- two members have the same canonical path,
- one member is an ancestor or descendant of another (except a worktree inside
  its named parent),
- the VM has more than `max_projects_per_vm` exports,
- two running VMs export the same canonical path.

The error names the missing or conflicting projects and the command to fix it.
There is no partial boot and no silent subset.

---

## Importing a project

You do not have to move your repositories. Import creates only a symlink inside
`projects_root` and registers the canonical target:

```bash
hegn project import ~/code/existing-repo --vm dev
hegn project import /mnt/data/repo --name repo-picker-name --vm dev
```

The original directory is never moved, copied or modified. In the TUI, press `h`
or `←` in the project list to do the same thing.

For safety, hegn refuses to import or export:

- `/` or `$HOME`,
- anything inside `projects_root` (it is already listed there),
- a parent directory of `projects_root`,
- anything that overlaps hegn's state, snapshots, VMs, templates, profiles or
  config,
- a picker name that is not a plain directory name.

An existing picker entry is never overwritten. Discovery also skips symlinks
that point to those dangerous targets.

---

## Guest layout

For member index `i`, the host uses:

```text
tag        = "hegn-prj-<i>"          # configurable prefix
socket     = "<vms_dir>/<vm>/run/virtiofs-<i>.sock"
guest path = "/projects/<slug>"
```

The slug is the sanitized project name: ASCII letters, digits, `-`, `_` and `.`,
lowercased and limited to 24 bytes. Duplicate slugs get `-2`, `-3`. Members are
sorted by canonical host path, so the device order is stable across boots.

Inside the guest:

```text
/projects/parser   -> host ~/code/parser
/projects/cli      -> host ~/code/cli
/workspace         -> symlink to /projects/parser   (the VM's primary project)
/run/hegn/profile  -> the VM's copied harness profile
```

`/workspace` is a guest-side symlink created at boot. It is not a separate
export and cannot point outside the configured project base (default
`/projects`). A custom base changes the mount paths; `/workspace` points at the
primary project below it. `/workspace` and paths under it are reserved and
cannot be the project base. The login shell starts there when it exists.

Starting any member starts the whole VM. `hegn attach` opens SSH over the VM's
control TAP by default; every session gets its own guest PTY, so several agents
can run in parallel. The serial console remains the boot and recovery channel
(`hegn attach <vm> --serial`) and is shared. Multiple SSH sessions still share
one guest user and can read each other's projects; use separate VMs for a hard
boundary.

---

## Parallel workspaces

A fork is another project member of the same VM, so it inherits the VM's tools
and rootfs. `clone` is the default.

### Clone (default)

```bash
hegn project fork parser --name parser-fix-tests
```

- `git clone` into `<vms_dir>/<vm>/workspaces/<name>`.
- Independent `.git`, refs, hooks and object store, so one agent cannot corrupt
  the source repository.
- Its own sandboxed `virtiofsd` and mount. It can be read-only, detached or
  deleted independently.
- `kind = "clone"`, `managed = true`, `parent = "parser"`.
- hegn uses `git clone --local` (hardlinks when the filesystems match) and falls
  back to a normal clone across devices.

A clone contains only **committed** history. Commit or stash uncommitted work
first. hegn refuses to fork a repository without commits with `has no git
commit to fork; create an initial commit first`.

### Worktree (opt-in, shared git state)

```bash
hegn project fork parser --mode worktree --name parser-experiment
hegn project fork parser --mode worktree --name parser-experiment \
  --target ~/code/parser/.hegn/worktrees/parser-experiment
```

- The worktree lives inside the parent project's exported tree, by default at
  `<parent>/.hegn/worktrees/<name>`.
- It is **not** a separate export. It rides the parent's share, so it adds no
  `virtiofsd` or `--fs` device.
- Its `vm` is inherited from the parent. A read-only parent cannot host
  worktrees. Nested worktrees (a worktree of a worktree) are refused.
- hegn verifies it with `git worktree list` and never deletes it implicitly.
  `hegn project delete` uses `git worktree remove` after confirmation.

Worktrees share refs, hooks and the object store with the parent. A destructive
git command in one worktree can affect the parent. That is the opt-in tradeoff
for near-zero disk and instant creation; clones remain the isolation-preserving
default.

---

## Deleting projects

```bash
hegn project delete my-app --keep-files --yes
hegn project delete my-app --delete-files --yes
hegn project restore my-app          # show a kept project again
```

hegn prints the git status first, then applies the file policy:

- `--keep-files` removes the project from the picker and registry but keeps its
  files. A real directory is hidden from future scans.
- `--delete-files` also removes the project directory.

Imported projects only lose their symlink; the original directory is never
deleted. Deleting a project never deletes its VM, its siblings or other members.
A running VM keeps its current shares until `hegn vm apply`.

Managed clones use `hegn project delete <name>` without a file-policy flag: the
managed directory is removed after confirmation. `--keep-files` is refused for
clones. Worktrees use `git worktree remove` and keep the parent.

If a deleted project was primary, hegn moves `/workspace` to the next exported
member (canonical-path order) or leaves the VM without a primary. A running VM
needs `hegn vm apply` for the new symlink.

---

## Deleting a VM

```bash
hegn vm delete dev --yes
hegn vm delete dev --yes --delete-files
```

`hegn vm delete` stops the VM, unassigns its ordinary projects and removes the
VM configuration. Projects are never deleted. Managed clone and worktree
registrations are removed because they depend on the VM.

- Without `--delete-files`, the VM directory, private rootfs and managed
  workspaces are kept on disk.
- With `--delete-files`, the VM directory and its managed clone workspaces are
  deleted.

---

## Read-only projects

```bash
hegn project assign ~/code/vendor --vm dev --read-only
```

Read-only is enforced twice:

1. `virtiofsd --readonly` for that project's share.
2. The guest mounts it with the `ro` option.

The agent cannot change the workspace. It can still read it, and it can read
every other project mounted in the same VM. A parent with worktrees cannot be
made read-only.

Toggle read-only by re-running `hegn project assign <project> --vm <vm>` with or
without `--read-only`. The change takes effect on the next start.

---

## On-disk layout

```text
<vms_dir>/<vm>/vm.toml                  VM profile (0600)
<vms_dir>/<vm>/run/                     sockets, pids, session and membership
<vms_dir>/<vm>/rootfs.<rootfs_format>   private rootfs (first start; .qcow2 or .raw)
<vms_dir>/<vm>/workspaces/<name>/       hegn-managed clone workspaces

<state_dir>/projects.toml               private project registry (0600)
<state_dir>/logs/                       persistent serial logs

<projects_root>/<name> -> <target>      imported project symlink
```

`vms_dir` defaults to `~/.local/share/hegn/vms`; `state_dir` defaults to
`~/.local/state/hegn`. The registry is the single source of truth for
membership, read-only flags and fork lineage. It is written atomically under a
lock, is never exported, and cannot be edited by the guest.

`vm.toml` deliberately does not list projects. Membership is derived from the
registry, so there is only one source of truth.

Every VM gets one rootfs copy on first start, using a reflink on btrfs or XFS and
a full copy otherwise. A shared explicit `rootfs_image` is refused when two VMs
point at the same writable disk. `hegn images download` only touches the shared
base image. Existing VM rootfs copies are not changed.
