//! Snapshots panel: restore and delete targets scoped to a project.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, Focus, RowHit, Screen};

use super::super::text::truncate;
use super::super::theme::{CYAN, MUTED, ON_ACCENT, TEXT, tab_block};

pub(crate) fn render_snapshots(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    number: usize,
) -> Vec<RowHit> {
    let active = app.focus == Focus::Snapshots;
    let indices: Vec<usize> = if app.screen == Screen::Project {
        app.project_snapshots.clone()
    } else {
        (0..app.snapshots.len()).collect()
    };
    let selected_index = if app.screen == Screen::Project {
        app.project_snapshot_selected
    } else {
        app.selected_snapshot
    };
    let title = format!(" snapshots ({}) ", indices.len());
    let block = tab_block(title, number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = Vec::new();
    if indices.is_empty() {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(" ▾ no snapshots", Style::default().fg(MUTED))),
                Line::from(Span::styled(
                    "   hegn snapshot create",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return hits;
    }

    let visible_rows = inner.height as usize;
    let offset = selected_index.saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, &index) in indices.iter().enumerate().skip(offset).take(visible_rows) {
        let Some(snapshot) = app.snapshots.get(index) else {
            continue;
        };
        let selected = row == selected_index && active;
        let style = if selected {
            Style::default()
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let prefix = if row == offset { " ▾ " } else { "   " };
        lines.push(Line::from(vec![
            Span::styled(prefix, style.fg(if selected { ON_ACCENT } else { CYAN })),
            Span::styled(truncate(&snapshot.name, 20), style),
            Span::styled(
                format!("  {}", snapshot.size_label()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::ui::test_support::*;
    use std::path::PathBuf;

    #[test]
    fn snapshots_render_when_present() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.open_project = Some(PathBuf::from("/projects/demo"));
        app.snapshots = vec![crate::session::snapshot::SnapshotMeta {
            name: "before-refactor".into(),
            created_at: chrono::Utc::now(),
            vm_state: "Paused".into(),
            dir: PathBuf::from("/snap/demo"),
            instance: None,
            project: Some(PathBuf::from("/projects/demo")),
            size_bytes: 1024 * 1024,
            note: None,
        }];
        app.refresh_project_snapshots();
        let (text, hits) = render(&app);
        assert!(text.contains("before-refactor"));
        assert!(text.contains("1.0 MiB"));
        assert_eq!(hits.snapshots.len(), 1);
    }
}
