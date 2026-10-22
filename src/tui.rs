//! Terminal lifecycle and the blocking TUI event loop.

use std::io::{Stdout, stdout};
use std::path::{Path, PathBuf};
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

use crate::app::{App, Effect, SandboxState, ToastLevel};
use crate::config::Config;
use crate::firecracker::{FirecrackerClient, SnapshotLoad};
use crate::project;
use crate::ui;

const TICK: Duration = Duration::from_millis(50);

enum ScanMessage {
    Projects(Vec<project::Project>),
    Error(String),
}

/// Run the interactive TUI until the user quits.
pub fn run(config: Config) -> Result<()> {
    let mut app = App::new(config);
    let (scan_tx, scan_rx) = mpsc::channel();
    spawn_scan(app.config.clone(), scan_tx.clone());

    let mut terminal = setup_terminal()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build async runtime")?;

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
        app.snapshot_rows = hits.snapshots;

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
            Ok(ScanMessage::Error(err)) => {
                app.loading = false;
                app.toast(format!("project scan failed: {err}"), ToastLevel::Error);
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn spawn_scan(config: Config, tx: mpsc::Sender<ScanMessage>) {
    std::thread::Builder::new()
        .name("hegn-project-scan".into())
        .spawn(move || match project::discover(&config.projects_root) {
            Ok(projects) => {
                let _ = tx.send(ScanMessage::Projects(projects));
            }
            Err(err) => {
                let _ = tx.send(ScanMessage::Error(err.to_string()));
            }
        })
        .ok();
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
                app.toast("refreshing projects…", ToastLevel::Info);
                spawn_scan(app.config.clone(), scan_tx.clone());
            }
            Effect::OpenEditor(path) => {
                with_suspended_terminal(terminal, || open_editor(&path))?;
                app.toast("returned from editor", ToastLevel::Info);
            }
            Effect::OpenLazygit(path) => {
                with_suspended_terminal(terminal, || open_lazygit(&path))?;
            }
            Effect::RestoreSnapshot(snapshot) => {
                match runtime.block_on(restore_snapshot(&app.config, &snapshot)) {
                    Ok(()) => {
                        app.toast(format!("restored {}", snapshot.name), ToastLevel::Info);
                        app.sandbox.state = SandboxState::Paused;
                    }
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
    execute!(out, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(out);
    Terminal::new(backend).context("failed to create terminal")
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

async fn restore_snapshot(config: &Config, snapshot: &crate::snapshot::SnapshotMeta) -> Result<()> {
    let socket = firecracker_socket(config);
    if !socket.exists() {
        anyhow::bail!(
            "no Firecracker API socket at {} (start a VM first)",
            socket.display()
        );
    }
    FirecrackerClient::new(&socket)
        .load_snapshot(&SnapshotLoad {
            snapshot_path: snapshot.snapshot_path.display().to_string(),
            mem_file_path: snapshot.mem_file_path.display().to_string(),
            enable_diff_snapshots: None,
            resume_vm: Some(true),
        })
        .await?;
    Ok(())
}

pub fn firecracker_socket(config: &Config) -> PathBuf {
    config.run_dir().join("firecracker.sock")
}
