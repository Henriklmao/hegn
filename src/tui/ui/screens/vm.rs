//! Reusable VM screen: projects, snapshots, tools and health.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::tui::app::{App, VmSection, VmSectionHit};
use crate::tui::ui::RenderHits;

use super::health::render_health;
use super::snapshots::render_snapshots;
use super::vm_projects::render_vm_projects;
use super::vm_tools::render_vm_tools;

pub(crate) fn render_vm(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .spacing(1)
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .spacing(1)
        .split(columns[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .spacing(1)
        .split(columns[1]);

    let projects = render_vm_projects(frame, app, left[0], 1);
    render_vm_tools(frame, app, left[1], 3);
    let snapshots = render_snapshots(frame, app, right[0], 2);
    render_health(frame, app, right[1], 4);

    RenderHits {
        vm_members: projects,
        snapshots,
        vm_sections: vec![
            VmSectionHit {
                rect: left[0],
                section: VmSection::Projects,
            },
            VmSectionHit {
                rect: right[0],
                section: VmSection::Snapshots,
            },
            VmSectionHit {
                rect: left[1],
                section: VmSection::Tools,
            },
            VmSectionHit {
                rect: right[1],
                section: VmSection::Health,
            },
        ],
        ..RenderHits::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;
    use crate::tui::app::Screen;
    use crate::tui::ui::test_support::*;
    use crate::vm::registry::VmSummary;
    use std::path::PathBuf;

    fn summary(app: &App) -> VmSummary {
        VmSummary {
            name: "rust".into(),
            harness: Harness::Opencode,
            primary: Some("parser".into()),
            members: 2,
            project: PathBuf::from("/projects/parser"),
            workspace: PathBuf::from("/projects/parser"),
            created_at: chrono::Utc::now(),
            state: "running".into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.vms_dir.join("rust/vm.toml"),
            vcpus: 4,
            memory_mib: 4096,
            error: None,
        }
    }

    #[test]
    fn vm_screen_renders_all_four_sections_and_tools() {
        let (_dir, mut app) = app();
        app.vms = vec![summary(&app)];
        app.open_vm_screen("rust".into());
        assert_eq!(app.screen, Screen::Vm);

        let (text, hits) = render(&app);
        for needle in [
            "projects",
            "snapshots",
            "tools",
            "health",
            "OpenCode",
            "4 vCPU",
        ] {
            assert!(text.contains(needle), "missing {needle} in:\n{text}");
        }
        assert_eq!(hits.vm_sections.len(), 4);
        assert!(text.contains("[1]"), "{text}");
        assert!(text.contains("[3]"), "{text}");
    }
}
