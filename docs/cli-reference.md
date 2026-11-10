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

---

## Commands

```text
  tui                              Open the interactive TUI (default)
  projects [QUERY]                 List projects under projects_root (alias: ls)
  project create NAME              Scaffold a project (+ AGENTS.md, README, git, hegn.toml)
          [--harness H] [--template PATH] [--package P[,P...]]
  project import PATH [--name N]   Symlink an existing directory into projects_root
  project info PROJECT             Show metadata and git status
  project delete PROJECT [--delete-files | --keep-files] [--yes]
                                   Remove a project (+ its instances); --keep-files hides it
  project restore PROJECT          Show a project removed with --keep-files
  open PROJECT                     Boot/attach the VM serial console
          [--editor | --lazygit] [--no-branch] [--no-bar] [--command C]
  attach [PROJECT|INSTANCE] [--no-bar] [--command C]
                                   Attach to a running VM (alias: a)
  launch [PROJECT|INSTANCE] [--command NAME] [--integration I] [--no-bar]
                                   Launch the configured agent/command
  logs [list|path|show|prune]      Manage persistent serial logs (alias: log)
  instance list                    List instances (alias: ls)
  instance create PROJECT [--name N] [--harness H] [--clone]
  instance clone SOURCE [--name N] [--count N] [--harness H] [--no-start]
  instance start INSTANCE [--harness H] [--clone]
  instance stop [INSTANCE]
  instance status [INSTANCE]
  instance pause | resume INSTANCE
  instance config INSTANCE [--path | --edit]
  instance delete INSTANCE [--delete-files | --keep-files] [--yes]
  snapshot list                    List snapshots (alias: ls)
  snapshot create [NAME] [--instance INSTANCE]
  snapshot restore NAME [--instance INSTANCE] | delete NAME | show NAME
  check [--cwd PATH] CMD...        Inspect a command with tirith-core
  config show | path | init
  profile list | show HARNESS | path HARNESS [--edit] | init
  profile sync INSTANCE            Refresh an instance's profile payload
  skills sync INSTANCE             Copy host skills into the instance copy
  skills sync-back INSTANCE [--dry-run] [--force]
  images download [--force]        Fetch missing pinned kernel/rootfs images
          [--allow-unverified]
  images status | verify           Show or re-hash the configured images
  doctor                           Check host prerequisites (exit 1 if required checks fail)
  benchmark [PROJECT] [--clean]    Measure host cost; boots a VM on demand
```

---

## Exit codes

| Command | Exit code |
| --- | --- |
| `check` | `0` allow, `1` block, `2` warn |
| `doctor` | `1` if a required check fails |
| `images verify` | `1` on a hash mismatch |

---

## Environment variables

Config overrides and the variables that hegn sets for the harnesses are listed in
[Configuration](configuration.md#environment-variables).

---

## Examples

Short examples for the most common tasks are in
[Getting started](getting-started.md#4-or-use-the-command-line). Other pages cover
the individual areas:

- projects and instances: [Projects and instances](projects-and-instances.md)
- attach and logs: [Sessions, attach and logs](sessions-and-logs.md)
- snapshots: [Snapshots](snapshots.md)
- profiles and launch: [Harnesses and profiles](harnesses.md)
- network: [Networking](networking.md)
