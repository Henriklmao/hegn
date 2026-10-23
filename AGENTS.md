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

- `src/lib.rs` — module exports; the crate is a library with a thin `main.rs`.
- `src/cli.rs` — every TUI action needs a non-interactive CLI counterpart.
- `src/config.rs` — config schema; unknown keys are rejected, so update the
  struct *and* the README example when adding options.
- `src/security.rs` — `tirith-core` integration plus hegn-specific guardrails.
- `src/cloud_hypervisor.rs` / `src/sandbox.rs` / `src/virtiofs.rs` — VM lifecycle.
- `src/relay.rs` / `src/console.rs` — detached serial relay and attach transport.
- `src/cloud_init.rs` — NoCloud seed for the official cloud images.
- `src/snapshot.rs` / `src/state.rs` — persisted metadata under XDG dirs.
- `src/images.rs` / `src/doctor.rs` — pinned upstream VM assets and shared
  prerequisite checks.
- `docs/architecture.md` — detailed module, lifecycle, safety and CI documentation.
- `src/app.rs` / `src/ui.rs` / `src/tui.rs` — state machine, rendering, terminal
  event loop.

`README.md` is the user-facing documentation. Keep it in sync when behavior
changes.

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
