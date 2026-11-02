//! Transient toast and progress notifications anchored to the screen corner.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{App, ToastLevel};
use crate::vm::images::DownloadProgress;

use super::text::truncate;
use super::theme::{ACCENT, GREEN, ON_ACCENT, RED, YELLOW};
use super::widgets::render_progress_bar;

pub(crate) fn render_toast(frame: &mut Frame, app: &App, area: Rect) {
    let toast = render_toast_notification(frame, app, area);
    let mut bottom = toast.map_or(area.y + area.height, |rect| rect.y);
    if let Some(progress) = &app.provision
        && let Some(rect) = render_provision_progress(frame, progress, app.tick(), area, bottom)
    {
        bottom = rect.y;
    }
    if let Some(progress) = &app.download {
        render_download_progress(frame, progress, app.tick(), area, bottom);
    }
}

/// Progress notification for the first boot's package installation.
pub(crate) fn render_provision_progress(
    frame: &mut Frame,
    progress: &crate::agent::provision::ProvisionProgress,
    tick: u64,
    area: Rect,
    bottom: u16,
) -> Option<Rect> {
    let height = 4;
    if bottom < area.y + height || area.width < 28 {
        return None;
    }
    let label = format!(
        " setting up {} · {} · {} ",
        progress.harness_label(),
        progress.stage_label(),
        progress.eta_label()
    );
    let width = (label.width() as u16 + 4)
        .min(area.width.saturating_sub(4))
        .max(28);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width + 2),
        y: bottom - height,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return Some(rect);
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&label, inner.width as usize),
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rows[0],
    );
    render_progress_bar(frame, rows[1], Some(progress.fraction()), tick);
    Some(rect)
}

/// Draw the transient toast, returning its rectangle so a download
/// notification can stack above it.
pub(crate) fn render_toast_notification(frame: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    let toast = app.toast.as_ref()?;
    let color = match toast.level {
        ToastLevel::Info => GREEN,
        ToastLevel::Warn => YELLOW,
        ToastLevel::Error => RED,
    };
    let width = (toast.text.width() as u16 + 4)
        .min(area.width.saturating_sub(4))
        .max(20);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width + 2),
        y: area.y + area.height.saturating_sub(3),
        width,
        height: 3,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                " {} ",
                truncate(&toast.text, width.saturating_sub(2) as usize)
            ),
            Style::default()
                .fg(ON_ACCENT)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .style(Style::default()),
        ),
        rect,
    );
    Some(rect)
}

/// Bottom-right notification for an in-flight VM image download: the asset
/// name, its completion and a bar that fills as bytes arrive.
pub(crate) fn render_download_progress(
    frame: &mut Frame,
    progress: &DownloadProgress,
    tick: u64,
    area: Rect,
    bottom: u16,
) {
    let height = 4;
    if bottom < area.y + height || area.width < 24 {
        return;
    }
    let fraction = progress.fraction();
    let detail = match fraction {
        Some(fraction) => format!("{}%", (fraction * 100.0).floor() as u16),
        None => format!("{} MiB", progress.received / (1024 * 1024)),
    };
    let label = format!(" downloading {} · {detail} ", progress.asset);
    let width = (label.width() as u16 + 4)
        .min(area.width.saturating_sub(4))
        .max(24);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width + 2),
        y: bottom - height,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rows[0],
    );
    render_progress_bar(frame, rows[1], fraction, tick);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::ui::test_support::*;

    #[test]
    fn image_download_shows_a_progress_notification() {
        let (_dir, mut app) = app();
        app.set_download_progress(DownloadProgress {
            asset: "rootfs",
            received: 512 * 1024 * 1024,
            total: Some(1024 * 1024 * 1024),
        });
        let (text, _) = render(&app);
        let rows: Vec<&str> = text.lines().collect();
        let label = rows
            .iter()
            .position(|row| row.contains("downloading rootfs"))
            .expect("the notification names the asset");
        assert!(rows[label].contains("50%"), "{text}");
        assert!(
            label > rows.len() / 2,
            "the notification is anchored at the bottom:\n{text}"
        );
        assert!(
            rows[label + 1].contains('█'),
            "the bar fills proportionally:\n{text}"
        );
        assert!(
            rows[label + 1].contains('░'),
            "the unfilled track stays visible:\n{text}"
        );
    }

    #[test]
    fn provisioning_progress_names_the_harness_and_stage() {
        let (_dir, mut app) = app();
        let plan =
            crate::agent::provision::ProvisionPlan::new(crate::config::Harness::Opencode, &[])
                .unwrap();
        let mut progress = crate::agent::provision::ProvisionProgress::new(&plan);
        progress.advance(crate::agent::provision::Stage::Packages);
        app.set_provision_progress(progress);
        let (text, _) = render(&app);
        assert!(text.contains("setting up OpenCode"), "{text}");
        assert!(text.contains("installing system packages"), "{text}");
        assert!(text.contains("left"), "{text}");
        assert!(text.contains('█') && text.contains('░'), "{text}");
    }

    #[test]
    fn image_download_progress_clears_when_the_worker_reports_back() {
        let (_dir, mut app) = app();
        app.set_download_progress(DownloadProgress {
            asset: "kernel",
            received: 1024,
            total: None,
        });
        app.clear_download_progress();
        let (text, _) = render(&app);
        assert!(!text.contains("downloading kernel"), "{text}");
    }
}
