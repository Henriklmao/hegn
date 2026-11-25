//! New-VM modal: name, harness, resources and extra packages.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::app::{ActionId, App, CreateVmFocus, FooterHit, ScrollHit};

use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::theme::{ACCENT, CYAN, GREEN, MUTED, ON_ACCENT, RED, TEXT, centered};
use super::super::widgets::{render_scrolled, scroll_pane};

pub(crate) fn render_create_vm_form(
    frame: &mut Frame,
    app: &App,
    form: &crate::tui::app::CreateVmForm,
    area: Rect,
) -> (Vec<FooterHit>, Option<ScrollHit>) {
    let rect = centered(area, 74, 84);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " new VM ",
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
            Constraint::Length(3),
            Constraint::Min(6),
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

    let name_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(form.focus == CreateVmFocus::Name))
        .title(Span::styled(
            " name ",
            field_style(form.focus == CreateVmFocus::Name),
        ));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(form.name.clone(), Style::default().fg(TEXT)),
            Span::styled(
                if form.focus == CreateVmFocus::Name {
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

    let harness_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(form.focus == CreateVmFocus::Harness))
        .title(Span::styled(
            " tool profile (←/→) ",
            field_style(form.focus == CreateVmFocus::Harness),
        ));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◀ ", Style::default().fg(MUTED)),
            Span::styled(
                form.harness.label(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ▶", Style::default().fg(MUTED)),
            Span::styled(
                format!("  {}", form.harness.packages().join(", ")),
                Style::default().fg(MUTED),
            ),
        ]))
        .block(harness_block),
        chunks[1],
    );

    let resources = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[2]);
    let vcpu_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(form.focus == CreateVmFocus::Vcpus))
        .title(Span::styled(
            " vCPUs (←/→) ",
            field_style(form.focus == CreateVmFocus::Vcpus),
        ));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}", form.vcpus),
            Style::default().fg(TEXT),
        )))
        .block(vcpu_block),
        resources[0],
    );
    let memory_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(form.focus == CreateVmFocus::Memory))
        .title(Span::styled(
            " memory MiB (←/→) ",
            field_style(form.focus == CreateVmFocus::Memory),
        ));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}", form.memory_mib),
            Style::default().fg(TEXT),
        )))
        .block(memory_block),
        resources[1],
    );

    let pkg_block = Block::default()
        .borders(Borders::ALL)
        .border_style(field_style(form.focus == CreateVmFocus::Packages))
        .title(Span::styled(
            " extra packages (space toggles) ",
            field_style(form.focus == CreateVmFocus::Packages),
        ));
    let pkg_inner = pkg_block.inner(chunks[3]);
    frame.render_widget(pkg_block, chunks[3]);
    let visible = pkg_inner.height.max(1) as usize;
    let pkg_offset = form
        .package_cursor
        .saturating_sub(visible.saturating_sub(1));
    let pkg_pane = scroll_pane(pkg_inner, form.packages.len() + 1, pkg_offset);
    let mut lines = Vec::new();
    for (index, package) in form.packages.iter().enumerate() {
        let cursor = index == form.package_cursor && form.focus == CreateVmFocus::Packages;
        let marker = if package.selected { "[x]" } else { "[ ]" };
        let style = if cursor {
            Style::default().fg(ON_ACCENT).bg(CYAN)
        } else if package.selected {
            Style::default().fg(GREEN)
        } else {
            Style::default().fg(MUTED)
        };
        lines.push(Line::from(Span::styled(
            format!(" {marker} {}", package.name),
            style,
        )));
    }
    lines.push(Line::from(Span::styled(
        " membership is managed from the VM screen, not in vm.toml",
        Style::default().fg(MUTED),
    )));
    render_scrolled(frame, app, &pkg_pane, lines);

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
            ..chunks[4]
        },
    );
    let footer = hint_buttons(
        frame,
        app,
        Rect {
            y: chunks[4].y + 1,
            height: 1,
            ..chunks[4]
        },
        &[
            FooterDef {
                key: "Tab",
                label: "Next field",
                action: ActionId::NextField,
            },
            FooterDef {
                key: "Enter",
                label: "Create VM",
                action: ActionId::SubmitVm,
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
    use crate::tui::app::{CreateVmForm, Modal};
    use crate::tui::ui::test_support::*;

    #[test]
    fn create_vm_modal_renders_fields_and_packages() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::CreateVm(CreateVmForm::new(
            crate::config::Harness::Opencode,
            4,
            4096,
        )));
        let (text, hits) = render(&app);
        assert!(text.contains("new VM"), "{text}");
        assert!(text.contains("OpenCode"), "{text}");
        assert!(text.contains("4096"), "{text}");
        assert!(text.contains("nodejs"), "{text}");
        assert!(hits.footer.iter().any(|h| h.action == ActionId::SubmitVm));
    }
}
