//! Shared fixtures for the TUI state-machine unit tests.
//!
//! The helper re-exports also give each test module a single place to import
//! the external types it asserts on.

#![allow(clippy::field_reassign_with_default)]

pub(super) use std::path::PathBuf;
pub(super) use std::sync::atomic::{AtomicBool, Ordering};
pub(super) use std::sync::{Arc, Mutex};

pub(super) use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
pub(super) use ratatui::layout::Rect;

pub(super) use crate::agent::launch::Integration;
pub(super) use crate::config::form::{ConfigField, ConfigKind};
pub(super) use crate::config::{Config, Harness};
pub(super) use crate::project::{GitSummary, Project};
pub(super) use crate::session::console::TerminalConsole;
pub(super) use crate::session::snapshot::{SnapshotMeta, SnapshotStore};
pub(super) use crate::vm::registry::VmSummary;

use super::App;

pub(super) fn test_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.projects_root = dir.path().join("projects");
    config.state_dir = dir.path().join("state");
    config.snapshots_dir = dir.path().join("snapshots");
    config.templates_dir = dir.path().join("templates");
    config.vms_dir = dir.path().join("vms");
    config.profiles_dir = dir.path().join("profiles");
    config.backend = crate::config::BackendKind::Auto;
    config.auto_branch = false;
    std::fs::create_dir_all(&config.projects_root).unwrap();
    std::fs::create_dir_all(&config.snapshots_dir).unwrap();
    let app = App::new(config);
    (dir, app)
}

pub(super) struct FakeConsole {
    pub(super) project: PathBuf,
    pub(super) output: Vec<Vec<u8>>,
    pub(super) sent: Vec<u8>,
    pub(super) exited: bool,
}

impl FakeConsole {
    pub(super) fn new(project: impl Into<PathBuf>) -> Self {
        Self {
            project: project.into(),
            output: Vec::new(),
            sent: Vec::new(),
            exited: false,
        }
    }
}

impl TerminalConsole for FakeConsole {
    fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.sent.extend_from_slice(bytes);
        Ok(())
    }

    fn drain(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.output)
    }

    fn is_exited(&mut self) -> bool {
        self.exited
    }

    fn project(&self) -> &std::path::Path {
        &self.project
    }

    fn label(&self) -> &str {
        "fake · serial"
    }

    fn vm_name(&self) -> &str {
        "fake"
    }
}

/// Shared handle for a [`RecordingConsole`], so a test can inspect what the
/// UI actually forwarded to the guest after the console was boxed.
#[derive(Clone, Default)]
pub(super) struct ConsoleLog {
    sent: Arc<Mutex<Vec<u8>>>,
    output: Arc<Mutex<Vec<Vec<u8>>>>,
    exited: Arc<AtomicBool>,
}

impl ConsoleLog {
    pub(super) fn console(&self, project: impl Into<PathBuf>) -> Box<dyn TerminalConsole> {
        self.console_named(project, "fake")
    }

    pub(super) fn console_named(
        &self,
        project: impl Into<PathBuf>,
        name: &str,
    ) -> Box<dyn TerminalConsole> {
        Box::new(RecordingConsole {
            log: self.clone(),
            project: project.into(),
            name: name.to_string(),
        })
    }

    pub(super) fn sent(&self) -> Vec<u8> {
        self.sent.lock().unwrap().clone()
    }

    pub(super) fn sent_text(&self) -> String {
        String::from_utf8_lossy(&self.sent()).into_owned()
    }

    pub(super) fn push_output(&self, bytes: &[u8]) {
        self.output.lock().unwrap().push(bytes.to_vec());
    }

    pub(super) fn set_exited(&self, exited: bool) {
        self.exited.store(exited, Ordering::SeqCst);
    }
}

pub(super) struct RecordingConsole {
    log: ConsoleLog,
    project: PathBuf,
    name: String,
}

impl TerminalConsole for RecordingConsole {
    fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.log.sent.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }

    fn drain(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.log.output.lock().unwrap())
    }

    fn is_exited(&mut self) -> bool {
        self.log.exited.load(Ordering::SeqCst)
    }

    fn project(&self) -> &std::path::Path {
        &self.project
    }

    fn label(&self) -> &str {
        "recording · serial"
    }

    fn vm_name(&self) -> &str {
        &self.name
    }
}

pub(super) fn fake_project(name: &str) -> Project {
    Project {
        name: name.into(),
        path: PathBuf::from(format!("/projects/{name}")),
        git: GitSummary::default(),
        last_opened: None,
    }
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn fake_vm(app: &App, name: &str, workspace: PathBuf) -> VmSummary {
    VmSummary {
        name: name.into(),
        harness: Harness::Shell,
        primary: Some("demo".into()),
        members: 1,
        project: app.config.projects_root.join("demo"),
        workspace,
        created_at: chrono::Utc::now(),
        state: "stopped".into(),
        vm_pid: None,
        serial_log: None,
        config_path: app.config.vms_dir.join(name).join("vm.toml"),
        vcpus: 2,
        memory_mib: 2048,
        error: None,
    }
}

/// Register a fake project and assign it to `vm`, then reload the app's
/// registry snapshot. The project directory must already exist.
pub(super) fn assign_fake_project(app: &mut App, path: &std::path::Path, vm: &str) {
    use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};

    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "demo".into());
    let mut registry = ProjectRegistry::load(&app.config.state_dir).unwrap();
    registry
        .upsert(ProjectEntry {
            name: name.clone(),
            path: canonical,
            vm: None,
            read_only: false,
            kind: ProjectKind::External,
            parent: None,
            managed: false,
            hidden: false,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    registry.assign(&name, vm, false).unwrap();
    app.refresh_project_entries();
}

/// Install a VM list and select the matching fake project, so the
/// project-scoped VM filter has something to work with.
pub(super) fn install_vms(app: &mut App, vms: Vec<VmSummary>) {
    if let Some(project) = vms.first().map(|vm| vm.project.clone()) {
        let name = project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.display().to_string());
        app.projects = vec![Project {
            name,
            path: project,
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.vm_project = None;
    }
    app.vms = vms;
    app.refresh_vm_filter();
}
