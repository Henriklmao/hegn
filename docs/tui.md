# TUI guide

The terminal UI opens when you run `hegn` without a subcommand. This page lists
every key and explains what each screen does.

- [Dashboard](#dashboard)
- [Project screen](#project-screen)
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

The dashboard is a project picker with a short preview of the selected project.
It never boots a VM.

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | Move the selection |
| `Enter` / `o` | Open the project screen (does not boot) |
| `/` | Fuzzy-filter projects |
| `b` / `2` | Open the project screen on instances |
| `s` / `3` | Open the project screen on snapshots |
| `c` | Create a project |
| `h` / `←` | Import an existing directory (symlink, no copy) |
| `d` | Delete a project. Shows the git status first, then asks whether to delete the files too. |
| `F4` | Global settings editor (mouse supported, `Ctrl-S` saves) |
| `e` | Open the selected project in `$EDITOR` |
| `g` | Open the selected project in lazygit |
| `r` | Refresh projects and instances |
| `F3` | Doctor modal (host checks, image download) |
| `?` / `F1` | Help |
| `q` | Quit |

---

## Project screen

The title bar shows `hegn ▸ <project>`, the path and the VM state. The page has
four numbered sections:

1. instances,
2. snapshots,
3. health,
4. git.

The terminal is a single line until you open it.

| Key | Action |
| --- | --- |
| `1` / `2` / `3` / `4` | Instances / snapshots / health / git |
| `Tab` | Cycle through the sections |
| `↑`/`↓`, `j`/`k` | Move inside the focused section |
| `Enter` (instances) | Attach to the selected instance, or boot the default one |
| `Enter` (snapshots) | Restore the selected snapshot |
| `i` / `a` / `t` | Open or focus the terminal (starts an instance if needed) |
| `L` | Launch the configured agent or command in its terminal integration |
| `Esc` | Insert → normal. Normal → section headers (the pane stays at 55%). |
| `←` | Back to the dashboard (the VM keeps running) |
| `F2` | Leave the TUI and start the real `hegn attach` client |
| `Ctrl-B` / `Ctrl-]` | Back to the dashboard from any mode |
| `n` | New instance (instances section) or new snapshot (snapshots section) |
| `M` | Create multi-agent clones for this project |
| `S` | Stop the selected instance |
| `d` | Delete the selected instance or snapshot |
| `F5` | Instance settings editor |
| `F6` | Project settings (`hegn.toml`): launch command, integration, auto-launch |
| `e` / `g` | Editor / lazygit for the project |
| `F4` | Global settings editor |
| `?` / `F1` | Help |

Opening the terminal moves it to the right and takes 55% of the width. Pressing a
section number expands that section and shrinks the terminal to 30%. All four
sections stay visible, and the ones you did not select collapse into a single
header row. Focusing the terminal again restores 55%. The collapsed strip shows
the selected instance and whether it is running.

Destructive actions are never confirmed with `Enter`. Deleting an instance,
snapshot or project, and allowing a flagged command, requires the explicit `y`
key (or `a` for the security prompt). Pressing `Enter` keeps the confirmation
open.

The project delete dialog first shows the branch, the sync state and the number
of changed files. `y` removes the project from the dashboard and keeps its files.
`f` also deletes the directory. An imported project only loses its symlink. Its
original directory is never touched.

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

The terminal is a pane inside the project screen. It opens in **normal** mode.
In normal mode, keys are hegn shortcuts and nothing reaches the guest. Press `i`
or `a` to enter **insert** mode and type commands. `Esc` returns to normal mode,
and then to the section headers. The pane stays open.

| Key | Action |
| --- | --- |
| `i` / `a` / `t` | Open or focus the terminal and enter insert mode |
| `L` | Launch the configured agent or command |
| `Esc` | Insert → normal. Normal → section headers. |
| `←` | Return to the dashboard (the VM keeps running) |
| `M` | Create multi-agent clones for the open project |
| `Enter` | In insert mode: check the command with `tirith-core`, then send it |
| `F2` | Leave the TUI and start the real `hegn attach` client |
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

`s` is the one setup entry point. It creates missing directories and the config,
downloads the pinned images, and applies the guarded network through `sudo` or
`pkexec` when needed. Every step can be repeated safely. Image downloads run in
the background, and a notification in the bottom-right corner shows the progress.
The same work is available from the command line as `hegn doctor --fix`.

Optional tools (lazygit, `$EDITOR`) never make the report fail. hegn does not
install them.

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
