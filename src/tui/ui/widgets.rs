//! Scrollable modal bodies and the progress bar widget.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, ScrollHit};

use super::text::fit_line;
use super::theme::{ACCENT, BORDER, TEXT};

/// A scrollable modal body: the content area (shrunk by the scrollbar column)
/// and the track geometry returned to the application for hover and drag.
pub(crate) struct ScrollPane {
    pub(crate) content: Rect,
    pub(crate) hit: Option<ScrollHit>,
    pub(crate) total: usize,
}

pub(crate) fn scroll_pane(area: Rect, total: usize, offset: usize) -> ScrollPane {
    let page = area.height as usize;
    let max = total.saturating_sub(page);
    if area.width < 6 || area.height == 0 || max == 0 {
        return ScrollPane {
            content: area,
            hit: None,
            total,
        };
    }
    let content = Rect {
        width: area.width - 1,
        ..area
    };
    ScrollPane {
        content,
        hit: Some(ScrollHit {
            rect: Rect {
                x: area.x + content.width,
                y: area.y,
                width: 1,
                height: area.height,
            },
            max,
            page,
            offset: offset.min(max),
        }),
        total,
    }
}

/// Draw the track and thumb. The thumb inverts under the pointer or while it
/// is dragged, like every other mouse-interactive element in the TUI.
pub(crate) fn render_scrollbar(frame: &mut Frame, app: &App, pane: &ScrollPane) {
    let Some(hit) = pane.hit else { return };
    let track = hit.rect.height as usize;
    if track == 0 {
        return;
    }
    let thumb = ((hit.page * track) / pane.total.max(1)).clamp(1, track);
    let position = hit
        .offset
        .saturating_mul(track - thumb)
        .checked_div(hit.max)
        .unwrap_or(0);
    let hovered = app.scroll_hover || app.scroll_dragging;
    for row in 0..track {
        let in_thumb = row >= position && row < position + thumb;
        let (symbol, style) = if in_thumb {
            if hovered {
                // Invert the accent instead of using the button foreground: the
                // thumb is a full block, so a dark foreground paints the whole
                // column black and the accent background never shows. The
                // terminal's default text color keeps the bar visible on the
                // track and still reads as the accent inverted.
                (
                    "█",
                    Style::default()
                        .fg(TEXT)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("█", Style::default().fg(ACCENT))
            }
        } else {
            ("│", Style::default().fg(BORDER))
        };
        frame.render_widget(
            Paragraph::new(symbol).style(style),
            Rect {
                x: hit.rect.x,
                y: hit.rect.y + row as u16,
                width: 1,
                height: 1,
            },
        );
    }
}

/// Render a scrollable body without wrapping, so one logical line is one row
/// and the scrollbar offset stays exact. Overlong lines are ellipsized.
pub(crate) fn render_scrolled(
    frame: &mut Frame,
    app: &App,
    pane: &ScrollPane,
    lines: Vec<Line<'static>>,
) {
    let width = pane.content.width as usize;
    let lines: Vec<Line<'static>> = lines
        .into_iter()
        .map(|line| fit_line(line, width))
        .collect();
    let offset = pane.hit.map_or(0, |hit| hit.offset);
    frame.render_widget(
        Paragraph::new(lines).scroll((offset.min(u16::MAX as usize) as u16, 0)),
        pane.content,
    );
    render_scrollbar(frame, app, pane);
}

/// Fill `area` with a bar: exact when `fraction` is known, a sliding segment
/// while the server has not advertised a length.
pub(crate) fn render_progress_bar(frame: &mut Frame, area: Rect, fraction: Option<f64>, tick: u64) {
    let width = area.width as usize;
    if width == 0 {
        return;
    }
    let (start, end) = match fraction {
        Some(fraction) => (
            0,
            (width as f64 * fraction.clamp(0.0, 1.0)).round() as usize,
        ),
        None => {
            // Unknown length: slide a segment across the empty track.
            let segment = (width / 4).max(1);
            let offset = (tick as usize / 2) % (width + segment);
            (offset.saturating_sub(segment).min(width), offset.min(width))
        }
    };
    let end = end.min(width).max(start);
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled("░".repeat(start), Style::default().fg(BORDER)));
    }
    if end > start {
        spans.push(Span::styled(
            "█".repeat(end - start),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    if end < width {
        spans.push(Span::styled(
            "░".repeat(width - end),
            Style::default().fg(BORDER),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{HelpState, Modal};
    use crate::tui::ui::test_support::*;
    use crate::tui::ui::theme::ON_ACCENT;
    use ratatui::style::Modifier;

    #[test]
    fn hovered_scrollbar_thumb_inverts_without_going_black() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Help(HelpState::default()));

        let (_, hits, buffer) = render_buffer(&app, 60, 24);
        let scroll = hits.modal_scroll.expect("the help list overflows");
        let idle = &buffer[(scroll.rect.x, scroll.rect.y)];
        assert_eq!(idle.symbol(), "█");
        assert_eq!(idle.fg, ACCENT, "the idle thumb is drawn in the accent");

        app.scroll_hover = true;
        let (_, hits, buffer) = render_buffer(&app, 60, 24);
        let scroll = hits.modal_scroll.expect("the help list overflows");
        let hovered = &buffer[(scroll.rect.x, scroll.rect.y)];
        assert_eq!(hovered.symbol(), "█");
        assert_eq!(
            hovered.fg, TEXT,
            "the accent is inverted into the text color"
        );
        assert_eq!(hovered.bg, ACCENT);
        assert_ne!(
            hovered.fg, ON_ACCENT,
            "a black block glyph would paint the track solid black"
        );
        assert!(
            hovered.modifier.contains(Modifier::BOLD),
            "the hovered thumb stays emphasized"
        );

        app.scroll_dragging = true;
        let (_, hits, buffer) = render_buffer(&app, 60, 24);
        let scroll = hits.modal_scroll.expect("the help list overflows");
        let dragged = &buffer[(scroll.rect.x, scroll.rect.y)];
        assert_eq!(dragged.fg, TEXT, "dragging keeps the inverted style");
    }
}
