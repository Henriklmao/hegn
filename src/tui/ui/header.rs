//! Top status bar: logo, open project, instance state and backend.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{App, InstanceState, Screen};

use super::theme::{ACCENT, BORDER, CYAN, GREEN, MUTED, ON_ACCENT, RED, YELLOW, spinner};

pub(crate) fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let project = (app.screen == Screen::Project)
        .then(|| app.active_project())
        .flatten();
    let block = if project.is_some() {
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
        Line::from(vec![
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
            Span::styled(
                format!(" {} ", project.display_path()),
                Style::default().fg(MUTED),
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

    let status = match app.instance.state {
        InstanceState::Running => ("● running".to_string(), GREEN),
        InstanceState::Starting => (format!("{} starting", spinner(app.tick())), YELLOW),
        InstanceState::Paused => ("Ⅱ paused".to_string(), YELLOW),
        InstanceState::Failed => ("✕ failed".to_string(), RED),
        InstanceState::Stopped => {
            if app.active_project().is_some() {
                ("○ no instance".to_string(), MUTED)
            } else {
                ("○ stopped".to_string(), MUTED)
            }
        }
    };
    let right_text = format!("{}  {} ", status.0, app.instance.backend);
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
