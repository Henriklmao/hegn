//! Shared palette, panel decoration, and responsive layout helpers.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

pub(crate) const ON_ACCENT: Color = Color::Black;
pub(crate) const BORDER: Color = Color::DarkGray;
pub(crate) const TEXT: Color = Color::Reset;
pub(crate) const MUTED: Color = Color::DarkGray;
pub(crate) const ACCENT: Color = Color::Cyan;
pub(crate) const CYAN: Color = Color::LightCyan;
pub(crate) const GREEN: Color = Color::Green;
pub(crate) const YELLOW: Color = Color::Yellow;
pub(crate) const RED: Color = Color::Red;

pub(crate) fn panel_block(title: String, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if active { ACCENT } else { BORDER }))
        .title_top(Span::styled(
            title,
            Style::default()
                .fg(if active { ACCENT } else { MUTED })
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default())
}

/// A focusable sidebar tab: like [`panel_block`], but with a coloured
/// `[n]` shortcut badge in the top-right corner.
pub(crate) fn tab_block(title: String, number: usize, active: bool) -> Block<'static> {
    panel_block(title, active).title_top(
        Line::from(Span::styled(
            format!("[{number}] "),
            Style::default()
                .fg(if active { GREEN } else { YELLOW })
                .add_modifier(Modifier::BOLD),
        ))
        .right_aligned(),
    )
}

pub(crate) fn spinner(tick: u64) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    FRAMES[(tick as usize / 2) % FRAMES.len()]
}

/// Preferred minimum size for a modal. Narrower terminals use the full width.
pub(crate) const MODAL_MIN_WIDTH: u16 = 48;
pub(crate) const MODAL_MIN_HEIGHT: u16 = 10;

/// Responsive modal geometry: honour the preferred percentage, but grow to the
/// available space on small terminals and never exceed the screen.
pub(crate) fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let margin_x = if area.width >= 60 { 2 } else { 0 };
    let margin_y = if area.height >= 16 { 1 } else { 0 };
    let max_width = area.width.saturating_sub(margin_x * 2).max(1);
    let max_height = area.height.saturating_sub(margin_y * 2).max(1);
    let preferred_width = (area.width as u32 * width_pct as u32 / 100) as u16;
    let width = if max_width <= MODAL_MIN_WIDTH {
        max_width
    } else {
        preferred_width.max(MODAL_MIN_WIDTH).min(max_width)
    };
    let preferred_height = (area.height as u32 * height_pct as u32 / 100) as u16;
    let height = preferred_height
        .max(MODAL_MIN_HEIGHT.min(max_height))
        .min(max_height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}
