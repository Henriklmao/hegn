//! Generic scrollable message modal shared by confirmations and errors.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};

use crate::tui::app::{App, FooterHit, ScrollHit};

use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::text::wrap_text;
use super::super::theme::{TEXT, centered};
use super::super::widgets::{render_scrolled, scroll_pane};

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_message(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    width_pct: u16,
    height_pct: u16,
    hint_rows: u16,
    title: &str,
    body: &str,
    color: Color,
    offset: usize,
    buttons: &[FooterDef],
) -> (Vec<FooterHit>, Option<ScrollHit>) {
    let rect = centered(area, width_pct, height_pct);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(hint_rows.max(1))])
        .split(inner);
    let width = chunks[0].width as usize;
    let lines: Vec<Line<'static>> = body
        .lines()
        .flat_map(|line| wrap_text(line, width))
        .map(|line| Line::from(Span::styled(line, Style::default().fg(TEXT))))
        .collect();
    let pane = scroll_pane(chunks[0], lines.len(), offset);
    render_scrolled(frame, app, &pane, lines);
    let footer = hint_buttons(frame, app, chunks[1], buttons);
    (footer, pane.hit)
}
