//! Fork-workspace modal: clone (isolated) or nested worktree.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::project::ForkMode;
use crate::tui::app::{ActionId, App, ForkWorkspaceForm};

use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::theme::{ACCENT, CYAN, GREEN, MUTED, ON_ACCENT, RED, TEXT, YELLOW, centered};

pub(crate) fn render_fork_workspace(
    frame: &mut Frame,
    app: &App,
    form: &ForkWorkspaceForm,
    area: Rect,
) -> Vec<crate::tui::app::FooterHit> {
    let rect = centered(area, 70, 58);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " fork workspace ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" parent  ", Style::default().fg(MUTED)),
            Span::styled(form.parent.clone(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" vm      ", Style::default().fg(MUTED)),
            Span::styled(form.vm.clone(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" name    ", Style::default().fg(MUTED)),
            Span::styled(form.name.clone(), Style::default().fg(TEXT)),
            Span::styled("▌", Style::default().fg(CYAN)),
        ]),
        Line::default(),
        Line::from(vec![
            Span::styled(" mode    ", Style::default().fg(MUTED)),
            Span::styled(
                "clone",
                if form.mode == ForkMode::Clone {
                    Style::default()
                        .fg(ON_ACCENT)
                        .bg(CYAN)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                },
            ),
            Span::styled("  ", Style::default()),
            Span::styled(
                "worktree",
                if form.mode == ForkMode::Worktree {
                    Style::default()
                        .fg(ON_ACCENT)
                        .bg(CYAN)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                },
            ),
            Span::styled("   Tab toggles", Style::default().fg(MUTED)),
        ]),
        Line::default(),
    ];
    match form.mode {
        ForkMode::Clone => {
            lines.push(Line::from(Span::styled(
                " clone: isolated git state under <vms_dir>/<vm>/workspaces/<name>",
                Style::default().fg(GREEN),
            )));
            lines.push(Line::from(Span::styled(
                "        exported as its own share; the safe default",
                Style::default().fg(MUTED),
            )));
        }
        ForkMode::Worktree => {
            lines.push(Line::from(Span::styled(
                " worktree: nested inside the parent, shares refs and objects",
                Style::default().fg(YELLOW),
            )));
            lines.push(Line::from(Span::styled(
                "           not exported separately; mutable git state is shared",
                Style::default().fg(MUTED),
            )));
        }
    }
    if let Some(error) = &form.error {
        lines.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(RED),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        },
    );
    hint_buttons(
        frame,
        app,
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
        &[
            FooterDef {
                key: "Tab",
                label: "Clone/worktree",
                action: ActionId::ToggleForkMode,
            },
            FooterDef {
                key: "Enter",
                label: "Fork",
                action: ActionId::SubmitFork,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Close,
            },
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Modal;
    use crate::tui::ui::test_support::*;

    #[test]
    fn fork_modal_renders_clone_by_default_and_worktree_after_toggle() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::ForkWorkspace(ForkWorkspaceForm::new(
            "parser", "rust",
        )));
        let (text, hits) = render(&app);
        assert!(text.contains("fork workspace"), "{text}");
        assert!(text.contains("clone"), "{text}");
        assert!(text.contains("isolated git state"), "{text}");
        assert!(hits.footer.iter().any(|h| h.action == ActionId::SubmitFork));

        let Some(Modal::ForkWorkspace(mut form)) = app.modal.take() else {
            panic!("expected fork modal");
        };
        form.toggle_mode();
        app.modal = Some(Modal::ForkWorkspace(form));
        let (text, _) = render(&app);
        assert!(text.contains("worktree"), "{text}");
        assert!(text.contains("shares refs and objects"), "{text}");
    }
}
