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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;
    use crate::tui::app::Screen;
    use crate::tui::ui::test_support::*;
    use crate::vm::instance::InstanceSummary;
    use std::path::PathBuf;

    fn instance(app: &App, name: &str, state: &str, harness: Harness) -> InstanceSummary {
        InstanceSummary {
            name: name.into(),
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from(format!("/workspaces/{name}")),
            harness,
            ephemeral: false,
            source_instance: None,
            created_at: chrono::Utc::now(),
            state: state.into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.instances_dir.join(name).join("hegn.toml"),
            vcpus: 2,
            memory_mib: 2048,
            error: None,
        }
    }

    fn project_app() -> (tempfile::TempDir, App) {
        let (dir, mut app) = app();
        app.screen = Screen::Project;
        app.open_project = Some(PathBuf::from("/projects/demo"));
        (dir, app)
    }

    #[test]
    fn instance_rows_show_state_markers_and_harness_labels() {
        let (_dir, mut app) = project_app();
        app.instances = vec![
            instance(&app, "build", "running", Harness::Opencode),
            instance(&app, "paused", "paused", Harness::ClaudeCode),
            instance(&app, "broken", "failed", Harness::Shell),
            instance(&app, "idle", "stopped", Harness::Shell),
        ];
        app.instances[3].ephemeral = true;
        app.refresh_instance_filter();

        let (text, hits, buffer) = render_buffer(&app, 120, 42);
        assert!(text.contains("instances (4)"), "{text}");
        assert_eq!(hits.instances.len(), 4);
        assert!(text.contains("●"), "{text}");
        assert!(text.contains("○"), "{text}");
        assert!(text.contains("clone"), "{text}");
        assert!(text.contains("OpenCode"), "{text}");
        assert!(text.contains("Claude Code"), "{text}");

        for (row, color, marker) in [
            (0, GREEN, "●"),
            (1, YELLOW, "○"),
            (2, RED, "○"),
            (3, MUTED, "○"),
        ] {
            let hit = &hits.instances[row];
            let cell = &buffer[(hit.rect.x + 1, hit.rect.y)];
            assert_eq!(cell.symbol(), marker, "row {row} marker");
            assert_eq!(cell.fg, color, "row {row} state color");
        }
    }

    #[test]
    fn empty_instance_list_explains_how_to_start_one() {
        let (_dir, mut app) = project_app();
        app.instances.clear();
        app.refresh_instance_filter();
        let (text, hits) = render(&app);

        assert!(text.contains("no instances for demo"), "{text}");
        assert!(text.contains("Enter or n starts one"), "{text}");
        assert!(hits.instances.is_empty());
    }

    #[test]
    fn empty_instance_list_without_a_project_asks_for_one() {
        let (_dir, mut app) = app();
        app.set_projects(Vec::new());
        app.screen = Screen::Project;
        app.open_project = None;
        app.instances.clear();
        app.refresh_instance_filter();
        let (text, hits) = render(&app);

        assert!(text.contains("no project selected"), "{text}");
        assert!(text.contains("pick a project to scope it"), "{text}");
        assert!(hits.instances.is_empty());
    }

    #[test]
    fn instance_rows_scroll_to_the_selection_and_truncate_long_names() {
        let (_dir, mut app) = project_app();
        app.instances = (0..60)
            .map(|index| {
                instance(
                    &app,
                    &format!("{index:02}-agent-session-with-a-long-name"),
                    "stopped",
                    Harness::Shell,
                )
            })
            .collect();
        app.refresh_instance_filter();
        app.instance_selected = 59;

        let (text, hits) = render(&app);
        assert!(
            hits.instances.len() < 60,
            "only the visible window is drawn"
        );
        assert!(text.contains("agent-session-"), "{text}");
        assert!(text.contains('…'), "long names are ellipsized:\n{text}");
        assert!(
            !text.contains("00-agent-session"),
            "the scrolled-away first row must not be drawn:\n{text}"
        );
        assert!(
            text.contains("59-agent-session"),
            "the selected last row is kept visible:\n{text}"
        );
    }
}
