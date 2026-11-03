//! Raw-mode terminal setup, restore and temporary suspension.
//!
//! Every path that leaves the TUI calls [`restore_terminal`]. Handing the
//! terminal to an external program goes through [`with_suspended_terminal`],
//! which restores before running the action and always re-enters the alternate
//! screen afterwards, returning the action's result to the caller.

use std::io::{Stdout, stdout};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub(super) fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut out = stdout();
    if let Err(err) = execute!(out, EnterAlternateScreen, EnableMouseCapture) {
        disable_raw_mode().ok();
        return Err(err).context("failed to enter alternate screen");
    }
    match Terminal::new(CrosstermBackend::new(out)) {
        Ok(terminal) => Ok(terminal),
        Err(err) => {
            disable_raw_mode().ok();
            let mut out = stdout();
            let _ = execute!(out, LeaveAlternateScreen, DisableMouseCapture, Show);
            Err(err).context("failed to create terminal")
        }
    }
}

pub(super) fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        Show
    )
    .ok();
    terminal.show_cursor().ok();
    Ok(())
}

pub(super) fn with_suspended_terminal<T>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    restore_terminal(terminal)?;
    let result = action();
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.clear().ok();
    result
}
