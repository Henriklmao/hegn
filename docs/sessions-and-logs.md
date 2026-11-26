# Sessions, attach and logs

A **session** is your connection to a running VM. `hegn attach` opens SSH by
default, so every session gets its own guest PTY and several can run in parallel.
The serial console remains the boot, provisioning and recovery channel. Starting
or attaching any member starts or attaches the whole VM.

- [Session branches](#session-branches)
- [Attaching over SSH](#attaching-over-ssh)
- [The serial console and attach bar](#the-serial-console-and-attach-bar)
- [Session logs](#session-logs)
- [Start diagnostics](#start-diagnostics)

---

## Session branches

With `auto_branch = true` (the default), `hegn open <project>` checks out a branch
named `agent/hegn-session-<timestamp>` in that project on the host once the VM
has booted. Each opened project therefore gets its own branch, and the agent's
commits end up there instead of on your main branch. With `auto_branch = false`,
`hegn open` creates no branch automatically. `hegn open --no-branch` forces the
branch off for one run.

The setting lives in `config.toml` (see [Configuration](configuration.md)).
`hegn attach` never creates a branch, because it does not open a project.

---

## Attaching over SSH

```bash
hegn attach          # SSH to the only running VM
hegn a dev           # SSH to VM dev
hegn attach parser   # SSH to the VM that hosts project parser
hegn attach dev --command 'cargo test'
                     # one-off SSH exec in the project's guest directory
hegn attach dev --serial
                     # serial console (boot, provisioning, recovery)
```

`hegn attach` never boots or restarts a VM. It fails when no VM is running. A
selector may be a VM name or a project name; `hegn open <project>` boots the
project's VM if needed, waits for provisioning and SSH readiness, and also
prints the project's guest path.

If several VMs run at the same time, pass the VM name (or a project that names
exactly one VM). Without an argument, `attach` only connects when exactly one VM
is running.

Each SSH invocation is a separate guest PTY, so you can open several terminals,
tmux panes or herdr panes for the same VM. `exit` or `Ctrl-D` ends only that
session; the VM keeps running. Full-screen programs resize with the host
terminal.

`--command` is executed through SSH exec in the project's guest directory, and
its exit code is propagated. A transport or authentication failure is reported
as a hegn error with a hint; hegn never falls back to the serial console.

If the registry changed while the VM was running, attach prints a warning. The
running VM keeps the shares it booted with. Run `hegn vm apply <vm>` to restart
with the new membership; that kills running agents and SSH sessions.

---

## The serial console and attach bar

`hegn attach <vm> --serial` opens the single serial console. The bottom line of
the terminal is reserved for a status bar. It shows the project (or the VM's
primary project), the harness, the VM process ID, the log file and the input
mode:

```
 hegn ▸ my-app · OpenCode · pid 4242 · log session-…-my-app-4242.log   INPUT DIRECT · Ctrl-B/Ctrl-] detach
```

The bar is repainted after every burst of guest output. It stays in place even
when a full-screen program moves the cursor or resets the scroll region.

Input goes straight to the guest's serial port. The `INPUT DIRECT` label shows
that this path skips the `tirith-core` check. SSH sessions bypass the check too,
but have no bar; they are labelled by the host terminal title
(`hegn: <vm> · <project>`). For checked input, use the project terminal in the
TUI. For scripts, use `hegn check`.

The serial console cannot change size over the legacy serial port. So the bar
takes one row of the host terminal, and full-screen guest programs keep their own
size. Use `--no-bar` (also accepted by `hegn open --serial`) when a guest TUI
needs every row. `--no-bar` is serial-only; SSH sessions get a real PTY and
resize normally.

Detach from the serial console with `Ctrl-B` or `Ctrl-]`. Inside tmux, `Ctrl-B`
is tmux's own prefix key, so use `Ctrl-]` there. Both keys are shown in the bar.
Detaching leaves the VM running.

---

## Session logs

Every VM boot writes its serial output to:

```text
<state_dir>/logs/session-<timestamp>-<project>-<pid>.log
```

The log is created with private permissions. It covers the serial console (boot,
cloud-init, provisioning and `--serial` attach). SSH session I/O is not copied
into it; there is no per-session transcript. Stopping the VM does not delete the
log; only `hegn logs prune` removes old logs.

```bash
hegn logs                       # list, newest first
hegn logs list --vm dev         # only logs from dev's member projects
hegn logs path my-app           # absolute path (newest by default)
hegn logs show --lines 120      # tail with ANSI/control sequences stripped
hegn logs show --raw            # exact bytes; only for trusted logs
hegn logs prune --keep 20       # only hegn's session-*.log files
```

`--vm` is accepted by `logs list`, `logs path` and `logs show`. The old
`--instance` spelling is accepted as a hidden alias.

Logs can contain secrets that an agent printed. So:

- files are `0600` in a `0700` directory,
- symlinks are rejected,
- `show` removes terminal control sequences and bidi-override characters unless
  you ask for `--raw`.

A log is not truncated while the VM is running. Retention is based on the count
(default 50). Run `hegn logs prune` whenever you like. Pruning never touches the
log of the currently running session.

Logs from v1 are ordinary files and remain readable. They are diagnostics, not
state; hegn does not migrate or delete them.

---

## Start diagnostics

If a start fails, the details are in the VM's run directory:

```bash
# <vms_dir> defaults to ~/.local/share/hegn/vms
cat ~/.local/share/hegn/vms/<vm>/run/hegn.log             # lifecycle and start errors
cat ~/.local/share/hegn/vms/<vm>/run/cloud-hypervisor.log # VMM log
cat ~/.local/share/hegn/vms/<vm>/run/console-relay.log    # relay stderr
cat ~/.local/share/hegn/vms/<vm>/run/autosave.log         # automatic checkpoint errors
hegn logs list                  # serial logs, including failed starts
hegn logs show <name>           # VMM API and guest output
```

The error message in the CLI includes the path of `hegn.log` and the last lines of
the VMM or relay log. A boot failure is therefore not reported as a generic relay
error. If one member's `virtiofsd` fails, every daemon started for that VM is
killed and the serial log is kept, so the Cloud Hypervisor messages that caused
the failure stay available. See also [Troubleshooting](troubleshooting.md).
