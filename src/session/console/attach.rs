//! Interactive attach loop and terminal restoration.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::session::attach_bar::{OutputGate, StatusBar, detach_requested};
use crate::session::logs::sanitize_label;

use super::transport::TerminalConsole;
use super::vm_console::VmConsole;

/// The smallest terminal hegn will attach to. The bottom line is reserved for
/// the status bar, and the guest needs at least a few lines above it.
const MIN_COLS: u16 = 32;
const MIN_ROWS: u16 = 4;

/// Attaches the process terminal to the console and restores it on drop,
/// including when the console loop unwinds with a panic.
struct TerminalGuard {
    cols: u16,
    rows: u16,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        use crossterm::cursor::Show;
        use crossterm::execute;
        use crossterm::terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        };

        let (cols, rows) =
            crossterm::terminal::size().context("failed to read the terminal size")?;
        if cols < MIN_COLS || rows < MIN_ROWS {
            bail!(
                "terminal is too small to attach ({cols}x{rows}); need at least {MIN_COLS}x{MIN_ROWS}"
            );
        }

        enable_raw_mode().context("failed to enable raw mode")?;
        let mut out = std::io::stdout();
        if let Err(err) = execute!(out, EnterAlternateScreen) {
            disable_raw_mode().ok();
            return Err(err).context("failed to enter the serial console");
        }
        // Clear the alternate screen for both bar modes and start from a known
        // margin/origin state.
        if let Err(err) = write!(out, "\x1b[2J\x1b[?6l\x1b[r\x1b[H").and_then(|()| out.flush()) {
            disable_raw_mode().ok();
            let _ = execute!(out, LeaveAlternateScreen, Show);
            return Err(err).context("failed to initialize the serial console");
        }
        Ok(Self { cols, rows })
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    fn update_size(&mut self, cols: u16, rows: u16) -> Result<()> {
        if cols < MIN_COLS || rows < MIN_ROWS {
            bail!(
                "terminal is too small to attach ({cols}x{rows}); need at least {MIN_COLS}x{MIN_ROWS}"
            );
        }
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use crossterm::cursor::Show;
        use crossterm::execute;
        use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

        disable_raw_mode().ok();
        let mut out = std::io::stdout();
        // Reset margins and origin mode on both screens; a stale region or
        // origin mode would break the user's shell after hegn exits.
        let _ = write!(out, "\x1b[?6l\x1b[r\x1b[0m");
        let _ = out.flush();
        let _ = execute!(out, LeaveAlternateScreen, Show);
        let _ = write!(out, "\x1b[?6l\x1b[r\x1b[0m");
        let _ = out.flush();
    }
}

/// Attach an interactive terminal to a VM serial console.
///
/// This is the CLI equivalent of the TUI terminal view. Input is passed
/// straight to the guest UART (the status bar marks this as `INPUT DIRECT`); use
/// `hegn check` for pre-execution guardrails or the TUI for line-buffered
/// interception. `Ctrl-B` (or `Ctrl-]`) detaches without stopping the VM.
///
/// When `bar` is set, the bottom line shows the project, harness, VM pid and
/// persistent serial log while the guest uses the lines above it. The guest has
/// no window-size channel, so full-screen guest programs keep their own
/// geometry; pass `bar = false` to give them the whole host terminal.
pub fn run_interactive(console: VmConsole, bar: bool) -> Result<()> {
    run_interactive_with_command(console, bar, None)
}

/// Like [`run_interactive`], but types `command` into the guest after the
/// console is live. Used by the launch integrations so `tmux`/`herdr` panes can
/// open the agent instead of a bare shell.
pub fn run_interactive_with_command(
    mut console: VmConsole,
    bar: bool,
    initial_command: Option<&str>,
) -> Result<()> {
    use crossterm::event::{self, Event, KeyEventKind};

    let mut guard = TerminalGuard::enter()?;
    let project = console.project_name();
    let harness = console.harness().to_string();
    let pid = console.pid();
    let log = console.serial_log().map(Path::to_path_buf);
    let instance = console.instance_name().to_string();
    let (cols, rows) = guard.size();
    let mut status = bar.then(|| {
        StatusBar::new(
            project.clone(),
            harness.clone(),
            pid,
            log.clone(),
            cols,
            rows,
        )
    });
    let mut gate = OutputGate::new();
    let mut pending_resize: Option<(u16, u16)> = None;
    let mut vm_exited = false;

    let result = (|| -> Result<()> {
        let mut out = std::io::stdout();
        if let Some(status) = &status {
            status.reserve_region(&mut out)?;
            status.paint(&mut out)?;
        }
        if let Some(command) = initial_command {
            let command = command.trim_end_matches(['\r', '\n']);
            if !command.is_empty() {
                console.send(command.as_bytes())?;
                console.send(b"\n")?;
            }
        }

        loop {
            let chunks = console.drain();
            let mut dirty = !chunks.is_empty();
            for chunk in chunks {
                gate.feed(&chunk, status.as_ref().map(StatusBar::rows), &mut out)?;
            }
            out.flush()?;
            if console.is_exited() {
                vm_exited = true;
                break;
            }

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if detach_requested(key) {
                            break;
                        }
                        if let Some(bytes) = crate::tui::app::encode_key(key) {
                            console.send(&bytes)?;
                        }
                    }
                    Event::Resize(cols, rows) => {
                        guard.update_size(cols, rows)?;
                        if let Some(status) = status.as_mut() {
                            if gate.safe_to_inject() {
                                status.resize(cols, rows, &mut out)?;
                                dirty = true;
                            } else {
                                // Do not split a half-received escape sequence.
                                // Apply the resize after the next clean batch.
                                pending_resize = Some((cols, rows));
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Repaint after every output burst. A full-screen guest can move
            // the cursor onto the reserved row, and a reset can drop the
            // scroll region; only an immediate repaint keeps the bar pinned to
            // the bottom. Skip it while a split escape sequence or multi-byte
            // character is in flight: injecting there would corrupt the guest
            // stream. The next batch that ends cleanly repaints.
            if let Some(status) = status.as_mut()
                && gate.safe_to_inject()
            {
                if let Some((cols, rows)) = pending_resize.take() {
                    status.resize(cols, rows, &mut out)?;
                    dirty = true;
                }
                if dirty {
                    status.paint(&mut out)?;
                }
            }
        }
        Ok(())
    })();

    // Terminate a partial escape sequence even when the loop unwound with an
    // error, so the terminal reset on drop is not swallowed by an open string.
    let _ = gate.finish(&mut std::io::stdout());
    drop(guard);
    result?;
    let project = sanitize_label(&project);
    let instance = sanitize_label(&instance);
    if vm_exited {
        println!("Serial console closed: the VM for {project} exited.");
    } else {
        println!(
            "Detached from {project}. The VM keeps running; re-attach with `hegn attach {instance}`."
        );
    }
    if let Some(log) = &log {
        println!("Serial log: {}", sanitize_label(&log.display().to_string()));
    }
    Ok(())
}
