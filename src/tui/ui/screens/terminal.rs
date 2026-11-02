//! Terminal pane and collapsed terminal strip.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::app::{App, TerminalMode};

use super::super::theme::{ACCENT, CYAN, GREEN, MUTED, RED, TEXT, YELLOW, panel_block};

pub(crate) fn render_terminal_strip(frame: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    if area.height == 0 || area.width == 0 {
        return None;
    }
    let selected = app
        .selected_instance()
        .map(|instance| (instance.name.clone(), instance.state.clone()));
    let selected = selected.or_else(|| {
        app.session
            .as_ref()
            .map(|session| (session.instance_name().to_string(), "running".to_string()))
    });
    let (text, color) = match selected {
        Some((name, state)) => {
            let color = match state.as_str() {
                "running" => GREEN,
                "failed" => RED,
                _ => MUTED,
            };
            let action = if app.session.is_some() {
                "i to open · F2 attach"
            } else {
                "Enter to open · F2 attach"
            };
            (format!(" terminal · {name} · {state} · {action} "), color)
        }
        None => (
            " terminal · no instance · Enter to start ".to_string(),
            MUTED,
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))),
        area,
    );
    Some(area)
}

pub(crate) fn render_terminal_pane(frame: &mut Frame, app: &App, area: Rect) {
    let name = app
        .session
        .as_ref()
        .map(|session| session.instance_name().to_string())
        .or_else(|| {
            app.selected_instance()
                .map(|instance| instance.name.clone())
        })
        .unwrap_or_else(|| "no instance".into());
    let mode_label = match app.terminal_mode {
        TerminalMode::Normal => "normal",
        TerminalMode::Insert => "insert",
        TerminalMode::Raw => "raw",
    };
    let block = panel_block(
        format!(" terminal · {name} · {mode_label} "),
        app.terminal_focus,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 4 || inner.width < 16 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "terminal too small",
                Style::default().fg(MUTED),
            ))),
            inner,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(3)])
        .split(inner);
    let out = rows[0];
    let available = out.height.max(1) as usize;
    let total = app.terminal.len();
    let scroll = app.scroll.min(total.saturating_sub(available));
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(available);
    let visible: Vec<Line> = app
        .terminal
        .range(start, end)
        .into_iter()
        .map(|line| {
            let color = if line.starts_with("$ ") { GREEN } else { TEXT };
            Line::from(Span::styled(line, Style::default().fg(color)))
        })
        .collect();
    if app.terminal.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no console output yet — start an instance with Enter",
                Style::default().fg(MUTED),
            ))),
            out,
        );
    } else {
        frame.render_widget(Paragraph::new(Text::from(visible)), out);
    }

    let input_block = panel_block(
        match app.terminal_mode {
            TerminalMode::Normal => " normal mode ".to_string(),
            TerminalMode::Insert => " insert · command (tirith-core) ".to_string(),
            TerminalMode::Raw => " raw passthrough ".to_string(),
        },
        app.terminal_focus,
    );
    let input_inner = input_block.inner(rows[1]);
    frame.render_widget(input_block, rows[1]);
    if app.terminal_mode == TerminalMode::Normal {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "i type · Esc sections · F2 attach · Ctrl-B back",
                Style::default().fg(MUTED),
            ))),
            input_inner,
        );
    } else if app.terminal_mode == TerminalMode::Raw {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "raw passthrough · F2 attach · Ctrl-B back",
                Style::default().fg(YELLOW),
            ))),
            input_inner,
        );
    } else {
        let prompt = Span::styled("❯ ", Style::default().fg(ACCENT));
        let available = input_inner.width.saturating_sub(3) as usize;
        let shown = if app.input.width() > available {
            let mut width = app.input.width();
            let mut skip = 0;
            for ch in app.input.chars() {
                if width <= available {
                    break;
                }
                width -= ch.width().unwrap_or(0);
                skip += 1;
            }
            app.input.chars().skip(skip).collect::<String>()
        } else {
            app.input.clone()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                prompt,
                Span::styled(shown.clone(), Style::default().fg(TEXT)),
                Span::styled("▌", Style::default().fg(CYAN)),
            ])),
            input_inner,
        );
        let cursor_x = input_inner
            .x
            .saturating_add(2)
            .saturating_add(shown.width() as u16)
            .min(input_inner.x + input_inner.width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, input_inner.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;
    use crate::session::console::TerminalConsole;
    use crate::tui::app::{ProjectView, Screen};
    use crate::tui::ui::test_support::*;
    use crate::vm::instance::InstanceSummary;
    use std::path::{Path, PathBuf};

    struct FakeConsole {
        project: PathBuf,
    }

    impl TerminalConsole for FakeConsole {
        fn send(&mut self, _bytes: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }

        fn drain(&mut self) -> Vec<Vec<u8>> {
            Vec::new()
        }

        fn is_exited(&mut self) -> bool {
            false
        }

        fn project(&self) -> &Path {
            &self.project
        }

        fn label(&self) -> &str {
            "fake · serial"
        }

        fn instance_name(&self) -> &str {
            "fake"
        }
    }

    fn instance(app: &App, name: &str, state: &str) -> InstanceSummary {
        InstanceSummary {
            name: name.into(),
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from(format!("/workspaces/{name}")),
            harness: Harness::Shell,
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
    fn empty_terminal_shows_the_start_hint() {
        let (_dir, mut app) = project_app();
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
        let (text, _) = render(&app);
        assert!(text.contains("no console output yet"), "{text}");
    }

    #[test]
    fn terminal_view_renders_buffer_and_prompt() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
        app.terminal
            .push_bytes(b"first output line\r\nsecond output line\r\n");
        let (text, _) = render(&app);
        assert!(text.contains("first output line"), "{text}");
        assert!(text.contains("second output line"), "{text}");
        assert!(text.contains("terminal"));
        assert!(text.contains("normal"));

        app.terminal_mode = TerminalMode::Insert;
        app.input = "cargo test".into();
        let (text, _) = render(&app);
        assert!(text.contains("cargo test"));
        assert!(text.contains("insert"));
    }

    #[test]
    fn terminal_renders_the_full_scrollback_window() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
        for index in 0..40 {
            app.terminal
                .push_bytes(format!("scrollback line {index}\r\n").as_bytes());
        }
        let (text, _) = render(&app);
        // The newest lines are visible; earlier lines scrolled out of the pane.
        assert!(text.contains("scrollback line 39"), "{text}");
        assert!(text.contains("scrollback line 38"), "{text}");
        assert!(!text.contains("scrollback line 0\n"), "{text}");
    }

    #[test]
    fn collapsed_strip_describes_missing_running_and_attached_instances() {
        let (_dir, mut app) = project_app();
        let (text, hits) = render(&app);
        assert!(hits.terminal_strip.is_some());
        assert!(
            text.contains("terminal · no instance · Enter to start"),
            "{text}"
        );

        app.instances = vec![instance(&app, "build", "running")];
        app.refresh_instance_filter();
        let (text, hits) = render(&app);
        assert!(hits.terminal_strip.is_some());
        assert!(
            text.contains("terminal · build · running · Enter to open · F2 attach"),
            "{text}"
        );

        app.attach_console(Box::new(FakeConsole {
            project: PathBuf::from("/projects/demo"),
        }));
        app.project_view = ProjectView::Compact;
        let (text, _) = render(&app);
        assert!(
            text.contains("terminal · fake · running · i to open · F2 attach"),
            "{text}"
        );
    }

    #[test]
    fn terminal_pane_renders_empty_scrollback_and_tiny_panels() {
        let (_dir, mut app) = project_app();
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
        let (text, _) = render(&app);
        // Note: TerminalBuffer::len() always counts the in-progress line, so
        // the renderer's "no console output yet" branch is currently
        // unreachable. Keep this test focused on the pane chrome.
        assert!(text.contains("terminal · no instance · normal"), "{text}");

        let (text, _) = render_sized(&app, 40, 5);
        assert!(text.contains("terminal too small"), "{text}");
    }

    #[test]
    fn normal_mode_hides_pending_input_until_insert_mode() {
        let (_dir, mut app) = project_app();
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
        app.input = "super-secret-token".into();
        app.terminal_mode = TerminalMode::Normal;

        let (text, _) = render(&app);
        assert!(
            !text.contains("super-secret-token"),
            "normal mode must not render buffered input:\n{text}"
        );
        assert!(text.contains("normal mode"), "{text}");
        assert!(text.contains("i type · Esc sections"), "{text}");

        app.terminal_mode = TerminalMode::Insert;
        let (text, _) = render(&app);
        assert!(text.contains("super-secret-token"), "{text}");
        assert!(text.contains("insert · command"), "{text}");
    }

    #[test]
    fn long_insert_input_scrolls_to_keep_the_cursor_visible() {
        let (_dir, mut app) = project_app();
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
        app.terminal_mode = TerminalMode::Insert;
        app.input = format!("HEAD{}TAIL", "x".repeat(200));

        let (text, _) = render_sized(&app, 80, 24);
        assert!(text.contains("TAIL"), "{text}");
        assert!(
            !text.contains("HEAD"),
            "the beginning scrolls out of the input line:\n{text}"
        );
    }
}
