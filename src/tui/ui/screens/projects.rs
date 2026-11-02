//! Project sidebar: search field, git badges and clickable project rows.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, Focus, RowHit};

use super::super::theme::{
    ACCENT, CYAN, GREEN, MUTED, ON_ACCENT, TEXT, YELLOW, spinner, tab_block,
};

pub(crate) fn render_projects(frame: &mut Frame, app: &App, area: Rect) -> Vec<RowHit> {
    let active = app.focus == Focus::Projects;
    let title = format!(" projects ({}) ", app.projects.len());
    let block = tab_block(title, 1, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let query_line = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let mut query_spans = vec![Span::styled(
        " / ",
        Style::default().fg(if app.searching { CYAN } else { MUTED }),
    )];
    if app.query.is_empty() {
        query_spans.push(Span::styled(
            if app.searching {
                "type to filter…"
            } else {
                "filter projects"
            },
            Style::default().fg(MUTED),
        ));
    } else {
        query_spans.push(Span::styled(app.query.clone(), Style::default().fg(TEXT)));
    }
    if app.searching {
        query_spans.push(Span::styled("▌", Style::default().fg(CYAN)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(query_spans)).style(Style::default()),
        query_line[0],
    );

    let list_area = query_line[1];
    let mut hits = Vec::new();
    if app.loading {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} scanning projects…", spinner(app.tick())),
                Style::default().fg(MUTED),
            ))),
            list_area,
        );
        return hits;
    }
    if app.visible.is_empty() {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    " no projects found",
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    " press c to create one",
                    Style::default().fg(MUTED),
                )),
            ])),
            list_area,
        );
        return hits;
    }

    // Two lines per project.
    let visible_rows = (list_area.height / 2).max(1) as usize;
    let offset = app.selected.saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, &project_index) in app
        .visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
    {
        let project = &app.projects[project_index];
        let selected = row == app.selected && active;
        let base = if selected {
            Style::default()
                .fg(ON_ACCENT)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let marker = if project.git.is_repo {
            if project.git.is_dirty() { "●" } else { "◆" }
        } else {
            "◇"
        };
        let name_line = Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                base.fg(if selected {
                    ON_ACCENT
                } else {
                    branch_color(&project.git)
                }),
            ),
            Span::styled(project.name.clone(), base),
        ]);
        lines.push(name_line);
        let detail = Line::from(Span::styled(
            format!("   {}", project.git.badge()),
            if selected {
                Style::default().fg(ON_ACCENT).bg(ACCENT)
            } else {
                Style::default().fg(MUTED)
            },
        ));
        lines.push(detail);
        hits.push(RowHit {
            rect: Rect {
                x: list_area.x,
                y: list_area.y + ((row - offset) as u16) * 2,
                width: list_area.width,
                height: 2,
            },
            index: row,
        });
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default()),
        list_area,
    );
    hits
}

pub(crate) fn branch_color(git: &crate::project::GitSummary) -> Color {
    if !git.is_repo {
        MUTED
    } else if git.is_dirty() {
        YELLOW
    } else {
        GREEN
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::ui::test_support::*;

    #[test]
    fn empty_project_list_has_guidance() {
        let (_dir, mut app) = app();
        app.set_projects(vec![]);
        let (text, _) = render(&app);
        assert!(text.contains("no projects found"));
        assert!(text.contains("press c to create one"));
    }
}
