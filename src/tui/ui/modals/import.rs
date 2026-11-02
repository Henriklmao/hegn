//! Import-existing-project modal.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::app::{ActionId, App, FooterHit};

use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::theme::{ACCENT, CYAN, MUTED, RED, TEXT, centered};

pub(crate) fn render_import_form(
    frame: &mut Frame,
    app: &App,
    form: &crate::tui::app::ImportProjectForm,
    area: Rect,
) -> Vec<FooterHit> {
    let rect = centered(area, 78, 56);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " import project ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .spacing(1)
        .split(inner);

    let field_style = |focus: bool| {
        if focus {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        }
    };

    let path_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(
            form.focus == crate::tui::app::ImportFocus::Path,
        ))
        .title(Span::styled(
            " existing directory (~ allowed) ",
            field_style(form.focus == crate::tui::app::ImportFocus::Path),
        ))
        .style(Style::default());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(form.path.clone(), Style::default().fg(TEXT)),
            Span::styled(
                if form.focus == crate::tui::app::ImportFocus::Path {
                    "▌"
                } else {
                    ""
                },
                Style::default().fg(CYAN),
            ),
        ]))
        .block(path_block),
        chunks[0],
    );

    let name_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(
            form.focus == crate::tui::app::ImportFocus::Name,
        ))
        .title(Span::styled(
            " picker name (optional) ",
            field_style(form.focus == crate::tui::app::ImportFocus::Name),
        ))
        .style(Style::default());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(form.name.clone(), Style::default().fg(TEXT)),
            Span::styled(
                if form.focus == crate::tui::app::ImportFocus::Name {
                    "▌"
                } else {
                    ""
                },
                Style::default().fg(CYAN),
            ),
        ]))
        .block(name_block),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                " Creates a symlink; the project stays where it is.",
                Style::default().fg(TEXT),
            )),
            Line::from(Span::styled(
                format!(" Import target: {}", app.config.projects_root.display()),
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                " Refuses /, $HOME, ancestors of the root and hegn state paths.",
                Style::default().fg(MUTED),
            )),
        ]),
        chunks[2],
    );

    // Errors + clickable keybinding hints.
    let mut bottom = Vec::new();
    if let Some(error) = &form.error {
        bottom.push(Line::from(Span::styled(
            format!(" error: {error}"),
            Style::default().fg(RED),
        )));
    }
    frame.render_widget(
        Paragraph::new(bottom),
        Rect {
            height: 1,
            ..chunks[3]
        },
    );
    hint_buttons(
        frame,
        app,
        Rect {
            y: chunks[3].y + 1,
            height: 1,
            ..chunks[3]
        },
        &[
            FooterDef {
                key: "Tab",
                label: "Next field",
                action: ActionId::NextField,
            },
            FooterDef {
                key: "Enter",
                label: "Import",
                action: ActionId::SubmitImport,
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
    fn import_modal_renders_the_symlink_explanation() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::ImportProject(
            crate::tui::app::ImportProjectForm::new(),
        ));
        let (text, hits) = render(&app);
        assert!(text.contains("import project"), "{text}");
        assert!(text.contains("Creates a symlink"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::SubmitImport)
        );
    }
}
