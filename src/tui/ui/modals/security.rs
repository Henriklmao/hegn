//! Security review modal for tirith-core findings.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::app::{ActionId, App, FooterHit, ScrollHit};

use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::text::fit_line;
use super::super::theme::{CYAN, MUTED, ON_ACCENT, RED, TEXT, YELLOW, centered};
use super::super::widgets::{render_scrolled, scroll_pane};

pub(crate) fn render_security(
    frame: &mut Frame,
    app: &App,
    prompt: &crate::tui::app::SecurityPrompt,
    area: Rect,
) -> (Vec<FooterHit>, Option<ScrollHit>) {
    let rect = centered(area, 82, 80);
    frame.render_widget(Clear, rect);
    let blocked = prompt.inspection.decision == crate::security::Decision::Block;
    let color = if blocked { RED } else { YELLOW };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            " security alert · tirith-core ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .spacing(1)
        .split(inner);

    let header = vec![
        Line::from(vec![
            Span::styled(" decision ", Style::default().fg(MUTED)),
            Span::styled(
                if blocked { "BLOCKED" } else { "REVIEW" },
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   project ", Style::default().fg(MUTED)),
            Span::styled(
                prompt.project.display().to_string(),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" command  ", Style::default().fg(MUTED)),
            Span::styled(prompt.command.clone(), Style::default().fg(TEXT)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(
            header
                .into_iter()
                .map(|line| fit_line(line, chunks[0].width as usize))
                .collect::<Vec<_>>(),
        ),
        chunks[0],
    );

    let mut lines = Vec::new();
    for finding in &prompt.inspection.findings {
        let severity_color = match finding.severity {
            crate::security::Severity::Critical | crate::security::Severity::High => RED,
            crate::security::Severity::Medium => YELLOW,
            _ => CYAN,
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" [{}] ", finding.severity.label()),
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(severity_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{}  ", finding.rule), Style::default().fg(MUTED)),
            Span::styled(finding.title.clone(), Style::default().fg(TEXT)),
        ]));
        if !finding.description.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("   {}", finding.description),
                Style::default().fg(MUTED),
            )));
        }
        lines.push(Line::default());
    }
    if prompt.inspection.destructive {
        lines.push(Line::from(Span::styled(
            " Destructive operation: create a git checkpoint before allowing.",
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(error) = &prompt.error {
        lines.push(Line::from(Span::styled(
            format!(" checkpoint failed: {error}"),
            Style::default().fg(RED),
        )));
    }
    let pane = scroll_pane(chunks[1], lines.len(), prompt.scroll);
    render_scrolled(frame, app, &pane, lines);

    let footer = hint_buttons(
        frame,
        app,
        chunks[2],
        &[
            FooterDef {
                key: "a",
                label: "Allow once",
                action: ActionId::Allow,
            },
            FooterDef {
                key: "c",
                label: "Checkpoint + allow",
                action: ActionId::Checkpoint,
            },
            FooterDef {
                key: "d",
                label: "Deny",
                action: ActionId::Deny,
            },
        ],
    );
    (footer, pane.hit)
}

#[cfg(test)]
mod tests {
    use crate::security::{Decision, Finding, Inspection, SecurityEngine, Severity};
    use crate::tui::app::{Modal, SecurityPrompt};
    use crate::tui::ui::test_support::*;
    use std::path::PathBuf;

    #[test]
    fn security_modal_renders_findings() {
        let (_dir, mut app) = app();
        let engine = SecurityEngine::default();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/projects/demo"),
            inspection: engine.inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        let (text, _) = render(&app);
        assert!(text.contains("security alert"));
        assert!(text.contains("BLOCKED"));
        assert!(text.contains("rm -rf /workspace"));
        assert!(text.contains("workspace"));
    }

    #[test]
    fn security_modal_review_lists_findings_and_destructive_warning() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "git reset --hard".into(),
            project: PathBuf::from("/projects/demo"),
            inspection: Inspection {
                decision: Decision::Warn,
                findings: vec![
                    Finding {
                        rule: "git.reset-hard".into(),
                        severity: Severity::Critical,
                        title: "Discards uncommitted work".into(),
                        description: "Everything staged is lost".into(),
                    },
                    Finding {
                        rule: "git.history".into(),
                        severity: Severity::Medium,
                        title: "Rewrites local history".into(),
                        description: String::new(),
                    },
                ],
                destructive: true,
                suggest_checkpoint: true,
            },
            error: None,
            scroll: 0,
        }));
        let (text, hits) = render(&app);

        assert!(text.contains("REVIEW"), "{text}");
        assert!(!text.contains("BLOCKED"), "{text}");
        assert!(text.contains("[CRITICAL]"), "{text}");
        assert!(text.contains("[MEDIUM]"), "{text}");
        assert!(text.contains("git.reset-hard"), "{text}");
        assert!(text.contains("Discards uncommitted work"), "{text}");
        assert!(text.contains("Everything staged is lost"), "{text}");
        assert!(text.contains("Rewrites local history"), "{text}");
        assert!(
            text.contains("Destructive operation: create a git checkpoint"),
            "{text}"
        );
        assert!(text.contains("<a> Allow once"), "{text}");
        assert!(text.contains("<c> Checkpoint + allow"), "{text}");
        assert!(text.contains("<d> Deny"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == crate::tui::app::ActionId::Checkpoint)
        );
    }

    #[test]
    fn security_modal_clips_long_findings_and_scrolls_with_an_error() {
        let (_dir, mut app) = app();
        let long = "x".repeat(200);
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: format!("rm -rf {long}"),
            project: PathBuf::from("/projects/demo"),
            inspection: Inspection {
                decision: Decision::Block,
                findings: (0..6)
                    .map(|index| Finding {
                        rule: format!("rule.{index}"),
                        severity: Severity::High,
                        title: format!("Long finding {index} {long}"),
                        description: format!("Description {index} {long}"),
                    })
                    .collect(),
                destructive: true,
                suggest_checkpoint: false,
            },
            error: Some("checkpoint repository is dirty".into()),
            scroll: 20,
        }));
        let (text, hits) = render_sized(&app, 60, 20);

        assert!(text.contains("checkpoint failed"), "{text}");
        assert!(
            text.contains("checkpoint failed: checkpoint repository"),
            "{text}"
        );
        assert!(
            !text.contains(&"x".repeat(120)),
            "the long command and findings must be ellipsized"
        );
        assert!(
            hits.modal_scroll.is_some(),
            "the overflowing findings list needs a scrollbar"
        );
    }
}
