//! Git panel: branch, sync counters and working tree changes.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, Focus};

use super::super::theme::{MUTED, TEXT, YELLOW, tab_block};
use super::projects::branch_color;

pub(crate) fn render_git(frame: &mut Frame, app: &App, area: Rect, number: usize) {
    let active = app.focus == Focus::Git;
    let block = tab_block(" git ".to_string(), number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(project) = app.active_project() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no project",
                Style::default().fg(MUTED),
            ))),
            inner,
        );
        return;
    };
    if !project.git.is_repo {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    "not a git repository",
                    Style::default().fg(YELLOW),
                )),
                Line::from(Span::styled(
                    "checkpoints are unavailable",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return;
    }
    let lines = vec![
        Line::from(vec![
            Span::styled(" branch ", Style::default().fg(MUTED)),
            Span::styled(
                project.git.badge(),
                Style::default().fg(branch_color(&project.git)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" sync   ", Style::default().fg(MUTED)),
            Span::styled(
                format!("↑{} ↓{}", project.git.ahead, project.git.behind),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" change ", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "{} staged · {} modified · {} untracked",
                    project.git.staged, project.git.modified, project.git.untracked
                ),
                Style::default().fg(TEXT),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}
