//! Agent launch integration for the TUI.
//!
//! Turns a [`LaunchSpec`] into a concrete [`LaunchAction`], then either types the
//! command into the embedded serial console, suspends the TUI for an external
//! SSH session, spawns a detached program, or asks herdr for a new pane.

use std::io::Stdout;
use std::process::Command;

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::agent::launch::{self, LaunchAction, LaunchSpec};
use crate::tui::App;
use crate::tui::app::ToastLevel;

pub(super) fn launch_agent(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    spec: LaunchSpec,
    vm: String,
    project: std::path::PathBuf,
) -> Result<()> {
    let hegn = std::env::current_exe().context("failed to locate the hegn executable")?;
    let action = match launch::action(&spec, &vm, &project, &hegn) {
        Ok(action) => action,
        Err(err) => {
            app.toast(format!("launch failed: {err}"), ToastLevel::Error);
            return Ok(());
        }
    };
    match action {
        LaunchAction::Embedded { command } => match send_embedded_command(app, &command) {
            Ok(()) => {
                app.project_view = crate::tui::app::ProjectView::Split;
                app.terminal_focus = true;
                app.terminal_mode = crate::tui::app::TerminalMode::Normal;
                app.toast(format!("launched {}", spec.name), ToastLevel::Info);
            }
            Err(err) => app.toast(format!("launch failed: {err}"), ToastLevel::Error),
        },
        LaunchAction::External { vm, command, bar } => {
            // `bar` belongs to the serial console and has no SSH equivalent.
            let _ = bar;
            let result =
                super::effects::attach_ssh_session(terminal, &app.config, &vm, command.as_deref());
            // The SSH guard cleared the title; the event loop will not rewrite
            // it because its cached title is unchanged.
            let _ = super::terminal::set_title(terminal, &app.window_title());
            match result {
                Ok(()) => app.toast(
                    format!("returned from SSH session to {vm}"),
                    ToastLevel::Info,
                ),
                Err(err) => app.toast(
                    format!("SSH launch in {vm} failed: {err:#}"),
                    ToastLevel::Error,
                ),
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

/// Type an embedded launch command into the serial console.
///
/// Only external sessions use SSH; the embedded TUI terminal remains the
/// serial relay with its line buffer.
pub(super) fn send_embedded_command(app: &mut App, command: &str) -> Result<()> {
    let session = app
        .session
        .as_mut()
        .context("open the terminal before launching an embedded command")?;
    if !command.trim().is_empty() {
        session.send(command.as_bytes())?;
        session.send(b"\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::console::TerminalConsole;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);

    struct RecordingConsole {
        sent: Sent,
        project: PathBuf,
    }

    impl TerminalConsole for RecordingConsole {
        fn send(&mut self, bytes: &[u8]) -> Result<()> {
            self.sent.0.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }

        fn drain(&mut self) -> Vec<Vec<u8>> {
            Vec::new()
        }

        fn is_exited(&mut self) -> bool {
            false
        }

        fn project(&self) -> &Path {
            &self.project
        }

        fn label(&self) -> &str {
            "fake · serial"
        }

        fn vm_name(&self) -> &str {
            "rust"
        }
    }

    fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            projects_root: dir.path().join("projects"),
            state_dir: dir.path().join("state"),
            vms_dir: dir.path().join("vms"),
            ..crate::config::Config::default()
        };
        std::fs::create_dir_all(&config.projects_root).unwrap();
        std::fs::create_dir_all(&config.vms_dir).unwrap();
        (dir, App::new(config))
    }

    #[test]
    fn embedded_launch_types_into_the_serial_console() {
        let (_dir, mut app) = test_app();
        let sent = Sent::default();
        app.session = Some(Box::new(RecordingConsole {
            sent: sent.clone(),
            project: PathBuf::from("/projects/demo"),
        }));

        send_embedded_command(&mut app, "opencode").unwrap();

        assert_eq!(sent.0.lock().unwrap().as_slice(), b"opencode\n");
        assert!(
            app.session.is_some(),
            "an embedded launch must keep the serial console attached"
        );
    }

    #[test]
    fn embedded_launch_without_a_console_is_a_soft_error() {
        let (_dir, mut app) = test_app();

        let err = send_embedded_command(&mut app, "opencode").unwrap_err();

        assert!(err.to_string().contains("open the terminal"), "{err}");
    }
}
