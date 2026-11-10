# Sessions, attach and logs

A **session** is your connection to a running VM. Attaching and detaching never
stop the VM. Every session also writes a log that you can read later.

- [Session branches](#session-branches)
- [Attaching like tmux](#attaching-like-tmux)
- [The attach bar](#the-attach-bar)
- [Session logs](#session-logs)
- [Start diagnostics](#start-diagnostics)

---

## Session branches

With `auto_branch = true` (the default), hegn checks out a branch named
`agent/hegn-session-<timestamp>` on the host once the VM has booted. Each session
therefore has its own branch, and the agent's commits end up there instead of on
your main branch. `hegn open --no-branch` skips this step.

The setting lives in `config.toml` (see [Configuration](configuration.md)).

---

## Attaching like tmux

```bash
hegn attach          # attach to the single running VM
hegn a my-app        # attach, but only if the running VM belongs to my-app
hegn attach --no-bar # give full-screen guest programs the whole terminal
```

`hegn attach` never boots or restarts a VM. It fails when no instance is running.

If several VMs run at the same time, pass the instance name (or a project name
that identifies exactly one base instance). Without an argument, `attach` only
connects when exactly one instance is running.

Detach with `Ctrl-B`, or with `Ctrl-]` when hegn runs inside tmux. In tmux,
`Ctrl-B` is tmux's own prefix key, so hegn's detach key is `Ctrl-]` there. Both
keys are shown in the bar.

Detaching leaves the VM running.

---

## The attach bar

The bottom line of the terminal is reserved for a status bar. It shows the
project, the harness, the VM process ID, the log file and the input mode:

```
 hegn ▸ my-app · OpenCode · pid 4242 · log session-…-my-app-4242.log   INPUT DIRECT · Ctrl-B/Ctrl-] detach
```

The bar is repainted after every burst of guest output. It stays in place even
when a full-screen program moves the cursor or resets the scroll region.

Input goes straight to the guest's serial port. The `INPUT DIRECT` label shows
that this path skips the `tirith-core` check. For checked input, use the project
terminal in the TUI. For scripts, use `hegn check`.

The serial console cannot change size over the legacy serial port. So the bar
takes one row of the host terminal, and full-screen guest programs keep their own
size. Use `--no-bar` (also accepted by `hegn open`) when a guest TUI needs every
row.

---

## Session logs

Every VM session writes its serial output to:

```text
~/.local/state/hegn/logs/session-<timestamp>-<project>-<pid>.log
```

The log is created with private permissions and is kept for the whole VM
lifetime. Stopping the VM does not erase the boot log.

```bash
hegn logs                       # list, newest first
hegn logs path my-app           # absolute path (newest by default)
hegn logs show --lines 120      # tail with ANSI/control sequences stripped
hegn logs show --raw            # exact bytes; only for trusted logs
hegn logs prune --keep 20       # only hegn's session-*.log files
```

Logs can contain secrets that an agent printed. So:

- files are `0600` in a `0700` directory,
- symlinks are rejected,
- `show` removes terminal control sequences and bidi-override characters unless
  you ask for `--raw`.

A log is not truncated while the VM is running. Retention is based on the count
(default 50). Run `hegn logs prune` whenever you like. Pruning never touches the
log of the currently running session.

---

## Start diagnostics

If a start fails, the details are in the instance's run directory:

```bash
# <instances_dir> defaults to ~/.local/share/hegn/instances
cat ~/.local/share/hegn/instances/<name>/run/hegn.log             # lifecycle and start errors
cat ~/.local/share/hegn/instances/<name>/run/cloud-hypervisor.log # VMM log
cat ~/.local/share/hegn/instances/<name>/run/console-relay.log    # relay stderr
hegn logs list                  # serial logs, including failed starts
hegn logs show <name>           # VMM API and guest output
```

The error message in the CLI includes the path of `hegn.log` and the last lines of
the VMM or relay log. A boot failure is therefore not reported as a generic relay
error. Serial logs of failed starts are kept, so the Cloud Hypervisor messages
that caused the failure stay available. See also [Troubleshooting](troubleshooting.md).
