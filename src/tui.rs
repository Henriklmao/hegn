//! Terminal lifecycle and the blocking TUI event loop.

use std::io::{Stdout, stdout};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, Effect, ToastLevel};
use crate::config::{Config, Harness};
use crate::console::TerminalConsole;
use crate::doctor::DoctorReport;
use crate::project;
use crate::ui;

const TICK: Duration = Duration::from_millis(50);

enum ScanMessage {
    Projects(Vec<project::Project>),
    Started {
        console: Box<dyn TerminalConsole>,
        branch: Option<Result<String, String>>,
    },
    StartFailed(String),
    SandboxesChanged {
        note: String,
        level: ToastLevel,
    },
    SandboxError(String),
    Doctor {
        report: DoctorReport,
        note: Option<String>,
    },
    Error(String),
}

/// Run the interactive TUI until the user quits.
pub fn run(config: Config) -> Result<()> {
    let mut app = App::new(config);
    // First launch / a broken host: surface the doctor immediately instead of
    // waiting for a VM start to fail. The "Set up everything" action is an
    // explicit confirmation inside the modal.
    let report = crate::doctor::run(&app.config);
    if should_show_doctor(&report) {
        app.show_doctor(report, None);
    }
    let (scan_tx, scan_rx) = mpsc::channel();
    spawn_scan(app.config.clone(), scan_tx.clone());

    // Build the runtime before taking over the terminal so a failure cannot
    // leave the user in raw mode on the alternate screen.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build async runtime")?;
    let mut terminal = setup_terminal()?;

    let result = run_loop(&mut terminal, &mut app, &scan_tx, &scan_rx, &runtime);
    restore_terminal(&mut terminal)?;
    result
}

fn run_loop(
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
        app.sandbox_rows = hits.sandboxes;
        app.snapshot_rows = hits.snapshots;
        app.config_rows = hits.config;
        app.config_steppers = hits.config_steppers;
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

        if app.should_quit {
            return Ok(());
        }
    }
}

fn drain_scan(app: &mut App, scan_rx: &Receiver<ScanMessage>) {
    loop {
        match scan_rx.try_recv() {
            Ok(ScanMessage::Projects(projects)) => app.set_projects(projects),
            Ok(ScanMessage::Started { console, branch }) => {
                app.attach_console(console);
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
            Ok(ScanMessage::SandboxesChanged { note, level }) => {
                app.refresh_sandboxes();
                app.toast(note, level);
            }
            Ok(ScanMessage::SandboxError(err)) => {
                app.refresh_sandboxes();
                app.toast(err, ToastLevel::Error);
            }
            Ok(ScanMessage::Doctor { report, note }) => app.show_doctor(report, note),
            Ok(ScanMessage::Error(err)) => {
                app.loading = false;
                app.toast(format!("project scan failed: {err}"), ToastLevel::Error);
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn spawn_scan(config: Config, tx: mpsc::Sender<ScanMessage>) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-project-scan".into())
        .spawn(move || {
            let protected = config.protected_paths();
            match project::discover_protected(&config.projects_root, &protected) {
                Ok(projects) => {
                    let _ = thread_tx.send(ScanMessage::Projects(projects));
                }
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::Error(err.to_string()));
                }
            }
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::Error(format!(
            "could not start project scanner: {err}"
        )));
    }
}

fn spawn_image_download(config: Config, tx: mpsc::Sender<ScanMessage>) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-images".into())
        .spawn(move || {
            let result = crate::images::download_missing(&config, false);
            let report = crate::doctor::run(&config);
            let note = Some(match result {
                Ok(_) => "VM images downloaded and verified".to_string(),
                Err(err) => format!("image download failed: {err:#}"),
            });
            let _ = thread_tx.send(ScanMessage::Doctor { report, note });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::Error(format!(
            "could not start image download: {err}"
        )));
    }
}

fn spawn_attach(config: Config, sandbox: String, tx: mpsc::Sender<ScanMessage>) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-attach-vm".into())
        .spawn(move || {
            let registry = crate::sandbox::SandboxRegistry::new(config);
            let result = registry
                .manager_by_name(&sandbox)
                .and_then(|manager| manager.ensure_console());
            match result {
                Ok(console) => {
                    let _ = thread_tx.send(ScanMessage::Started {
                        console: Box::new(console),
                        branch: None,
                    });
                }
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::StartFailed(err.to_string()));
                }
            }
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::StartFailed(format!(
            "could not start the attach thread: {err}"
        )));
    }
}

