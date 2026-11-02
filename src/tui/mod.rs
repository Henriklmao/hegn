//! Terminal lifecycle and the blocking TUI event loop.
//!
//! [`app`] owns the pure state machine and [`ui`] is the ratatui renderer.
//! This module only wires the pieces together: [`terminal`] handles raw-mode
//! setup and restore, [`event_loop`] drives drawing, input and ticking,
//! [`effects`] executes state-machine effects, [`jobs`] owns the background
//! workers, [`messages`] carries their results and [`launch`] integrates agent
//! launches.

pub mod app;
mod effects;
mod event_loop;
mod jobs;
mod launch;
mod messages;
mod terminal;
pub mod ui;

pub use app::App;

use std::sync::mpsc;

use anyhow::{Context, Result};

use crate::config::Config;

/// Run the interactive TUI until the user quits.
pub fn run(config: Config) -> Result<()> {
    let mut app = App::new(config);
    // First launch / a broken host: surface the doctor immediately instead of
    // waiting for a VM start to fail. The "Set up everything" action is an
    // explicit confirmation inside the modal.
    let report = crate::diagnostics::doctor::run(&app.config);
    if event_loop::should_show_doctor(&report) {
        app.show_doctor(report, None);
    }
    let (scan_tx, scan_rx) = mpsc::channel();
    jobs::spawn_scan(app.config.clone(), scan_tx.clone());

    // Build the runtime before taking over the terminal so a failure cannot
    // leave the user in raw mode on the alternate screen.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build async runtime")?;
    let mut terminal = terminal::setup_terminal()?;

    let result = event_loop::run_loop(&mut terminal, &mut app, &scan_tx, &scan_rx, &runtime);
    terminal::restore_terminal(&mut terminal)?;
    result
}
