//! Shared fixtures for the renderer tests.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

use crate::config::Config;
use crate::project::{GitSummary, Project};
use crate::tui::app::App;
use crate::tui::ui::{UiHits, draw};

use std::path::PathBuf;

#[allow(clippy::field_reassign_with_default)]
pub(crate) fn app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.projects_root = dir.path().join("projects");
    config.state_dir = dir.path().join("state");
    config.vms_dir = dir.path().join("vms");
    config.snapshots_dir = dir.path().join("snapshots");
    config.templates_dir = dir.path().join("templates");
    config.profiles_dir = dir.path().join("profiles");
    config.backend = crate::config::BackendKind::Auto;
    config.auto_branch = false;
    std::fs::create_dir_all(&config.projects_root).unwrap();
    let mut app = App::new(config);
    app.set_projects(vec![Project {
        name: "demo".into(),
        path: PathBuf::from("/projects/demo"),
        git: GitSummary {
            is_repo: true,
            branch: Some("main".into()),
            modified: 1,
            ..GitSummary::default()
        },
        last_opened: None,
    }]);
    (dir, app)
}

/// Register `name` under the app's projects root and assign it to `vm`.
pub(crate) fn assign_project(app: &mut App, name: &str, vm: &str) {
    use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};

    let path = app.config.projects_root.join(name);
    std::fs::create_dir_all(&path).unwrap();
    let canonical = path.canonicalize().unwrap();
    let mut registry = ProjectRegistry::load(&app.config.state_dir).unwrap();
    registry
        .upsert(ProjectEntry {
            name: name.into(),
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
    registry.assign(name, vm, false).unwrap();
    app.refresh_project_entries();
}

pub(crate) fn render(app: &App) -> (String, UiHits) {
    render_sized(app, 120, 42)
}

pub(crate) fn render_sized(app: &App, width: u16, height: u16) -> (String, UiHits) {
    let (text, hits, _) = render_buffer(app, width, height);
    (text, hits)
}

pub(crate) fn render_buffer(app: &App, width: u16, height: u16) -> (String, UiHits, Buffer) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = UiHits::default();
    terminal
        .draw(|frame| {
            hits = draw(frame, app);
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    (text, hits, buffer)
}
