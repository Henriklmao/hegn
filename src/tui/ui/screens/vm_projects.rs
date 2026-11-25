//! VM screen `1 Projects`: member list with primary and read-only markers.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::project::ProjectKind;
use crate::tui::app::{App, Focus, RowHit};

use super::super::text::truncate;
use super::super::theme::{ACCENT, CYAN, MUTED, ON_ACCENT, TEXT, YELLOW, tab_block};

pub(crate) fn render_vm_projects(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    number: usize,
) -> Vec<RowHit> {
    let active = app.focus == Focus::Projects;
    let block = tab_block(
        format!(" projects ({}) ", app.vm_members.len()),
        number,
        active,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = Vec::new();
    if app.vm_members.is_empty() {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    " no projects assigned",
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    " press a to assign one · f to fork the selected project",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return hits;
    }

    let visible_rows = inner.height as usize;
    let offset = app
        .vm_member_selected
        .saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, member) in app
        .vm_members
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
    {
        let selected = row == app.vm_member_selected && active;
        let base = if selected {
            Style::default()
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let muted = if selected {
            Style::default().fg(ON_ACCENT).bg(CYAN)
        } else {
            Style::default().fg(MUTED)
        };
        let kind = match member.kind {
            ProjectKind::External => "external",
            ProjectKind::Clone => "clone",
            ProjectKind::Worktree => "worktree",
        };
        let primary = app
            .active_vm_summary()
            .and_then(|summary| summary.primary.as_deref())
            == Some(member.name.as_str());
        let marker = if primary { "★" } else { "◇" };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(if primary { YELLOW } else { ACCENT })
                },
            ),
            Span::styled(truncate(&member.name, 26), base),
            Span::styled(format!("  {kind}"), muted),
            Span::styled(
                if member.read_only {
                    "  ro".to_string()
                } else {
                    String::new()
                },
                muted,
            ),
            Span::styled(
                if primary {
                    "  primary".to_string()
                } else {
                    String::new()
                },
                muted,
            ),
        ]));
        hits.push(RowHit {
            rect: Rect {
                x: inner.x,
                y: inner.y + (row - offset) as u16,
                width: inner.width,
                height: 1,
            },
            index: row,
        });
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default()),
        inner,
    );
    hits
}
