//! Agent launch integration for the TUI.
//!
//! Turns a [`LaunchSpec`] into a concrete [`LaunchAction`], then either types the
//! command into the embedded console, suspends the TUI for an external or tmux
//! client, spawns a detached program, or asks herdr for a new pane.

use std::io::Stdout;
use std::process::Command;

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::agent::launch::{self, LaunchAction, LaunchSpec};
use crate::tui::App;
use crate::tui::app::ToastLevel;
use crate::tui::terminal::with_suspended_terminal;

pub(super) fn launch_agent(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    spec: LaunchSpec,
    instance: String,
    project: std::path::PathBuf,
) -> Result<()> {
    let hegn = std::env::current_exe().context("failed to locate the hegn executable")?;
    let action = match launch::action(&spec, &instance, &project, &hegn) {
        Ok(action) => action,
        Err(err) => {
            app.toast(format!("launch failed: {err}"), ToastLevel::Error);
            return Ok(());
        }
    };
    match action {
        LaunchAction::Embedded { command } => {
            let result: Result<()> = (|| {
                let session = app
                    .session
                    .as_mut()
                    .context("open the terminal before launching an embedded command")?;
                if !command.trim().is_empty() {
                    session.send(command.as_bytes())?;
                    session.send(b"\n")?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    app.project_view = crate::tui::app::ProjectView::Split;
                    app.terminal_focus = true;
                    app.terminal_mode = crate::tui::app::TerminalMode::Normal;
                    app.toast(format!("launched {}", spec.name), ToastLevel::Info);
                }
                Err(err) => app.toast(format!("launch failed: {err}"), ToastLevel::Error),
            }
        }
        LaunchAction::External {
            instance,
            command,
            bar,
        } => {
            let registry = crate::vm::instance::InstanceRegistry::new(app.config.clone());
            app.detach_console();
            let result = with_suspended_terminal(terminal, || {
                let config = registry.resolve(&instance)?;
                let console = registry.manager(config).attach_console()?;
                crate::session::console::run_interactive_with_command(
                    console,
                    bar,
                    command.as_deref(),
                )
            });
            // The external client closed on detach; restore the embedded view so
            // the project screen is not left without a console.
            let reattached = registry
                .resolve(&instance)
                .ok()
                .and_then(|config| registry.manager(config).attach_console().ok());
            if let Some(console) = reattached {
                app.attach_console(Box::new(console));
                app.project_view = crate::tui::app::ProjectView::Split;
                app.terminal_focus = true;
            }
            if let Err(err) = result {
                app.toast(format!("launch failed: {err}"), ToastLevel::Error);
            }
        }
        LaunchAction::Spawn { program, args } => {
            app.detach_console();
            match Command::new(&program).args(&args).output() {
                Ok(output) if output.status.success() => {
                    app.toast(
                        format!("launched {} via {program}", spec.name),
                        ToastLevel::Info,
                    );
                }
                Ok(output) => app.toast(
                    format!(
                        "{program} launch failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                    ToastLevel::Error,
                ),
                Err(err) => app.toast(format!("{program} launch failed: {err}"), ToastLevel::Error),
            }
        }
        LaunchAction::Herdr { pane_command, cwd } => {
            app.detach_console();
            match launch::spawn_herdr(&pane_command, &cwd) {
                Ok(()) => app.toast(format!("launched {} in herdr", spec.name), ToastLevel::Info),
                Err(err) => app.toast(format!("herdr launch failed: {err:#}"), ToastLevel::Error),
            }
        }
    }
    Ok(())
}