fn spawn_clones(
    config: Config,
    source: String,
    count: usize,
    harness: Harness,
    start: bool,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-clone-agents".into())
        .spawn(move || {
            let registry = crate::sandbox::SandboxRegistry::new(config.clone());
            let source = match registry.resolve(&source) {
                Ok(source) => source,
                Err(err) => {
                    // Clone failures must not tear down the attached VM.
                    let _ = thread_tx.send(ScanMessage::SandboxError(format!(
                        "could not create clones: {err}"
                    )));
                    return;
                }
            };
            let mut created = 0;
            let mut failures = Vec::new();
            let mut warnings = Vec::new();
            for index in 1..=count {
                let base = format!("{}-agent", source.name);
                let name = match registry.unique_name(&format!("{base}-{index}")) {
                    Ok(name) => name,
                    Err(err) => {
                        failures.push(err.to_string());
                        continue;
                    }
                };
                match registry.clone_from(&source, &name, Some(harness)) {
                    Ok(outcome) => {
                        for warning in &outcome.warnings {
                            if !warnings.iter().any(|existing| existing == warning) {
                                warnings.push(warning.clone());
                            }
                        }
                        if start && let Err(err) = registry.manager(outcome.sandbox.clone()).start()
                        {
                            failures.push(format!("{name}: {err}"));
                        }
                        created += 1;
                    }
                    Err(err) => failures.push(format!("{name}: {err}")),
                }
            }
            let mut note = if failures.is_empty() {
                format!("created {created} clone(s) for parallel agents")
            } else {
                format!(
                    "created {created} clone(s); {} failed: {}",
                    failures.len(),
                    failures.join("; ")
                )
            };
            if !warnings.is_empty() {
                note.push_str("; warning: ");
                note.push_str(&warnings.join("; "));
            }
            let level = if failures.is_empty() && warnings.is_empty() {
                ToastLevel::Info
            } else {
                ToastLevel::Warn
            };
            let _ = thread_tx.send(ScanMessage::SandboxesChanged { note, level });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::SandboxError(format!(
            "could not start the clone thread: {err}"
        )));
    }
}

fn spawn_start(
    config: Config,
    project: std::path::PathBuf,
    harness: Harness,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-start-vm".into())
        .spawn(move || {
            let registry = crate::sandbox::SandboxRegistry::new(config.clone());
            let sandbox = match registry.ensure_for_project(&project, harness) {
                Ok(sandbox) => sandbox,
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::StartFailed(err.to_string()));
                    return;
                }
            };
            let workspace = sandbox.workspace.clone();
            let auto_branch = sandbox.auto_branch;
            let manager = registry.manager(sandbox);
            match manager.ensure_console() {
                Ok(console) => {
                    let branch = if auto_branch && project::read_git_summary(&workspace).is_repo {
                        Some(
                            project::create_session_branch(&workspace)
                                .map_err(|err| err.to_string()),
                        )
                    } else {
                        None
                    };
                    let _ = thread_tx.send(ScanMessage::Started {
                        console: Box::new(console),
                        branch,
                    });
                }
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::StartFailed(err.to_string()));
                }
            }
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::StartFailed(format!(
            "could not start the VM start thread: {err}"
        )));
    }
}

