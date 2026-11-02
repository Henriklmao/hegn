//! The blocking TUI event loop and the drain of background worker messages.
//!
//! Each iteration draws a frame, handles at most one terminal event, ticks the
//! state machine and applies any resulting [`Effect`]s through
//! [`super::effects`]. Worker messages are drained once per iteration so scans,
//! downloads and VM starts surface without blocking the UI.
//!
//! The file is named `event_loop.rs` because `loop` is a Rust keyword.

use std::io::Stdout;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::diagnostics::doctor::DoctorReport;
use crate::tui::App;
use crate::tui::app::{Effect, ToastLevel};
use crate::tui::effects::handle_effects;
use crate::tui::messages::ScanMessage;
use crate::tui::ui;

const TICK: Duration = Duration::from_millis(50);

pub(super) fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    scan_tx: &mpsc::Sender<ScanMessage>,
    scan_rx: &Receiver<ScanMessage>,
    runtime: &tokio::runtime::Runtime,
) -> Result<()> {
    let mut last_tick = Instant::now();
    loop {
        let size = terminal.size()?;
        app.set_viewport(size.width, size.height);
        let mut hits = ui::UiHits::default();
        terminal.draw(|frame| {
            hits = ui::draw(frame, app);
        })?;
        app.footer_hits = hits.footer;
        app.project_rows = hits.projects;
        app.instance_rows = hits.instances;
        app.snapshot_rows = hits.snapshots;
        app.section_hits = hits.sections;
        app.terminal_hit = hits.terminal;
        app.terminal_strip_hit = hits.terminal_strip;
        app.config_rows = hits.config;
        app.config_steppers = hits.config_steppers;
        app.config_tabs = hits.config_tabs;
        app.modal_scroll = hits.modal_scroll;

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let effects = app.handle_key(key);
                    handle_effects(terminal, app, effects, scan_tx, runtime)?;
                }
                Event::Mouse(mouse) => {
                    let effects = app.handle_mouse(mouse);
                    handle_effects(terminal, app, effects, scan_tx, runtime)?;
                }
                Event::Resize(cols, rows) => app.set_viewport(cols, rows),
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            app.poll();
        }

        drain_scan(app, scan_rx);
        if let Some((spec, instance, project)) = app.take_due_launch() {
            handle_effects(
                terminal,
                app,
                vec![Effect::LaunchAgent {
                    spec,
                    instance,
                    project,
                }],
                scan_tx,
                runtime,
            )?;
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn drain_scan(app: &mut App, scan_rx: &Receiver<ScanMessage>) {
    loop {
        match scan_rx.try_recv() {
            Ok(ScanMessage::Projects(projects)) => app.set_projects(projects),
            Ok(ScanMessage::Started {
                console,
                branch,
                harness,
                started,
            }) => {
                let instance = console.instance_name().to_string();
                let project = console.project().to_path_buf();
                app.attach_console(console);
                app.queue_auto_launch(&instance, harness, &project, started);
                match branch {
                    Some(Ok(branch)) => {
                        app.toast(format!("session branch {branch}"), ToastLevel::Info)
                    }
                    Some(Err(err)) => {
                        app.toast(format!("branch not created: {err}"), ToastLevel::Warn)
                    }
                    None => {}
                }
            }
            Ok(ScanMessage::StartFailed(err)) => app.fail_start(err),
            Ok(ScanMessage::InstancesChanged { note, level }) => {
                app.refresh_instances();
                app.toast(note, level);
            }
            Ok(ScanMessage::InstanceError(err)) => {
                app.refresh_instances();
                app.toast(err, ToastLevel::Error);
            }
            Ok(ScanMessage::Doctor { report, note }) => {
                app.clear_download_progress();
                app.show_doctor(report, note);
            }
            Ok(ScanMessage::DownloadProgress(progress)) => app.set_download_progress(progress),
            Ok(ScanMessage::Provision(progress)) => {
                app.set_provision_progress(progress);
                app.provision_started();
            }
            Ok(ScanMessage::ProvisionReady) => app.provision_ready(),
            Ok(ScanMessage::ProvisionFailed(err)) => app.provision_failed(err),
            Ok(ScanMessage::Error(err)) => {
                app.loading = false;
                app.toast(format!("project scan failed: {err}"), ToastLevel::Error);
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
}

/// Open the doctor on first launch when anything is broken or hegn can prepare
/// the host without further input.
pub(super) fn should_show_doctor(report: &DoctorReport) -> bool {
    !report.ok || report.can_setup()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_launch_opens_the_doctor_when_setup_is_available() {
        let clean = DoctorReport {
            ok: true,
            checks: Vec::new(),
            assets: Vec::new(),
            setup: crate::diagnostics::doctor::SetupActions::default(),
        };
        assert!(!should_show_doctor(&clean));

        let setup = DoctorReport {
            setup: crate::diagnostics::doctor::SetupActions {
                init_config: true,
                ..crate::diagnostics::doctor::SetupActions::default()
            },
            ..clean.clone()
        };
        assert!(should_show_doctor(&setup));

        let broken = DoctorReport { ok: false, ..clean };
        assert!(should_show_doctor(&broken));
    }
}
