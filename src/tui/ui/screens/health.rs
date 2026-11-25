//! Health panel: versions, pinned assets, VM age, newest snapshot and apt state.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, Focus};

use super::super::theme::{GREEN, MUTED, TEXT, YELLOW, tab_block};

pub(crate) fn render_health(frame: &mut Frame, app: &App, area: Rect, number: usize) {
    let active = app.focus == Focus::Health;
    let block = tab_block(" health ".to_string(), number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let project = app.active_project();
    let vm = app.active_vm_summary();
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" hegn      ", Style::default().fg(MUTED)),
        Span::styled(env!("CARGO_PKG_VERSION"), Style::default().fg(TEXT)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" images    ", Style::default().fg(MUTED)),
        Span::styled(
            if app.config.vm_assets_ready() {
                "pinned · ready"
            } else {
                "missing · run doctor"
            },
            Style::default().fg(if app.config.vm_assets_ready() {
                GREEN
            } else {
                YELLOW
            }),
        ),
    ]));
    let age = vm
        .map(|vm| {
            let created = vm.created_at.format("%Y-%m-%d").to_string();
            if vm.state == "running" {
                format!("{created} · running")
            } else {
                created
            }
        })
        .or_else(|| {
            project.and_then(|project| {
                project
                    .last_opened
                    .map(|t| t.format("%Y-%m-%d").to_string())
            })
        })
        .unwrap_or_else(|| "—".into());
    lines.push(Line::from(vec![
        Span::styled(" age       ", Style::default().fg(MUTED)),
        Span::styled(age, Style::default().fg(TEXT)),
    ]));
    let backup = vm
        .and_then(|vm| app.newest_snapshot_for_vm(&vm.name))
        .map(|snapshot| {
            format!(
                "{} · {}",
                snapshot.created_at.format("%Y-%m-%d %H:%M"),
                snapshot.size_label()
            )
        })
        .unwrap_or_else(|| "none".into());
    lines.push(Line::from(vec![
        Span::styled(" snapshot  ", Style::default().fg(MUTED)),
        Span::styled(backup, Style::default().fg(TEXT)),
    ]));
    let apt = app
        .health
        .as_ref()
        .map(|health| health.label())
        .unwrap_or_else(|| "not checked".into());
    lines.push(Line::from(vec![
        Span::styled(" apt       ", Style::default().fg(MUTED)),
        Span::styled(apt, Style::default().fg(TEXT)),
    ]));
    if app.screen == crate::tui::app::Screen::Project {
        let template = app
            .project_meta
            .template
            .clone()
            .unwrap_or_else(|| "—".into());
        lines.push(Line::from(vec![
            Span::styled(" template  ", Style::default().fg(MUTED)),
            Span::styled(template, Style::default().fg(TEXT)),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}
