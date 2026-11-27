# TUI guide

The terminal UI opens when you run `hegn` without a subcommand. This page lists
every key and explains what each screen does.

- [Dashboard](#dashboard)
- [Project screen](#project-screen)
- [VM screen](#vm-screen)
- [Help](#help-modal)
- [Snapshots](#snapshot-panel)
- [Terminal](#terminal-guest-serial-console)
- [Doctor](#doctor-modal)
- [Security prompt](#security-modal)
- [Theming](#theming)

Keys shown in a modal are also clickable buttons. Hovering a button highlights
it. Keyboard input clears the hover state.

---

## Dashboard

The dashboard is a project picker with a preview of the selected project. It
never boots a VM.

Each project row shows a VM badge: `⬢ <vm>` when the project is assigned, or
`unassigned` when it is not. A VM badge also shows the member count once a VM
hosts more than one project, for example `⬢ dev · 3p`.

The preview shows the selected project's VM state, harness, member count, newest
snapshot and guest mount path. For an unassigned project it shows the last time
it was opened and how to assign it.

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | Move the selection |
| `Enter` / `o` | Open the project screen and attach its VM over SSH |
| `/` | Fuzzy-filter projects |
| `1` / `b` | Open the project screen on the VM section |
| `2` / `3` / `4` | Open the project screen on snapshots / health / git |
| `v` | Open the project's VM screen |
| `c` | Create a project |
| `h` / `←` | Import an existing directory (symlink, no copy) |
| `a` | Assign the selected project to a VM |
| `f` | Fork the selected project (clone or worktree) |
| `D` | Detach the selected project from its VM |
| `n` / `V` | Create a VM |
| `d` | Delete a project. Shows the git status first, then asks whether to delete the files too. |
| `S` / `A` | Stop / apply (restart) the selected project's VM |
| `F4` | Global settings editor (mouse supported, `Ctrl-S` saves) |
| `F5` | VM settings editor for the selected project's `vm.toml` |
| `e` | Open the selected project in `$EDITOR` |
| `g` | Open the selected project in lazygit |
| `r` | Refresh projects and VMs |
| `F3` | Doctor modal (host checks, image download) |
| `?` / `F1` | Help |
| `q` | Quit |

---

## Project screen

The title bar shows `hegn ▸ <project> · <vm>`, the project path and the VM state.
The page has four numbered sections:

1. VM (state, tool profile, mount path, sibling projects),
2. snapshots,
3. health,
4. git.

The terminal is a single line until you open it.

| Key | Action |
| --- | --- |
| `1` / `2` / `3` / `4` | VM / snapshots / health / git |
| `Tab` | Cycle through the sections |
| `↑`/`↓`, `j`/`k` | Move inside the focused section |
| `Enter` (VM) | Attach the VM over SSH, starting it if needed |
| `Enter` (snapshots) | Restore the selected snapshot |
| `v` | Open the VM screen (projects, snapshots, tools, health) |
| `i` / `a` / `t` | Open or focus the terminal (starts the VM if needed) |
| `L` | Launch the configured agent or command in its terminal integration |
| `Esc` | Section view → split view; again back to the dashboard |
| `←` | Back to the dashboard (the VM keeps running) |
| `F2` | Leave the TUI and open an external SSH session via `hegn attach` |
| `Ctrl-B` / `Ctrl-]` | Back to the dashboard from any mode |
| `n` | New snapshot (snapshots section) |
| `M` / `f` | Fork the project into a clone or worktree |
| `S` / `A` | Stop / apply (restart) the VM |
| `d` | Detach the project (VM section) or delete the selected snapshot (snapshots section) |
| `D` | Delete the VM |
| `F5` | VM settings editor (`vm.toml`) |
| `F6` | Project settings (`hegn.toml`): launch command, integration, auto-launch |
| `e` / `g` | Editor / lazygit for the project |
| `F4` | Global settings editor |
| `?` / `F1` | Help |

Opening the terminal moves it to the right and takes 55% of the width. Pressing a
section number expands that section and shrinks the terminal to 30%. All four
sections stay visible, and the ones you did not select collapse into a single
header row. Focusing the terminal again restores 55%. The collapsed strip shows
the attached VM and whether it is running.

The VM section shows the primary marker, read-only status and sibling projects.
`Enter` attaches; the terminal title and header carry `project · vm`.

Destructive actions are never confirmed with `Enter`. Deleting a VM, snapshot or
project, restoring a snapshot, and allowing a flagged command require an explicit
key: `y` for most prompts, `a` for the security prompt. Pressing `Enter` keeps the
confirmation open.

The project delete dialog first shows the branch, the sync state and the number
of changed files. `y` removes the project from the dashboard and keeps its files;
`f` also deletes the directory. An imported project only loses its symlink. Its
original directory is never touched. The project's VM is never deleted here.

---

## VM screen

Press `v` on the dashboard or the project screen to open the VM screen. It has
four numbered sections:

1. Projects (members, kind, read-only and primary markers),
2. snapshots,
3. tools (harness, resources, extra packages, provisioning state),
4. health.

| Key | Action |
| --- | --- |
| `1` / `2` / `3` / `4` | Projects / snapshots / tools / health |
| `Tab` | Cycle through the sections |
| `↑`/`↓`, `j`/`k` | Move inside the focused section |
| `Enter` (projects) | Open the member project and attach the VM over SSH |
| `Enter` (snapshots) | Restore the selected snapshot |
| `Enter` (tools) | Attach the VM |
| `s` | Attach the VM over SSH, starting it if needed |
| `S` / `A` | Stop / apply (restart) the VM |
| `p` | Make the selected project the primary (`/workspace`) |
| `a` | Assign an unassigned project to this VM |
| `f` | Fork the selected project into a clone or worktree |
| `d` (projects) | Detach the selected project; the VM and other members stay |
| `d` (snapshots) | Delete the selected snapshot |
| `D` | Delete the VM |
| `n` (snapshots) | New snapshot |
| `n` | Create another VM |
| `F5` / `e` | Edit this VM's `vm.toml` |
| `F4` | Global settings editor |
| `F2` | External SSH attach |
| `r` | Refresh VMs, projects and snapshots |
| `Esc` / `←` | Back to the dashboard |
| `?` / `F1` | Help |

Creating a VM (`n` on the dashboard, or `n` outside the snapshots section here)
opens a form for name, harness, vCPUs, memory and extra packages. Editing an
existing VM uses `F5` or `e`, which opens `vm.toml` in the settings editor.

Assigning a project opens a candidate list and a read-only toggle. The first
project assigned to an empty VM applies automatically without a prompt, so the
VM boots and its harness setup starts immediately. Later assignments ask whether
to restart the VM now with `hegn vm apply`. Answer `n`/`Esc` to apply later; the
health section then shows `apply required · hegn vm apply <vm>` until an apply
succeeds. Forking opens the
fork modal: `Tab` toggles between the isolated clone default and the nested
worktree, `Enter` creates it, and `Esc` cancels. Detaching asks for confirmation
and never deletes project files.

Deleting a VM asks twice. The first confirmation names the harness and member
count and states that external projects are unassigned, never deleted. The
second offers `y` to delete files (the VM directory and managed workspaces) or
`n` to keep files (unassign every project and leave the VM directory in place).
`Esc` cancels.

There is no separate VM list screen. Projects are the dashboard rows, and the VM
screen lists the members of one VM.

---

## Help modal

Open it with `?` or `F1`. Type `/` to search the keys, titles and descriptions.
Every word you type must match.

- `Esc` stops searching and keeps the results.
- A second `Esc` clears the query.
- A third `Esc` closes the modal.

On narrow terminals the key sits on its own line and the description below it. A
scrollbar appears when the list is taller than the window. Drag it with the mouse,
or use `↑`/`↓`, `j`/`k` and `PgUp`/`PgDn`.

| Key | Action |
| --- | --- |
| `/` | Search the keybinding list |
| `Esc` | Stop searching, then clear the query, then close |
| `↑`/`↓`, `j`/`k` | Scroll the list |
| `PgUp` / `PgDn` | Scroll one page |

The doctor and security modals scroll the same way. Mouse wheel and dragging
work in every modal that overflows. All modals size themselves to the terminal
and keep their buttons visible, wrapping them onto another row when needed.
Long SSH attach failures open the same scrollable error modal instead of a
one-line toast, so the full recovery hint stays readable.

---

## Snapshot panel

| Key | Action |
| --- | --- |
| `Enter` | Restore the selected snapshot |
| `n` | Create a snapshot |
| `d` | Delete the selected snapshot |

More on snapshots in [Snapshots](snapshots.md).

---

## Terminal (guest serial console)

The embedded terminal is a pane inside the project screen and stays on the
**serial console**. It opens in **normal** mode. In normal mode, keys are hegn
shortcuts and nothing reaches the guest. Press `i` or `a` to enter **insert**
mode and type commands. `Esc` returns to normal mode, and then to the section
headers. The pane stays open.

The TUI does not embed SSH, tabs or a VT emulator. `Enter`/`s`/`F2` open an
external SSH session in the current terminal; each one gets its own guest PTY.
The serial pane is still there for boot, provisioning, recovery and checked
input. If a running VM has no `guest_ip`/`ssh_user` in its session record, the
TUI reports it and points at `hegn attach <vm> --serial`.

| Key | Action |
| --- | --- |
| `i` / `a` / `t` | Open or focus the terminal and enter insert mode |
| `L` | Launch the configured agent or command |
| `Esc` | Insert → normal. Normal → section headers. |
| `←` | Return to the dashboard (the VM keeps running) |
| `M` / `f` | Fork the open project into a clone or worktree |
| `Enter` | In insert mode: check the command with `tirith-core`, then send it |
| `F2` | Leave the TUI and open an external SSH session via `hegn attach` |
| `Ctrl-B` / `Ctrl-]` | Return to the dashboard from any mode |
| `PgUp` / `PgDn` | Scroll the output |
| `Ctrl-L` | Clear the scrollback |

The output pane shows the live scrollback. Line endings from the serial console
are handled correctly, so every line the guest printed stays visible.

---

## Doctor modal

| Key | Action |
| --- | --- |
| `s` | Set up everything that is shown as missing (after a confirmation) |
| `d` | Download and verify the missing VM images (in the background) |
| `r` | Run all checks again |
| `Esc` / `q` | Close |

The doctor opens on the first start when setup is available or a required check
fails. It also opens after a VM start or attach failure.

`s` is the one setup entry point. It repairs root-owned or unreadable state from
an earlier sudo run, creates missing directories and the config, downloads the
pinned images, and applies the control/guarded network through `sudo` or
`pkexec` when needed. The report also checks `ssh`/`ssh-keygen`, the
network marker and the control TAPs. Every step can be repeated safely. Image downloads run in
the background, and a notification in the bottom-right corner shows the progress.
The same work is available from the command line as `hegn doctor --fix`.

Optional findings are marked `!`. That includes lazygit, `$EDITOR` and the legacy
v1 checks. They never make the report fail, and no legacy state is deleted.

---

## Security modal

| Key | Action |
| --- | --- |
| `a` | Allow the flagged command once |
| `c` | Create a git checkpoint, then allow the command |
| `d` / `Esc` | Deny the command |

`Enter` never allows a flagged command. It leaves the prompt open. Every action
is available as a button in the modal and in the footer. More about the checks in
[Security](security.md).

---

## Theming

The TUI uses your terminal's own foreground and background colours and its ANSI
palette. hegn does not paint a full-screen background. Your existing dark or light
terminal theme therefore works unchanged. Only selected rows, badges and
diagnostic cells use a small ANSI background colour.
