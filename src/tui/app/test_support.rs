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
pub(super) use crate::vm::instance::InstanceSummary;

use super::App;

pub(super) fn test_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.projects_root = dir.path().join("projects");
    config.state_dir = dir.path().join("state");
    config.snapshots_dir = dir.path().join("snapshots");
    config.templates_dir = dir.path().join("templates");
    config.instances_dir = dir.path().join("instances");
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

    fn instance_name(&self) -> &str {
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

    fn instance_name(&self) -> &str {
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

pub(super) fn fake_instance(app: &App, name: &str, workspace: PathBuf) -> InstanceSummary {
    InstanceSummary {
        name: name.into(),
        project: app.config.projects_root.join("demo"),
        workspace,
        harness: Harness::Shell,
        ephemeral: false,
        source_instance: None,
        created_at: chrono::Utc::now(),
        state: "stopped".into(),
        vm_pid: None,
        serial_log: None,
        config_path: app.config.instances_dir.join(name).join("hegn.toml"),
        vcpus: 2,
        memory_mib: 2048,
        error: None,
    }
}

/// Install an instance list and select the matching fake project, so the
/// project-scoped instance filter has something to work with.
pub(super) fn install_instances(app: &mut App, instances: Vec<InstanceSummary>) {
    if let Some(project) = instances.first().map(|instance| instance.project.clone()) {
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
        app.instance_project = None;
    }
    app.instances = instances;
    app.refresh_instance_filter();
}
