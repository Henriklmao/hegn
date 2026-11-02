//! Instances panel: state markers, harness labels and clickable rows.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, Focus, RowHit};

use super::super::text::truncate;
use super::super::theme::{ACCENT, CYAN, GREEN, MUTED, ON_ACCENT, RED, TEXT, YELLOW, tab_block};

pub(crate) fn render_instances(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    number: usize,
) -> Vec<RowHit> {
    let active = app.focus == Focus::Instances;
    let title = format!(" instances ({}) ", app.instance_visible.len());
    let block = tab_block(title, number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = Vec::new();
    if app.instance_visible.is_empty() {
        let (headline, hint) = match app.selected_project() {
            Some(project) => (
                format!(" ▾ no instances for {}", truncate(&project.name, 18)),
                "   Enter or n starts one",
            ),
            None => (
                " ▾ no project selected".to_string(),
                "   pick a project to scope it",
            ),
        };
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(headline, Style::default().fg(MUTED))),
                Line::from(Span::styled(hint, Style::default().fg(MUTED))),
            ])),
            inner,
        );
        return hits;
    }

    let visible_rows = inner.height as usize;
    let offset = app
        .instance_selected
        .saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, &index) in app
        .instance_visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
    {
        let instance = &app.instances[index];
        let selected = row == app.instance_selected && active;
        let base = if selected {
            Style::default()
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let state_color = match instance.state.as_str() {
            "running" => GREEN,
            "paused" => YELLOW,
            "failed" => RED,
            _ => MUTED,
        };
        let marker = if instance.state == "running" {
            "●"
        } else {
            "○"
        };
        let badge = if instance.ephemeral { " clone" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(state_color)
                },
            ),
            Span::styled(truncate(&instance.name, 18), base),
            Span::styled(
                badge,
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(ACCENT)
                },
            ),
            Span::styled(
                format!(" {}", instance.harness.label()),
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(MUTED)
                },
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
