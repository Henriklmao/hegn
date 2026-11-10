# Harnesses, profiles and launch commands

A **harness** is the coding agent that runs inside a VM, such as OpenCode or
Claude Code. This page covers three things:

- which harnesses exist and what they install,
- how **profiles** give a harness your API keys, config files and skills,
- how **launch commands** start the agent in a terminal.

- [Harness presets](#harness-presets)
- [Harness profiles](#harness-profiles)
- [Skill sync](#skill-sync)
- [Launch commands and terminal integrations](#launch-commands-and-terminal-integrations)

---

## Harness presets

The preset list and the first-boot process are described in
[Getting started](getting-started.md#harness-presets). In short: each preset
installs its runtime and CLI in the guest on first boot. Provisioning runs on
every boot until it succeeds, and it needs the guest to have working DNS and
the guarded network.

Node presets upgrade Jammy's old package to Node.js 22 from NodeSource inside the
guest.

---

## Harness profiles

A profile is a TOML file per harness. It describes what the VM should receive
on every start:

- extra apt packages,
- config files and API keys,
- environment variables,
- skill directories.

`hegn config init` writes one template per harness to
`~/.config/hegn/profiles/` (`opencode.toml`, `prime-agent.toml`,
`claude-code.toml`, and so on). The files are private (`0600`) because they may
contain API keys. Existing profiles are never overwritten.

```toml
# ~/.config/hegn/profiles/opencode.toml
version = 1
packages = ["tmux", "gh"]              # extra apt packages for this instance

[environment]
OPENAI_API_KEY = "sk-..."              # exported to login shells and the agent

[[files]]                              # copied on every start
source = "~/.config/opencode/opencode.json"
target = "/root/.config/opencode/opencode.json"
mode = "0600"

[[files]]
source = "~/.config/opencode/auth.json"
target = "/root/.config/opencode/auth.json"

[[skills]]                             # copied, never linked
source = "~/.config/opencode/skills"
target = "/root/.config/opencode/skills"
sync_back = true

[[skills]]
source = "~/.agents/skills"
target = "/root/.agents/skills"
```

Both `source` paths and guest `target` paths are validated. Guest targets must be
under `/root`, `/home`, `/etc`, `/usr/local`, `/opt` or `/workspace`.

A missing source is reported in the instance's `run/hegn.log`. It does not block
the start. A default skill directory that does not exist is skipped silently.

### How profiles reach the guest

On start, hegn copies the profile into `<instances_dir>/<name>/profile/bundle/`.
A second, sandboxed `virtiofsd` exports that **copy** to the guest at
`/run/hegn/profile`.

- Config files and environment variables are applied on every boot.
- Skills are seeded on the first start. After that they live in the per-instance
  copy.
- Added packages are installed on the next boot. The harness installer does not
  run again.

The guest's skill directory links to that copy, **not** to the host directory. A
compromised guest can therefore change only the throw-away copy, never your real
skills.

```bash
hegn profile list                       # template paths and state
hegn profile show opencode              # effective profile as TOML
hegn profile path opencode --edit       # edit the template in $EDITOR
hegn profile init                       # write only missing templates

hegn profile sync my-app                # refresh files/env/skills for next boot
```

---

## Skill sync

Skills are copied in both directions, on purpose:

```bash
hegn skills sync my-app                 # merge host skill changes into the copy
hegn skills sync-back my-app            # copy guest changes back to the host
hegn skills sync-back my-app --dry-run  # show what would change
```

`sync-back` is copy-only:

- It copies regular files only. Symlinks and special files are skipped.
- It refuses to run against a live VM unless you pass `--force`.
- Deleted host files are never removed by a sync-back.

---

## Launch commands and terminal integrations

The embedded project terminal is the **maintenance shell**. It is a scrollback
pane, not a full terminal emulator. So when you start a VM, hegn also starts the
configured agent in a real terminal. Press `L` to launch it again later.

### What gets launched

`launch.default` in `config.toml` decides what starts:

- `"harness"` starts the agent of the instance's harness (`opencode`, `claude`,
  `codex`, `pi`, `gemini`).
- `"shell"` only opens the maintenance shell.
- Any other value refers to an entry in `[[launch.commands]]`.

Commands are typed into the guest shell. They never run on the host.

Launch commands are trusted, user-owned configuration. They are sent straight to
the guest shell, like input from `hegn attach`. The line-based `tirith-core`
check applies only to commands typed in insert mode. Use `hegn check` first for
anything you do not trust.

### Where the console is shown

The `integration` setting decides where the serial console appears:

| Value | Behaviour |
| --- | --- |
| `auto` | Use `herdr` when running inside herdr, then `tmux` when inside tmux, otherwise the current terminal. |
| `embedded` | Keep the command in the TUI's embedded terminal. |
| `external` | Leave the TUI and attach the current terminal through `hegn attach`. |
| `tmux` | Open a new tmux window that runs `hegn attach`. Requires hegn to run inside tmux. |
| `herdr` | Split a herdr pane and run `hegn attach` in it. Requires `HERDR_ENV=1`. |

The integration only chooses where the console is shown. The command always runs
inside the VM.

### Configuration

```toml
# config.toml
[launch]
default         = "harness"  # "harness", "shell", or a name from commands
integration     = "auto"     # auto | embedded | external | tmux | herdr
auto_start      = true

[[launch.commands]]
name            = "review"
command         = "opencode"
integration     = "tmux"
bar             = false      # full-screen commands should own every row
```

Per-project overrides go into the project's `hegn.toml`:

```toml
launch = "review"
integration = "herdr"
auto_start = true
```

`F6` in the project screen edits the same three values (plus the harness used for
new instances). Use `←`/`→` to cycle a value and `↑`/`↓` to select a row. `Enter`
saves, and `r` saves and launches the command right away. Choose `tmux` or `herdr`
here to open an agent in a terminal multiplexer. `auto` keeps the default that
depends on the host.

TUI auto-launch waits for the first-boot provisioner. The agent is installed
before the command is typed.

### Non-interactive launch

```bash
hegn launch my-app                         # configured default
hegn launch my-app --command review        # named command
hegn launch my-app --integration tmux      # override the integration
hegn open my-app --command opencode        # attach and type one command
hegn attach my-app --command opencode --no-bar
```

`auto_start = false` in a project's `hegn.toml` keeps that project's VM boots
quiet, even when the global `launch.auto_start` is on.
