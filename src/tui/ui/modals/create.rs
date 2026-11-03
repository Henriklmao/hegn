//! New-project modal with name, harness and package selection.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::app::{ActionId, App, CreateFocus, FooterHit, ScrollHit};

use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::theme::{ACCENT, CYAN, GREEN, MUTED, ON_ACCENT, RED, TEXT, centered};
use super::super::widgets::{render_scrolled, scroll_pane};

pub(crate) fn render_create_form(
    frame: &mut Frame,
    app: &App,
    form: &crate::tui::app::CreateProjectForm,
    area: Rect,
) -> (Vec<FooterHit>, Option<ScrollHit>) {
    let rect = centered(area, 74, 84);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " new project ",
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
            Constraint::Min(8),
            Constraint::Length(2),
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

    // Name.
    let name_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(form.focus == CreateFocus::Name))
        .title(Span::styled(
            " name ",
            field_style(form.focus == CreateFocus::Name),
        ))
        .style(Style::default());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(form.name.clone(), Style::default().fg(TEXT)),
            Span::styled(
                if form.focus == CreateFocus::Name {
                    "▌"
                } else {
                    ""
                },
                Style::default().fg(CYAN),
            ),
        ]))
        .block(name_block),
        chunks[0],
    );

    // Harness.
    let harness_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(form.focus == CreateFocus::Harness))
        .title(Span::styled(
            " harness (←/→) ",
            field_style(form.focus == CreateFocus::Harness),
        ))
        .style(Style::default());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◀ ", Style::default().fg(MUTED)),
            Span::styled(
                form.harness.label(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ▶", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "  {} · preinstalls: {}",
                    form.harness.preset_summary(),
                    form.harness.packages().join(", ")
                ),
                Style::default().fg(MUTED),
            ),
        ]))
        .block(harness_block),
        chunks[1],
    );

    // Packages.
    let pkg_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(form.focus == CreateFocus::Packages))
        .title(Span::styled(
            " extra packages (space toggles) ",
            field_style(form.focus == CreateFocus::Packages),
        ))
        .style(Style::default());
    let pkg_inner = pkg_block.inner(chunks[2]);
    frame.render_widget(pkg_block, chunks[2]);
    let visible = pkg_inner.height.max(1) as usize;
    let pkg_offset = form
        .package_cursor
        .saturating_sub(visible.saturating_sub(1));
    let pkg_pane = scroll_pane(pkg_inner, form.packages.len() + 2, pkg_offset);
    let mut pkg_lines = Vec::new();
    for (index, package) in form.packages.iter().enumerate() {
        let cursor = index == form.package_cursor && form.focus == CreateFocus::Packages;
        let marker = if package.selected { "[x]" } else { "[ ]" };
        let style = if cursor {
            Style::default().fg(ON_ACCENT).bg(CYAN)
        } else if package.selected {
            Style::default().fg(GREEN)
        } else {
            Style::default().fg(MUTED)
        };
        pkg_lines.push(Line::from(Span::styled(
            format!(" {marker} {}", package.name),
            style,
        )));
    }
    pkg_lines.push(Line::default());
    pkg_lines.push(Line::from(Span::styled(
        format!(
            " template: {}",
            if app.config.templates_dir.join("AGENTS.md").is_file() {
                app.config
                    .templates_dir
                    .join("AGENTS.md")
                    .display()
                    .to_string()
            } else {
                "built-in AGENTS.md".to_string()
            }
        ),
        Style::default().fg(MUTED),
    )));
    render_scrolled(frame, app, &pkg_pane, pkg_lines);

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
    let footer = hint_buttons(
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
                label: "Create",
                action: ActionId::SubmitCreate,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Close,
            },
        ],
    );
    (footer, pkg_pane.hit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Modal;
    use crate::tui::ui::test_support::*;

    #[test]
    fn create_modal_renders_fields_and_packages() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::CreateProject(
            crate::tui::app::CreateProjectForm::new(crate::config::Harness::Opencode),
        ));
        let (text, hits) = render(&app);
        assert!(text.contains("new project"));
        assert!(text.contains("OpenCode"));
        assert!(text.contains("nodejs"));
        assert!(hits.footer.iter().any(|h| h.action == ActionId::Close));
    }
}
