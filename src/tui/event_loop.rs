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
use crossterm::execute;
use crossterm::terminal::SetTitle;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::diagnostics::doctor::DoctorReport;
use crate::tui::App;
use crate::tui::app::{Effect, ToastLevel};
use crate::tui::effects::handle_effects;
use crate::tui::jobs::spawn_apply_vm;
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
    let mut last_title = String::new();
    loop {
        let title = app.window_title();
        if title != last_title {
            execute!(terminal.backend_mut(), SetTitle(title.clone()))?;
            last_title = title;
        }
        let size = terminal.size()?;
        app.set_viewport(size.width, size.height);
        let mut hits = ui::UiHits::default();
        terminal.draw(|frame| {
            hits = ui::draw(frame, app);
        })?;
        app.footer_hits = hits.footer;
        app.project_rows = hits.projects;
        app.vm_rows = hits.vms;
        app.vm_member_rows = hits.vm_members;
        app.snapshot_rows = hits.snapshots;
        app.section_hits = hits.sections;
        app.vm_section_hits = hits.vm_sections;
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

        for vm in drain_scan(app, scan_rx) {
            app.toast(
                format!("applying VM {vm} for its first project…"),
                ToastLevel::Info,
            );
            spawn_apply_vm(app.config.clone(), vm, scan_tx.clone());
        }
        if let Some((spec, vm, project)) = app.take_due_launch() {
            handle_effects(
                terminal,
                app,
                vec![Effect::LaunchAgent { spec, vm, project }],
                scan_tx,
                runtime,
            )?;
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn drain_scan(app: &mut App, scan_rx: &Receiver<ScanMessage>) -> Vec<String> {
    let mut auto_apply = Vec::new();
    loop {
        match scan_rx.try_recv() {
            Ok(ScanMessage::Projects(projects)) => app.set_projects(projects),
            Ok(ScanMessage::Started {
                console,
                branch,
                harness,
                started,
            }) => {
                let vm = console.vm_name().to_string();
                let project = console.project().to_path_buf();
                app.attach_console(console);
                app.queue_auto_launch(&vm, harness, &project, started);
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
            Ok(ScanMessage::VmsChanged { note, level }) => {
                app.refresh_vms();
                app.toast(note, level);
            }
            Ok(ScanMessage::ProjectAssigned { vm, note, first }) => {
                app.refresh_vms();
                app.toast(note, ToastLevel::Info);
                if first {
                    app.pending_apply.remove(&vm);
                    auto_apply.push(vm);
                } else {
                    app.prompt_apply(&vm);
                }
            }
            Ok(ScanMessage::VmApplied { vm, note }) => {
                app.pending_apply.remove(&vm);
                app.refresh_vms();
                app.toast(note, ToastLevel::Info);
            }
            Ok(ScanMessage::VmStopped { vm, note }) => {
                if app.attached_vm.as_deref() == Some(vm.as_str()) {
                    app.detach_console();
                    app.attached_vm = None;
                }
                app.clear_provision();
                app.refresh_vms();
                app.toast(note, ToastLevel::Info);
            }
            Ok(ScanMessage::Error(err)) => {
                if app.loading {
                    app.loading = false;
                    app.toast(format!("project scan failed: {err}"), ToastLevel::Error);
                } else {
                    app.toast(err, ToastLevel::Error);
                }
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
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    auto_apply
}

/// Open the doctor on first launch when anything is broken or hegn can prepare
/// the host without further input.
pub(super) fn should_show_doctor(report: &DoctorReport) -> bool {
    !report.ok || report.can_setup()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;
    use crate::project::{GitSummary, Project};
    use crate::session::console::TerminalConsole;
    use crate::tui::app::{ConfirmAction, Modal, Screen, ToastLevel, VmUiState};
    use crate::vm::images::DownloadProgress;
    use std::path::{Path, PathBuf};

    struct FakeConsole {
        project: PathBuf,
    }

    impl TerminalConsole for FakeConsole {
        fn send(&mut self, _bytes: &[u8]) -> anyhow::Result<()> {
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
            "fake"
        }
    }

    #[allow(clippy::field_reassign_with_default)]
    fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.projects_root = dir.path().join("projects");
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.templates_dir = dir.path().join("templates");
        config.vms_dir = dir.path().join("vms");
        config.profiles_dir = dir.path().join("profiles");
        config.backend = crate::config::BackendKind::Auto;
        config.auto_branch = false;
        std::fs::create_dir_all(&config.projects_root).unwrap();
        std::fs::create_dir_all(&config.vms_dir).unwrap();
        let app = App::new(config);
        (dir, app)
    }

    fn toast(app: &App) -> (&str, ToastLevel) {
        let toast = app.toast.as_ref().expect("expected a toast");
        (toast.text.as_str(), toast.level)
    }

    fn project(app: &App) -> Project {
        Project {
            name: "demo".into(),
            path: app.config.projects_root.join("demo"),
            git: GitSummary::default(),
            last_opened: None,
        }
    }

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

    #[test]
    fn optional_doctor_failures_do_not_open_the_modal() {
        let report = DoctorReport {
            ok: true,
            checks: vec![crate::diagnostics::doctor::Check::new(
                "editor", false, false, "unset",
            )],
            assets: Vec::new(),
            setup: crate::diagnostics::doctor::SetupActions::default(),
        };
        assert!(!should_show_doctor(&report));
    }

    #[test]
    fn drain_scan_applies_projects_vm_changes_and_errors() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();
        let demo = project(&app);

        tx.send(ScanMessage::Projects(vec![demo])).unwrap();
        drain_scan(&mut app, &rx);
        assert_eq!(app.projects.len(), 1);
        assert_eq!(app.projects[0].name, "demo");
        assert_eq!(app.visible, vec![0]);
        assert!(!app.loading);

        tx.send(ScanMessage::VmsChanged {
            note: "VM list refreshed".into(),
            level: ToastLevel::Warn,
        })
        .unwrap();
        drain_scan(&mut app, &rx);
        assert_eq!(toast(&app), ("VM list refreshed", ToastLevel::Warn));

        tx.send(ScanMessage::Error("state file unreadable".into()))
            .unwrap();
        drain_scan(&mut app, &rx);
        assert_eq!(toast(&app), ("state file unreadable", ToastLevel::Error));

        app.loading = true;
        tx.send(ScanMessage::Error("permission denied".into()))
            .unwrap();
        drain_scan(&mut app, &rx);
        assert!(!app.loading);
        assert_eq!(
            toast(&app),
            ("project scan failed: permission denied", ToastLevel::Error)
        );
    }

    #[test]
    fn drain_scan_attaches_a_started_console_and_queues_the_launch() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();
        let project = app.config.projects_root.join("demo");

        tx.send(ScanMessage::Started {
            console: Box::new(FakeConsole {
                project: project.clone(),
            }),
            branch: Some(Ok("hegn/session-1".into())),
            harness: Harness::Opencode,
            started: true,
        })
        .unwrap();
        drain_scan(&mut app, &rx);

        assert!(app.session.is_some());
        assert_eq!(app.screen, Screen::Project);
        assert_eq!(app.open_project.as_deref(), Some(project.as_path()));
        assert!(
            app.pending_launch.is_some(),
            "auto_start + the default harness command queue a launch"
        );
        assert_eq!(
            toast(&app),
            ("session branch hegn/session-1", ToastLevel::Info)
        );
    }

    #[test]
    fn drain_scan_prompts_to_apply_after_an_assignment() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();

        tx.send(ScanMessage::ProjectAssigned {
            vm: "rust".into(),
            note: "assigned demo to VM rust".into(),
            first: false,
        })
        .unwrap();
        drain_scan(&mut app, &rx);

        let Some(Modal::Confirm(prompt)) = &app.modal else {
            panic!("expected the apply prompt, got {:?}", app.modal);
        };
        assert_eq!(prompt.title, "Restart VM?");
        assert!(
            matches!(&prompt.action, ConfirmAction::ApplyVm(vm) if vm == "rust"),
            "{:?}",
            prompt.action
        );
        assert_eq!(toast(&app), ("assigned demo to VM rust", ToastLevel::Info));
    }

    #[test]
    fn drain_scan_auto_applies_the_first_project_assignment() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();

        tx.send(ScanMessage::ProjectAssigned {
            vm: "rust".into(),
            note: "assigned demo to VM rust".into(),
            first: true,
        })
        .unwrap();
        let auto_apply = drain_scan(&mut app, &rx);

        assert_eq!(auto_apply, vec!["rust".to_string()]);
        assert!(app.modal.is_none(), "the first assignment must not prompt");
        assert!(!app.pending_apply.contains("rust"));
        assert_eq!(toast(&app), ("assigned demo to VM rust", ToastLevel::Info));
    }

    #[test]
    fn drain_scan_clears_pending_apply_when_the_vm_is_applied() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();
        app.pending_apply.insert("rust".into());

        tx.send(ScanMessage::VmApplied {
            vm: "rust".into(),
            note: "VM rust restarted with the current config".into(),
        })
        .unwrap();
        drain_scan(&mut app, &rx);

        assert!(!app.apply_pending("rust"));
        assert_eq!(
            toast(&app),
            (
                "VM rust restarted with the current config",
                ToastLevel::Info
            )
        );
    }

    #[test]
    fn drain_scan_clears_provisioning_when_the_vm_stops() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();
        let plan = crate::agent::provision::ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        app.set_provision_progress(crate::agent::provision::ProvisionProgress::new(&plan));
        assert!(app.provision.is_some());

        tx.send(ScanMessage::VmStopped {
            vm: "rust".into(),
            note: "VM rust stopped".into(),
        })
        .unwrap();
        drain_scan(&mut app, &rx);

        assert!(app.provision.is_none());
    }

    #[test]
    fn drain_scan_reports_branch_and_start_failures() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();
        let project = app.config.projects_root.join("demo");

        tx.send(ScanMessage::Started {
            console: Box::new(FakeConsole {
                project: project.clone(),
            }),
            branch: Some(Err("branch already exists".into())),
            harness: Harness::Shell,
            started: false,
        })
        .unwrap();
        drain_scan(&mut app, &rx);
        assert_eq!(
            toast(&app),
            (
                "branch not created: branch already exists",
                ToastLevel::Warn
            )
        );
        assert!(
            app.pending_launch.is_none(),
            "a shell harness and an attach-only start queue nothing"
        );

        tx.send(ScanMessage::StartFailed("missing kernel image".into()))
            .unwrap();
        drain_scan(&mut app, &rx);
        assert!(app.session.is_none());
        assert_eq!(app.vm.state, VmUiState::Failed);
        assert_eq!(
            toast(&app),
            ("VM start failed: missing kernel image", ToastLevel::Error)
        );
        match &app.modal {
            Some(Modal::Doctor(modal)) => {
                assert!(
                    modal
                        .error
                        .as_deref()
                        .is_some_and(|error| error.contains("missing kernel"))
                );
            }
            other => panic!("expected the doctor modal, got {other:?}"),
        }
    }

    #[test]
    fn drain_scan_tracks_provision_progress_ready_and_failure() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();
        let plan = crate::agent::provision::ProvisionPlan::new(Harness::Shell, &[]).unwrap();

        tx.send(ScanMessage::Provision(
            crate::agent::provision::ProvisionProgress::new(&plan),
        ))
        .unwrap();
        drain_scan(&mut app, &rx);
        let progress = app.provision.as_ref().expect("provision progress");
        assert_eq!(progress.harness_label(), "shell");

        tx.send(ScanMessage::ProvisionReady).unwrap();
        drain_scan(&mut app, &rx);
        assert!(app.provision.is_none());
        assert_eq!(toast(&app), ("harness preset ready", ToastLevel::Info));

        tx.send(ScanMessage::ProvisionFailed("no network".into()))
            .unwrap();
        drain_scan(&mut app, &rx);
        assert!(app.provision.is_none());
        assert_eq!(
            toast(&app),
            ("harness setup failed: no network", ToastLevel::Error)
        );
    }

    #[test]
    fn drain_scan_replaces_download_progress_with_the_doctor_report() {
        let (_dir, mut app) = test_app();
        let (tx, rx) = mpsc::channel();
        app.download = Some(DownloadProgress {
            asset: "kernel",
            received: 10,
            total: Some(100),
        });

        tx.send(ScanMessage::DownloadProgress(DownloadProgress {
            asset: "rootfs",
            received: 25,
            total: Some(200),
        }))
        .unwrap();
        drain_scan(&mut app, &rx);
        let download = app.download.as_ref().expect("download progress");
        assert_eq!(download.asset, "rootfs");
        assert_eq!(download.received, 25);

        tx.send(ScanMessage::Doctor {
            report: DoctorReport {
                ok: true,
                checks: Vec::new(),
                assets: Vec::new(),
                setup: crate::diagnostics::doctor::SetupActions::default(),
            },
            note: Some("images verified".into()),
        })
        .unwrap();
        drain_scan(&mut app, &rx);

        assert!(app.download.is_none(), "the doctor replaces progress");
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));
        assert_eq!(toast(&app), ("images verified", ToastLevel::Info));
    }
}
