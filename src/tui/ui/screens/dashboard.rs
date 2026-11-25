//! Dashboard screen: project list and the right-hand preview column.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, RowHit};

use super::super::theme::{GREEN, MUTED, TEXT, YELLOW, panel_block};
use super::project::render_project_panel;
use super::projects::render_projects;

pub(crate) fn render_dashboard(
    frame: &mut Frame,
    app: &App,
    area: Rect,
) -> (Vec<RowHit>, Vec<RowHit>, Vec<RowHit>) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(20)])
        .split(area);
    let projects = render_projects(frame, app, columns[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(6)])
        .spacing(1)
        .split(columns[1]);
    render_project_panel(frame, app, right[0]);
    render_dashboard_preview(frame, app, right[1]);
    (projects, Vec::new(), Vec::new())
}

pub(crate) fn render_dashboard_preview(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(" preview ".to_string(), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(project) = app.selected_project() else {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    "No project selected.",
                    Style::default().fg(TEXT),
                )),
                Line::from(Span::styled(
                    "Enter opens and attaches a project; c creates one.",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return;
    };

    let vm_name = app.project_vm_name(&project.path);
    let mut lines = Vec::new();
    if let Some(vm) = vm_name {
        let summary = app.project_vm(&project.path);
        let harness = summary
            .map(|summary| summary.harness.label().to_string())
            .unwrap_or_else(|| "—".to_string());
        let members = summary.map(|summary| summary.members).unwrap_or(0);
        let state = summary
            .map(|summary| summary.state.as_str())
            .unwrap_or("stopped");
        let newest = app
            .newest_snapshot_for_vm(vm)
            .map(|snapshot| {
                format!(
                    "{} · {}",
                    snapshot.created_at.format("%Y-%m-%d %H:%M"),
                    snapshot.size_label()
                )
            })
            .unwrap_or_else(|| "none".into());
        let guest = app
            .project_guest_path(&project.path)
            .unwrap_or_else(|| "not exported".into());
        lines.push(Line::from(vec![
            Span::styled(" vm        ", Style::default().fg(MUTED)),
            Span::styled(format!("⬢ {vm}"), Style::default().fg(TEXT)),
            Span::styled(
                format!("  {state}"),
                Style::default().fg(if state == "running" { GREEN } else { YELLOW }),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" harness   ", Style::default().fg(MUTED)),
            Span::styled(harness, Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" members   ", Style::default().fg(MUTED)),
            Span::styled(members.to_string(), Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" snapshot  ", Style::default().fg(MUTED)),
            Span::styled(newest, Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" guest     ", Style::default().fg(MUTED)),
            Span::styled(guest, Style::default().fg(TEXT)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(" vm        ", Style::default().fg(MUTED)),
            Span::styled("unassigned", Style::default().fg(YELLOW)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" last open ", Style::default().fg(MUTED)),
            Span::styled(
                project
                    .last_opened
                    .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "never".into()),
                Style::default().fg(TEXT),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            " press a to pick a VM · n creates a new one",
            Style::default().fg(MUTED),
        )));
    }
    lines.push(Line::from(Span::styled(
        "Enter opens · v VM screen · F6 presentation · e editor · g lazygit",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

#[cfg(test)]
mod tests {
    use crate::tui::ui::test_support::*;

    #[test]
    fn dashboard_renders_projects_and_vm_preview() {
        let (_dir, app) = app();
        let (text, hits) = render(&app);
        assert!(text.contains("hegn"));
        assert!(text.contains("projects (1)"));
        assert!(text.contains("preview"));
        assert!(text.contains("vm"));
        assert!(text.contains("unassigned"));
        assert!(text.contains("demo"));
        assert!(!hits.footer.is_empty());
        assert_eq!(hits.projects.len(), 1);
    }

    #[test]
    fn preview_shows_the_assigned_vm_fields() {
        let (_dir, mut app) = app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        if let Some(row) = app.projects.get_mut(0) {
            row.path = project.clone();
        }
        app.vms = vec![crate::vm::registry::VmSummary {
            name: "rust".into(),
            harness: crate::config::Harness::Opencode,
            primary: Some("demo".into()),
            members: 3,
            project: project.clone(),
            workspace: project.clone(),
            created_at: chrono::Utc::now(),
            state: "running".into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.vms_dir.join("rust/vm.toml"),
            vcpus: 4,
            memory_mib: 4096,
            error: None,
        }];
        crate::tui::ui::test_support::assign_project(&mut app, "demo", "rust");
        let (text, _) = render(&app);
        assert!(text.contains("⬢ rust"), "{text}");
        assert!(text.contains("OpenCode"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("members"), "{text}");
        assert!(text.contains("snapshot"), "{text}");
        assert!(text.contains("guest"), "{text}");
    }
}
