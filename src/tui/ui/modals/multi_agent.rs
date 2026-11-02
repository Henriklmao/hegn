//! Multi-agent clone prompt.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::tui::app::{ActionId, App, MultiAgentForm};

use super::super::ModalHits;
use super::super::screens::footer::FooterDef;
use super::super::theme::{CYAN, RED, centered};
use super::message::render_message;

pub(crate) fn render_multi_agent(
    frame: &mut Frame,
    app: &App,
    form: &MultiAgentForm,
    area: Rect,
) -> ModalHits {
    let body = format!(
        "Create temporary clones for parallel agents.\n\nInstance: {}\nClones:  {}\nHarness: {}\nStart:   {}\n\nEach clone gets its own workspace; commits cannot collide.",
        form.source,
        form.count,
        form.harness.label(),
        if form.start { "yes" } else { "no" }
    );
    let (footer, scroll) = render_message(
        frame,
        app,
        area,
        72,
        50,
        2,
        "multi-agent clones",
        &body,
        CYAN,
        form.scroll,
        &[
            FooterDef {
                key: "↑",
                label: "clones",
                action: ActionId::MultiAgentMore,
            },
            FooterDef {
                key: "↓",
                label: "clones",
                action: ActionId::MultiAgentFewer,
            },
            FooterDef {
                key: "←",
                label: "harness",
                action: ActionId::HarnessPrev,
            },
            FooterDef {
                key: "→",
                label: "harness",
                action: ActionId::HarnessNext,
            },
            FooterDef {
                key: "Space",
                label: "start",
                action: ActionId::ToggleCloneStart,
            },
            FooterDef {
                key: "Enter",
                label: "Create clones",
                action: ActionId::Confirm,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Cancel,
            },
        ],
    );
    if let Some(error) = &form.error {
        let rect = centered(area, 72, 50);
        frame.render_widget(
            Paragraph::new(Span::styled(format!(" {error}"), Style::default().fg(RED))),
            Rect {
                x: rect.x + 2,
                y: rect.y + rect.height.saturating_sub(4),
                width: rect.width.saturating_sub(4),
                height: 1,
            },
        );
    }
    ModalHits {
        footer,
        scroll,
        ..ModalHits::default()
    }
}
