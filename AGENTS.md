# AGENTS.md

Working agreement for agents and contributors in this repository.

## Required workflow

1. **Always run `cargo fmt`** after making changes, before committing. Do not
   hand-format Rust code.
2. **Always run the open-code-review delegate check after every commit.** Use
   the `open-code-review-delegate` skill (invoked as
   `@open-code-review-delegate`) to review the commit you just created.

### After each commit

```bash
# 1. Format (again, in case anything was generated)
cargo fmt

# 2. Commit
git add -A
git commit -m "<type>(<scope>): <summary>"

# 3. Review the commit with the delegate skill
ocr delegate preview --format json --commit HEAD
ocr delegate rule --format json <changed paths...>
git --no-pager show --no-ext-diff --no-textconv --no-color HEAD -- "<path>"
```

Follow the skill's workflow for diff selection, rule lookup, per-file review and
severity-grouped reporting. Fix critical/high findings before moving on; address
or consciously dismiss medium findings. Do not commit review fixes on top
without re-running the review for that commit.

If `ocr` is unavailable, ask before proceeding rather than skipping the review
step.

## Before pushing

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI pins Rust `1.99.1` and treats clippy warnings as errors, so a clean local run
is the minimum bar.

## Project layout

- `src/lib.rs` — module tree and domain re-exports; the crate is a library with
  a thin `main.rs`.
- `src/shared/` — dependency-free fs, process, path, text and numeric helpers.
- `src/cli/` — every TUI action needs a non-interactive CLI counterpart.
- `src/config/` — config schema; unknown keys are rejected, so update the
  struct *and* `docs/configuration.md` when adding options. `form/` is the TUI
  settings editor.
- `src/security/` — `tirith-core` integration plus hegn-specific guardrails.
- `src/network/` — guarded TAP pool, nftables rules and privileged setup.
- `src/vm/hypervisor/` / `src/vm/instance/` / `src/vm/virtiofs/` — VM lifecycle.
- `src/session/relay/` / `src/session/console/` — detached serial relay and
  attach transport.
- `src/vm/cloud_init/` — NoCloud seed for the official cloud images.
- `src/vm/instance_config/` — per-instance `hegn.toml`, cloning and rootfs copies.
- `src/session/snapshot/` / `src/session/state/` — persisted metadata under XDG
  dirs.
- `src/vm/images/` / `src/diagnostics/doctor/` — pinned upstream VM assets and
  shared prerequisite checks.
- `src/agent/launch/` — named agent commands and embedded/external/tmux/herdr
  launch integrations.
- `src/agent/profile/` — per-harness profiles, skill sync and guest payloads.
- `src/agent/provision/` — first-boot harness preset installers.
- `src/vm/rootfs/` — sparse per-instance disk growth (raw and qcow2).
- `src/diagnostics/benchmark/` — `hegn benchmark`: measured host cost and disk
  usage.
- `src/session/logs/` / `src/session/terminal/` / `src/session/attach_bar/` —
  persistent serial logs, scrollback and the attach status bar.
- `src/tui/app/` / `src/tui/ui/` / `src/tui/` — state machine, rendering,
  terminal event loop and background jobs.
- `docs/architecture.md` — detailed module, lifecycle, safety and CI documentation.

`README.md` is a short overview. The user documentation lives in `docs/`
(index: `docs/README.md`). Keep both in sync when behavior changes.

## Conventions

- Keep modules small and directly unit-testable; add tests next to the code in
  `#[cfg(test)] mod tests`.
- Use `anyhow::Context` for user-facing errors and `thiserror` for typed
  library errors.
- Never weaken sandboxing (virtiofs `--sandbox`, private file modes, canonicalized
  paths) for convenience.
- A missing VM is always a hard error. Never fall back to a host process or
  local PTY. No work happens outside a Cloud Hypervisor VM.
- Keep Cloud Hypervisor's seccomp and Landlock sandboxes enabled; do not weaken
  them for convenience.
- Use `git --no-pager` in every Git invocation during reviews.
- Write commit messages as `type(scope): summary` (e.g.
  `fix(vm): preflight restore binaries`).
