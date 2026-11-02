//! Dashboard screen: project list and the right-hand preview column.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, RowHit};

use super::super::text::truncate;
use super::super::theme::{GREEN, MUTED, TEXT, panel_block};
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
                    "Enter opens a project; c creates one.",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return;
    };

    let instances: Vec<_> = app
        .instances
        .iter()
        .filter(|instance| crate::vm::instance::same_project(&instance.project, &project.path))
        .collect();
    let running = instances
        .iter()
        .filter(|instance| instance.state == "running")
        .count();
    let snapshots = app
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.project.as_deref() == Some(project.path.as_path()))
        .count();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" instances ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{} ({} running)", instances.len(), running),
                Style::default().fg(if running > 0 { GREEN } else { TEXT }),
            ),
        ]),
        Line::from(vec![
            Span::styled(" backups   ", Style::default().fg(MUTED)),
            Span::styled(snapshots.to_string(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" last open ", Style::default().fg(MUTED)),
            Span::styled(
                project
                    .last_opened
                    .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "never".into()),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" harness   ", Style::default().fg(MUTED)),
            Span::styled(app.config.harness.label(), Style::default().fg(TEXT)),
        ]),
        Line::from(Span::styled(
            "Enter opens the project · e editor · g lazygit",
            Style::default().fg(MUTED),
        )),
    ];
    for instance in instances.iter().take(3) {
        lines.push(Line::from(vec![
            Span::styled(
                " ● ",
                Style::default().fg(if instance.state == "running" {
                    GREEN
                } else {
                    MUTED
                }),
            ),
            Span::styled(truncate(&instance.name, 24), Style::default().fg(TEXT)),
            Span::styled(format!("  {}", instance.state), Style::default().fg(MUTED)),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

#[cfg(test)]
mod tests {
    use crate::tui::ui::test_support::*;

    #[test]
    fn dashboard_renders_projects_and_preview() {
        let (_dir, app) = app();
        let (text, hits) = render(&app);
        assert!(text.contains("hegn"));
        assert!(text.contains("projects (1)"));
        assert!(text.contains("preview"));
        assert!(text.contains("instances"));
        assert!(text.contains("backups"));
        assert!(text.contains("demo"));
        assert!(!hits.footer.is_empty());
        assert_eq!(hits.projects.len(), 1);
    }
}
