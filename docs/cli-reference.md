# CLI reference

Every TUI action has a command-line counterpart. Use the commands in scripts,
over SSH, or in CI.

```bash
hegn [--config PATH] [--json] [COMMAND]
```

Global flags:

- `--config <PATH>` (or `HEGN_CONFIG`) selects the config file.
- `--json` prints machine-readable output for supported commands.

Run `hegn <command> --help` for the full options of each command.

- [Commands](#commands)
- [Exit codes](#exit-codes)
- [Environment variables](#environment-variables)
- [Examples](#examples)

---

## Commands

```text
  tui                              Open the interactive TUI (default)
  projects [QUERY]                 List projects with VM and primary markers (alias: ls)
  project create NAME              Scaffold a project (+ AGENTS.md, README, git)
          [--template PATH] [--vm VM | --solo]
  project import PATH [--name N]   Register an existing directory (symlink, no copy)
          [--vm VM | --solo]
  project info PROJECT             Show metadata, VM membership and guest path
  project assign PROJECT --vm VM   Assign a project to a VM
          [--read-only]
  project unassign PROJECT         Remove a project from its VM; the VM stays
  project fork PARENT [--name N]   Fork a project into the same VM
          [--mode clone|worktree] [--target PATH]
  project delete PROJECT [--delete-files | --keep-files] [--yes]
                                   Remove a project; a VM is never deleted
  project restore PROJECT          Show a project removed with --keep-files

  open PROJECT                     Boot/attach the project's VM serial console
          [--editor | --lazygit] [--no-branch] [--no-bar] [--command C]
  attach [VM|PROJECT] [--no-bar] [--command C]
                                   Attach to a running VM (alias: a)
  launch [VM|PROJECT] [--command NAME] [--integration I] [--no-bar]
                                   Launch the configured agent/command
  logs [list|path|show|prune]      Manage persistent serial logs (alias: log)
          [--vm VM]                Filter list/path/show by VM members

  vm list                          List VMs, state and member counts (alias: ls)
  vm create NAME [--harness H] [--vcpus N] [--memory MiB] [--package P]...
  vm show NAME                     Show one VM and its project members
  vm config NAME [--path | --edit]
  vm start NAME                    Boot the VM and every exported project
  vm stop NAME                     Stop the VM, relay and every member virtiofsd
  vm restart NAME                  Stop if running, then start
  vm apply NAME                    Restart to apply config or membership changes
  vm rebuild NAME [--yes]          Delete the private rootfs; next start copies fresh
  vm primary NAME PROJECT          Choose the member that /workspace points at
  vm projects NAME                 List members, read-only flags and guest paths
  vm shell NAME [--project PROJECT]
                                   Attach and start a shell/harness in a project
  vm delete NAME [--yes] [--delete-files]
                                   Unassign projects and remove the VM

  instance ...                     Removed v1 command group; prints the v2 mapping
                                   and exits non-zero (hidden from --help)

  snapshot list [--vm VM]          List snapshots with owner and format (alias: ls)
  snapshot create [NAME] --vm VM   Pause the VM, save its state, resume
  snapshot restore NAME [--vm VM]  Restore a snapshot into its owning VM
  snapshot delete NAME [--vm VM]   Delete a snapshot
  snapshot show NAME [--vm VM]     Show snapshot metadata
  check [--cwd PATH] CMD...        Inspect a command with tirith-core
  config show | path | init
  profile list | show HARNESS | path HARNESS [--edit] | init
  profile sync VM                  Refresh a VM's profile payload
  skills sync VM                   Copy host skills into the VM's profile copy
  skills sync-back VM [--dry-run] [--force]
  images download [--force]        Fetch missing pinned kernel/rootfs images
          [--allow-unverified]
  images status | verify           Show or re-hash the configured images
  network status | setup [--user U] [--dry-run] | reload [--dry-run] | teardown
  doctor [--fix]                   Check host prerequisites (exit 1 if required checks fail)
  benchmark [PROJECT] [--clean]    Measure host cost; boots a VM on demand
```

Notes:

- `project create` and `project import` without `--vm` ask whether to create a
  one-member VM named after the project. `--solo` does that without a prompt;
  non-interactive use requires `--vm` or `--solo`.
- `project assign` and `project unassign` never start, stop or delete a VM. A
  running VM keeps its current shares until `hegn vm apply`.
- `project fork` always creates a member of the parent's VM. Clone is the
  default; `--target` is valid only with `--mode worktree` and must stay inside
  the parent project.
- `vm delete` unassigns ordinary projects; only `--delete-files` removes the VM
  directory and its managed clone workspaces.
- `profile sync` and `skills sync` accept a VM name, a project name or a project
  path and operate on that VM's private profile copy.
- `snapshot create` requires `--vm <vm>`. The value can also be a project name or
  path that resolves to its VM. `--instance` remains a hidden deprecated alias for
  `--vm` and prints a warning. Without `--vm`, `snapshot list` covers every VM.

---

## Exit codes

| Command | Exit code |
| --- | --- |
| `check` | `0` allow, `1` block, `2` warn |
| `doctor` | `1` if a required check fails |
| `images verify` | `1` on a hash mismatch |
| `instance ...` | non-zero; prints the v1 → v2 mapping |

---

## Environment variables

Config overrides and the variables that hegn sets for the harnesses are listed in
[Configuration](configuration.md#environment-variables).

---

## Examples

Short examples for the most common tasks are in
[Getting started](getting-started.md#5-or-use-the-command-line). Other pages cover
the individual areas:

- projects, VMs and workspaces: [Projects, VMs and workspaces](projects-and-vms.md)
- attach and logs: [Sessions, attach and logs](sessions-and-logs.md)
- snapshots: [Snapshots](snapshots.md)
- profiles and launch: [Harnesses and profiles](harnesses.md)
- network: [Networking](networking.md)
- upgrading from v1: [Migrating to v2](migration-v2.md)