fn handle_effects(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    effects: Vec<Effect>,
    scan_tx: &mpsc::Sender<ScanMessage>,
    runtime: &tokio::runtime::Runtime,
) -> Result<()> {
    for effect in effects {
        match effect {
            Effect::Quit => app.should_quit = true,
            Effect::RefreshProjects => {
                app.loading = true;
                app.toast("refreshing projects…", ToastLevel::Info);
                spawn_scan(app.config.clone(), scan_tx.clone());
            }
            Effect::DownloadImages => {
                app.toast("downloading VM images…", ToastLevel::Info);
                spawn_image_download(app.config.clone(), scan_tx.clone());
            }
            Effect::SetupEnvironment => {
                let actions = crate::doctor::setup_actions(&app.config);
                let mut notes = Vec::new();
                let mut failed = false;
                let mut modal = crate::app::DoctorModal::new(crate::doctor::run(&app.config));
                modal.setting_up = true;
                app.modal = Some(crate::app::Modal::Doctor(modal));
                app.toast("setting up hegn…", ToastLevel::Info);

                if actions.create_dirs {
                    match app.config.ensure_dirs() {
                        Ok(()) => notes.push("directories created".to_string()),
                        Err(err) => {
                            failed = true;
                            notes.push(format!("directory setup failed: {err:#}"));
                        }
                    }
                }
                if actions.init_config {
                    match app.config.write_default_if_missing() {
                        Ok(true) => notes.push("default config written".to_string()),
                        Ok(false) => {}
                        Err(err) => {
                            failed = true;
                            notes.push(format!("config setup failed: {err:#}"));
                        }
                    }
                }
                if actions.configure_network {
                    app.toast("configuring the guarded network…", ToastLevel::Info);
                    match with_suspended_terminal(terminal, setup_network) {
                        Ok(()) => notes.push("guarded network configured".to_string()),
                        Err(err) => {
                            failed = true;
                            notes.push(format!("network setup failed: {err:#}"));
                        }
                    }
                }
                if actions.download_images {
                    app.toast("downloading VM images…", ToastLevel::Info);
                    spawn_image_download(app.config.clone(), scan_tx.clone());
                } else {
                    let note = if notes.is_empty() {
                        "nothing to set up".to_string()
                    } else {
                        notes.join("; ")
                    };
                    app.show_doctor(crate::doctor::run(&app.config), Some(note));
                }
                if failed && actions.download_images {
                    app.toast("some setup steps failed; see the doctor", ToastLevel::Error);
                }
            }
            Effect::OpenEditor(path) => {
                match with_suspended_terminal(terminal, || open_editor(&path)) {
                    Ok(()) => {
                        app.reload_after_edit();
                        app.toast("returned from editor", ToastLevel::Info);
                    }
                    Err(err) => app.toast(format!("editor failed: {err}"), ToastLevel::Error),
                }
            }
            Effect::OpenLazygit(path) => {
                if let Err(err) = with_suspended_terminal(terminal, || open_lazygit(&path)) {
                    app.toast(format!("lazygit failed: {err}"), ToastLevel::Error);
                }
            }
            Effect::StartProjectSandbox { project, harness } => {
                app.toast("starting VM…", ToastLevel::Info);
                spawn_start(app.config.clone(), project, harness, scan_tx.clone());
            }
            Effect::AttachSandbox { sandbox } => {
                app.toast(format!("attaching to {sandbox}…"), ToastLevel::Info);
                spawn_attach(app.config.clone(), sandbox, scan_tx.clone());
            }
            Effect::StopSandbox { sandbox } => {
                let registry = crate::sandbox::SandboxRegistry::new(app.config.clone());
                match registry.stop(&sandbox) {
                    Ok(()) => {
                        app.toast(format!("sandbox {sandbox} stopped"), ToastLevel::Info);
                        app.refresh_sandboxes();
                    }
                    Err(err) => app.toast(format!("stop failed: {err}"), ToastLevel::Error),
                }
            }
            Effect::DeleteSandbox {
                sandbox,
                delete_files,
            } => {
                let registry = crate::sandbox::SandboxRegistry::new(app.config.clone());
                match registry.delete(&sandbox, delete_files) {
                    Ok(outcome) => {
                        let message = match outcome.kept_workspace {
                            Some(path) => format!(
                                "sandbox {sandbox} removed; files kept at {}",
                                path.display()
                            ),
                            None if delete_files => format!("sandbox {sandbox} and files deleted"),
                            None => format!("sandbox {sandbox} removed; project files kept"),
                        };
                        app.toast(message, ToastLevel::Info);
                        app.refresh_sandboxes();
                    }
                    Err(err) => app.toast(format!("delete failed: {err}"), ToastLevel::Error),
                }
            }
            Effect::CloneSandbox {
                source,
                count,
                harness,
                start,
            } => {
                app.toast(format!("creating {count} clone(s)…"), ToastLevel::Info);
                spawn_clones(
                    app.config.clone(),
                    source,
                    count,
                    harness,
                    start,
                    scan_tx.clone(),
                );
            }
            Effect::RefreshSandboxes => app.refresh_sandboxes(),
            Effect::RestoreSnapshot(snapshot) => {
                let registry = crate::sandbox::SandboxRegistry::new(app.config.clone());
                let sandbox = snapshot
                    .sandbox
                    .as_deref()
                    .and_then(|name| registry.store().load(name).ok())
                    .or_else(|| {
                        snapshot
                            .project
                            .as_deref()
                            .and_then(|project| registry.resolve(&project.to_string_lossy()).ok())
                    });
                let Some(sandbox) = sandbox else {
                    app.toast("restore failed: no sandbox for snapshot", ToastLevel::Error);
                    return Ok(());
                };
                let manager = registry.manager(sandbox);
                match runtime.block_on(manager.restore_snapshot(&snapshot)) {
                    Ok(()) => match manager.attach_console() {
                        Ok(console) => {
                            app.attach_console(Box::new(console));
                            app.toast(format!("restored {}", snapshot.name), ToastLevel::Info);
                        }
                        Err(err) => app.fail_start(format!(
                            "snapshot restored, but console attach failed: {err}"
                        )),
                    },
                    Err(err) => app.toast(format!("restore failed: {err}"), ToastLevel::Error),
                }
            }
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
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

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
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

fn with_suspended_terminal<T>(
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

fn open_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$1\""))
        .arg("hegn-editor")
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    Ok(())
}

/// Open the doctor on first launch when anything is broken or hegn can prepare
/// the host without further input.
fn should_show_doctor(report: &DoctorReport) -> bool {
    !report.ok || report.can_setup()
}

fn setup_network() -> Result<()> {
    let user = crate::network::invoking_user()
        .context("could not determine the current user for TAP ownership")?;
    crate::network::setup_with_privilege(&user)
}

fn open_lazygit(path: &Path) -> Result<()> {
    let status = Command::new("lazygit")
        .current_dir(path)
        .status()
        .context("failed to launch lazygit (is it installed?)")?;
    if !status.success() {
        anyhow::bail!("lazygit exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn first_launch_opens_the_doctor_when_setup_is_available() {
        let clean = DoctorReport {
            ok: true,
            checks: Vec::new(),
            assets: Vec::new(),
            setup: crate::doctor::SetupActions::default(),
        };
        assert!(!should_show_doctor(&clean));

        let setup = DoctorReport {
            setup: crate::doctor::SetupActions {
                init_config: true,
                ..crate::doctor::SetupActions::default()
            },
            ..clean.clone()
        };
        assert!(should_show_doctor(&setup));

        let broken = DoctorReport { ok: false, ..clean };
        assert!(should_show_doctor(&broken));
    }

    #[test]
    fn vm_start_runs_off_the_ui_thread_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.projects_root = dir.path().join("projects");
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.templates_dir = dir.path().join("templates");
        config.kernel_image = dir.path().join("missing-kernel");
        config.rootfs_image = dir.path().join("missing-rootfs");
        std::fs::create_dir_all(&config.projects_root).unwrap();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn_start(config, project, Harness::Shell, tx);
        let message = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker did not report a result");
        assert!(matches!(message, ScanMessage::StartFailed(_)));
    }
}
