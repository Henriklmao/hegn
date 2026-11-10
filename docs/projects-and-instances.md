# Projects and instances

A **project** is a directory on your host, usually a git repository. An
**instance** is a Cloud Hypervisor VM that runs for one project. A project can
have several instances at the same time, for example one normal instance and
several clones for parallel agents.

- [Projects](#projects)
- [Importing a project](#importing-a-project)
- [Deleting and restoring projects](#deleting-and-restoring-projects)
- [Instances](#instances)
- [Multi-agent clones](#multi-agent-clones)
- [Deleting instances](#deleting-instances)
- [Read-only workspace](#read-only-workspace)
- [Instance layout on disk](#instance-layout-on-disk)

---

## Projects

The project picker lists every directory under `projects_root` (default
`~/Projects/hegn`). It has:

- fuzzy filtering,
- git status badges,
- a history of recent sessions,
- one-key launch into `$EDITOR` or `lazygit`.

Create a new project with a template and a harness:

```bash
hegn project create my-app --harness opencode --package jq
```

`hegn project create` copies `~/.config/hegn/templates/AGENTS.md` if it exists.
Use `--template <PATH>` to pick a different file. See
[Configuration](configuration.md#agentsmd-templates).

Each project also gets a `hegn.toml`. It records the harness, the package list
and the template that was used. Per-project launch settings live there too (see
[Launch commands](harnesses.md#launch-commands-and-terminal-integrations)).

---

## Importing a project

You do not have to move your repositories. Import creates only a symlink inside
`projects_root`:

```bash
hegn project import ~/code/existing-repo
hegn project import /mnt/data/repo --name repo-picker-name
```

The original directory is never moved, copied or modified. In the TUI, press `h`
or `←` in the project list to do the same thing.

For safety, hegn refuses to import:

- `/` or `$HOME`,
- anything inside `projects_root` (it is already listed there),
- a parent directory of `projects_root`,
- anything that overlaps hegn's own state, snapshots, templates or config,
- a picker name that is not a plain directory name.

An existing picker entry is never overwritten. Discovery also skips symlinks that
point to those dangerous targets, including hegn's state directory.

---

## Deleting and restoring projects

On the dashboard, press `d` to delete a project. hegn first shows the git status,
then asks whether to delete the project files too:

- `y` removes the entry from the picker and removes the project's instances. The
  files stay. The project is hidden from future scans.
- `f` also deletes the project directory.

Imported projects only lose their symlink. The original directory is never
deleted.

The CLI equivalent:

```bash
hegn project delete my-app --keep-files --yes
hegn project delete my-app --delete-files --yes
hegn project restore my-app          # show a kept project again
```

---

## Instances

Start, stop and inspect the VM of a project:

```bash
hegn instance create ~/Projects/hegn/my-app --name my-app
hegn instance start my-app
hegn instance list
hegn instance status my-app --json
hegn instance pause my-app           # pause the VM
hegn instance resume my-app
hegn instance stop my-app
```

Each instance has its own settings in `hegn.toml`. You can edit them in the TUI
(`F5`) or from the CLI:

```bash
hegn instance config my-app          # print hegn.toml
hegn instance config my-app --path   # print the path
hegn instance config my-app --edit   # open it in $EDITOR
```

More on the settings and how the editor works in [Configuration](configuration.md).

---

## Multi-agent clones

Several agents on the same repository can overwrite each other's commits. Clones
solve this. A **clone** gets:

- its own independent git clone of the project,
- its own private rootfs copy.

Each agent works in its own clone. When the agents are done, you merge their
work or open pull requests from the clones.

```bash
hegn instance clone my-app --count 3          # my-app-agent-1..3, booted
hegn instance clone my-app --name review --no-start
```

In the TUI, press `M` in a project's screen to create clones. They are listed in
the instance list of that project and can be attached with `Enter`, like any
other instance.

Things to know before you clone:

- Clones are real git clones. They contain only **committed** history. Commit or
  stash uncommitted work first.
- Cloning a repository without commits gives an empty clone. hegn shows git's
  warning as a message instead of letting it break the TUI.

---

## Deleting instances

```bash
hegn instance delete my-app-agent-1 --delete-files
hegn instance delete my-app-agent-1 --keep-files
```

In the TUI, `hegn instance delete` asks twice. The first question is whether to
delete the instance. The second is whether to delete its cloned workspace files.

- A normal instance uses the original project as its workspace. That workspace is
  **never** deleted.
- `--keep-files` unregisters the instance and keeps the clone under
  `<instances_dir>/<name>-files-<timestamp>/`.
- `--delete-files` removes the clone.

Deleting never boots the VM and runs no host checks. You can always remove an
instance whose start failed. With `--yes --keep-files` the command also works
unattended in CI.

---

## Read-only workspace

Set `read_only_workspace = true` in an instance's `hegn.toml`, or toggle
**Read-only workspace** with `F5`. The project is then exported read-only:
virtiofsd gets `--readonly` and the guest mounts it with `ro`. The agent cannot
change the workspace.

---

## Instance layout on disk

```text
~/.local/share/hegn/instances/<name>/
  hegn.toml     per-instance settings
  run/          API, serial, relay and virtio-fs sockets, pids, lock
  profile/      copied profile, API keys and skills (never host links)
  rootfs.qcow2  private rootfs copy (created on first start)
  workspace/    only for cloned instances
```

Every instance gets its own rootfs copy on the first start. It uses a reflink on
btrfs or XFS and a full copy otherwise. This is required for running several VMs
at once. Sharing one writable disk between VMs would corrupt it. Existing
instances keep their copy until they are deleted. `hegn images download` only
refreshes the base image.
