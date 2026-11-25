//! Top status bar: logo, open project, VM state and backend.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{App, Screen, VmUiState};

use super::theme::{ACCENT, BORDER, CYAN, GREEN, MUTED, ON_ACCENT, RED, YELLOW, spinner};

pub(crate) fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let project = (app.screen == Screen::Project)
        .then(|| app.active_project())
        .flatten();
    let vm = match app.screen {
        Screen::Vm => app.vm_open.clone(),
        Screen::Project => project
            .and_then(|project| app.project_vm_name(&project.path))
            .map(str::to_string),
        Screen::Dashboard => None,
    };
    let block = if project.is_some() || vm.is_some() {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default())
    } else {
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default())
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let left = if let Some(project) = project {
        let mut spans = vec![
            Span::styled(
                " hegn ",
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ▸ ", Style::default().fg(ACCENT)),
            Span::styled(
                format!(" {} ", project.name),
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(vm) = &vm {
            spans.push(Span::styled(
                format!("· {vm} "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled(
            format!(" {} ", project.display_path()),
            Style::default().fg(MUTED),
        ));
        Line::from(spans)
    } else if let Some(vm) = &vm {
        Line::from(vec![
            Span::styled(
                " hegn ",
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ▸ VM ", Style::default().fg(ACCENT)),
            Span::styled(
                format!(" {vm} "),
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " hegn ",
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "[dashboard]",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(left).style(Style::default()), inner);

    let status = match app.vm.state {
        VmUiState::Running => ("● running".to_string(), GREEN),
        VmUiState::Starting => (format!("{} starting", spinner(app.tick())), YELLOW),
        VmUiState::Paused => ("Ⅱ paused".to_string(), YELLOW),
        VmUiState::Failed => ("✕ failed".to_string(), RED),
        VmUiState::Stopped => {
            if app.active_project().is_some() || app.vm_open.is_some() {
                ("○ no VM".to_string(), MUTED)
            } else {
                ("○ stopped".to_string(), MUTED)
            }
        }
    };
    let right_text = format!("{}  {} ", status.0, app.vm.backend);
    let width = right_text.width() as u16;
    if inner.width > width + 10 {
        let right = Rect {
            x: inner.x + inner.width.saturating_sub(width),
            y: inner.y,
            width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                right_text,
                Style::default().fg(status.1),
            )))
            .alignment(Alignment::Right),
            right,
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::ui::test_support::*;

    #[test]
    fn project_header_shows_project_and_vm() {
        let (_dir, mut app) = app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.projects[0].path = project.clone();
        app.vms = vec![crate::vm::registry::VmSummary {
            name: "rust".into(),
            harness: crate::config::Harness::Shell,
            primary: Some("demo".into()),
            members: 1,
            project: project.clone(),
            workspace: project.clone(),
            created_at: chrono::Utc::now(),
            state: "running".into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.vms_dir.join("rust/vm.toml"),
            vcpus: 2,
            memory_mib: 2048,
            error: None,
        }];
        crate::tui::ui::test_support::assign_project(&mut app, "demo", "rust");
        app.screen = crate::tui::app::Screen::Project;
        app.open_project = Some(project);
        let (text, _) = render(&app);
        assert!(text.contains("demo · rust"), "{text}");
    }

    #[test]
    fn header_drops_the_tagline_on_the_dashboard() {
        let (_dir, app) = app();
        let (text, _) = render(&app);
        assert!(text.contains("hegn"), "{text}");
        assert!(text.contains("[dashboard]"), "{text}");
        assert!(
            !text.contains("Cloud Hypervisor instances for coding agents"),
            "the dashboard header no longer repeats the tagline:\n{text}"
        );
    }
}
