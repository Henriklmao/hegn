//! Assign-project and detach-project prompts.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::tui::app::{ActionId, App, AssignProjectForm, DetachProjectModal};

use super::super::ModalHits;
use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::theme::{ACCENT, CYAN, MUTED, ON_ACCENT, RED, TEXT, YELLOW, centered};
use super::message::render_message;

pub(crate) fn render_assign_project(
    frame: &mut Frame,
    app: &App,
    form: &AssignProjectForm,
    area: Rect,
) -> ModalHits {
    let rect = centered(area, 76, 62);
    frame.render_widget(ratatui::widgets::Clear, rect);
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " assign project ",
            Style::default()
                .fg(ACCENT)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let heading = match (&form.project, &form.vm) {
        (Some(project), None) => format!("Project {project} joins which VM?"),
        (None, Some(vm)) => format!("Which project joins VM {vm}?"),
        _ => "Assign project".to_string(),
    };
    let mut lines = vec![ratatui::text::Line::from(Span::styled(
        format!(" {heading}"),
        Style::default().fg(TEXT),
    ))];
    lines.push(ratatui::text::Line::default());
    if form.candidates.is_empty() {
        lines.push(ratatui::text::Line::from(Span::styled(
            " no candidates available",
            Style::default().fg(MUTED),
        )));
    }
    for (index, candidate) in form.candidates.iter().enumerate() {
        let selected = index == form.cursor;
        let style = if selected {
            Style::default()
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(ratatui::text::Line::from(Span::styled(
            format!(" {} {}", if selected { "▸" } else { " " }, candidate),
            style,
        )));
    }
    lines.push(ratatui::text::Line::default());
    lines.push(ratatui::text::Line::from(vec![
        Span::styled(" read-only ", Style::default().fg(MUTED)),
        Span::styled(
            if form.read_only { "on" } else { "off" },
            Style::default().fg(if form.read_only { YELLOW } else { TEXT }),
        ),
    ]));
    if let Some(error) = &form.error {
        lines.push(ratatui::text::Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(RED),
        )));
    }
    let body_area = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    frame.render_widget(Paragraph::new(lines), body_area);
    let footer = hint_buttons(
        frame,
        app,
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
        &[
            FooterDef {
                key: "←/→",
                label: "Choose",
                action: ActionId::NextField,
            },
            FooterDef {
                key: "r",
                label: "Read-only",
                action: ActionId::ToggleReadOnly,
            },
            FooterDef {
                key: "Enter",
                label: "Assign",
                action: ActionId::SubmitAssign,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Close,
            },
        ],
    );
    ModalHits::buttons(footer)
}

pub(crate) fn render_detach_project(
    frame: &mut Frame,
    app: &App,
    modal: &DetachProjectModal,
    area: Rect,
) -> ModalHits {
    let body = format!(
        "Detach '{}' from VM '{}'?\n\nThe project stays registered and its files are never \
         touched; the VM keeps running with the shares it booted with until it is applied.",
        modal.project, modal.vm
    );
    let (footer, scroll) = render_message(
        frame,
        app,
        area,
        72,
        46,
        1,
        "detach project",
        &body,
        YELLOW,
        modal.scroll,
        &[
            FooterDef {
                key: "y",
                label: "Detach",
                action: ActionId::Confirm,
            },
            FooterDef {
                key: "n",
                label: "Cancel",
                action: ActionId::Cancel,
            },
        ],
    );
    ModalHits {
        footer,
        scroll,
        ..ModalHits::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Modal;
    use crate::tui::ui::test_support::*;

    #[test]
    fn assign_modal_lists_candidates_and_read_only_state() {
        let (_dir, mut app) = app();
        let mut form = AssignProjectForm::for_project("demo", vec!["rust".into(), "go".into()]);
        form.read_only = true;
        app.modal = Some(Modal::AssignProject(form));
        let (text, hits) = render(&app);
        assert!(text.contains("assign project"), "{text}");
        assert!(text.contains("rust"), "{text}");
        assert!(text.contains("go"), "{text}");
        assert!(text.contains("read-only"), "{text}");
        assert!(text.contains("on"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|h| h.action == ActionId::SubmitAssign)
        );
    }

    #[test]
    fn detach_modal_is_explicit_and_protects_files() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::DetachProject(DetachProjectModal {
            project: "demo".into(),
            vm: "rust".into(),
            scroll: 0,
        }));
        let (text, hits) = render(&app);
        assert!(text.contains("detach project"), "{text}");
        assert!(text.contains("files are never"), "{text}");
        assert!(text.contains("<y> Detach"), "{text}");
        assert!(hits.footer.iter().any(|h| h.action == ActionId::Confirm));
    }
}
