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

#[cfg(test)]
mod tests {
    use crate::config::Harness;
    use crate::tui::app::{ActionId, Modal, MultiAgentForm};
    use crate::tui::ui::test_support::*;

    #[test]
    fn multi_agent_modal_shows_clone_count_harness_and_controls() {
        let (_dir, mut app) = app();
        let mut form = MultiAgentForm::new("build".into(), Harness::Opencode);
        form.count = 3;
        app.modal = Some(Modal::MultiAgent(form));
        let (text, hits) = render(&app);

        assert!(text.contains("multi-agent clones"), "{text}");
        assert!(text.contains("Instance: build"), "{text}");
        assert!(text.contains("Clones:  3"), "{text}");
        assert!(text.contains("Harness: OpenCode"), "{text}");
        assert!(text.contains("Start:   yes"), "{text}");
        assert!(text.contains("commits cannot collide"), "{text}");
        for action in [
            ActionId::MultiAgentMore,
            ActionId::MultiAgentFewer,
            ActionId::HarnessPrev,
            ActionId::HarnessNext,
            ActionId::ToggleCloneStart,
            ActionId::Confirm,
            ActionId::Cancel,
        ] {
            assert!(
                hits.footer.iter().any(|hit| hit.action == action),
                "missing footer action {action:?} in:\n{text}"
            );
        }
    }

    #[test]
    fn multi_agent_modal_renders_the_no_start_choice_and_errors() {
        let (_dir, mut app) = app();
        let mut form = MultiAgentForm::new("build".into(), Harness::Shell);
        form.count = 1;
        form.start = false;
        form.error = Some("refusing to create more than 8 clones".into());
        app.modal = Some(Modal::MultiAgent(form));
        let (text, _) = render(&app);

        assert!(text.contains("Clones:  1"), "{text}");
        assert!(text.contains("Start:   no"), "{text}");
        assert!(text.contains("Harness: shell"), "{text}");
        assert!(
            text.contains("refusing to create more than 8 clones"),
            "{text}"
        );
    }
}
