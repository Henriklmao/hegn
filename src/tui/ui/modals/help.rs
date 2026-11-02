//! Searchable keybinding help modal.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::app::{ActionId, App, FooterHit, HelpState, ScrollHit};

use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::text::{fit_line, truncate};
use super::super::theme::{ACCENT, CYAN, MUTED, TEXT, centered};
use super::super::widgets::{render_scrolled, scroll_pane};

pub(crate) fn render_help(
    frame: &mut Frame,
    app: &App,
    help: &HelpState,
    area: Rect,
) -> (Vec<FooterHit>, Option<ScrollHit>) {
    let rect = centered(area, 72, 90);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " hegn help ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);

    let mut search_spans = vec![Span::styled(
        " / ",
        Style::default().fg(if help.searching { CYAN } else { MUTED }),
    )];
    if help.query.is_empty() {
        search_spans.push(Span::styled(
            if help.searching {
                "type to search keybindings…"
            } else {
                "search keybindings"
            },
            Style::default().fg(MUTED),
        ));
    } else {
        search_spans.push(Span::styled(help.query.clone(), Style::default().fg(TEXT)));
    }
    if help.searching {
        search_spans.push(Span::styled("▌", Style::default().fg(CYAN)));
    }
    frame.render_widget(
        Paragraph::new(fit_line(Line::from(search_spans), chunks[0].width as usize))
            .style(Style::default()),
        chunks[0],
    );

    let sections: Vec<(&str, &[(&str, &str)])> = vec![
        (
            "Projects",
            &[
                ("↑/↓ j/k", "move selection"),
                ("Enter", "open the project screen (does not boot)"),
                ("/", "fuzzy filter projects"),
                ("b / 2", "open the project screen on instances"),
                ("s / 3", "open the project screen on snapshots"),
                ("c", "create project"),
                ("h / ←", "import existing project"),
                ("d", "delete project (git status · keep or delete files)"),
                ("F4", "global settings (arrows/mouse · Ctrl-S saves)"),
                ("e", "open in $EDITOR"),
                ("g", "open in lazygit"),
                ("r", "refresh projects + instances"),
                ("F3", "doctor (host checks, automatic setup)"),
            ],
        ),
        (
            "Project",
            &[
                ("1 / 2 / 3 / 4", "instances / snapshots / health / git"),
                ("Tab", "cycle project sections"),
                ("i / a / Enter", "open the terminal or attach the instance"),
                ("Esc", "sections → split; ×2 back to dashboard"),
                ("←", "back to dashboard (VM keeps running)"),
                ("L", "launch the configured agent/command"),
                (
                    "F6",
                    "project settings: launch command, integration, auto-launch",
                ),
                ("F2", "real attach mode (Ctrl-B/^] detaches)"),
                ("S", "stop selected instance"),
                ("n", "start a new instance / new snapshot"),
                ("M", "multi-agent clones for this project"),
                ("F5", "instance settings"),
                ("d", "delete instance or snapshot"),
                ("security", "a allow · c checkpoint + allow · d deny"),
                ("Ctrl-B / Ctrl-]", "back to dashboard from every mode"),
            ],
        ),
        (
            "Terminal",
            &[
                ("i / a", "insert mode — write in the guest terminal"),
                ("L", "launch the configured agent/command"),
                ("Esc", "normal mode, then back to the sections"),
                (
                    "Enter",
                    "in insert mode: inspect with tirith-core, then send",
                ),
                ("PgUp/PgDn", "scroll output"),
                ("Ctrl-L", "clear scrollback"),
                ("security", "a allow · c checkpoint + allow · d deny"),
            ],
        ),
        (
            "Snapshots",
            &[
                ("2", "focus the snapshots section"),
                ("Enter", "restore selected snapshot"),
                ("d", "delete selected snapshot"),
                ("n", "new snapshot (CLI: hegn snapshot create)"),
            ],
        ),
        (
            "Security",
            &[
                ("a", "allow flagged command once"),
                ("c", "create git checkpoint, then allow"),
                ("d", "deny command"),
            ],
        ),
        (
            "Help",
            &[
                ("/", "search these keybindings"),
                ("Esc", "clear the search, then close"),
                ("↑/↓ j/k", "scroll the list"),
                ("PgUp/PgDn", "scroll one page"),
            ],
        ),
    ];

    // Narrow terminals get one column: the key on its own line, then the
    // description, instead of a key column beside a description column.
    let narrow = chunks[1].width < HELP_NARROW_WIDTH;
    let total: usize = sections.iter().map(|(_, entries)| entries.len()).sum();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut shown = 0usize;
    for (title, entries) in sections {
        let matches: Vec<&(&str, &str)> = entries
            .iter()
            .filter(|(key, description)| help.matches(title, key, description))
            .collect();
        if matches.is_empty() {
            continue;
        }
        shown += matches.len();
        lines.push(Line::from(Span::styled(
            title,
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )));
        for (key, description) in matches {
            if narrow {
                lines.push(Line::from(Span::styled(
                    format!("  {key}"),
                    Style::default().fg(ACCENT),
                )));
                lines.push(Line::from(Span::styled(
                    format!("    {description}"),
                    Style::default().fg(TEXT),
                )));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {key:<12}"), Style::default().fg(ACCENT)),
                    Span::styled(*description, Style::default().fg(TEXT)),
                ]));
            }
        }
        lines.push(Line::default());
    }

    let tail_rows = if shown == 0 { 2 } else { 1 };
    let pane = scroll_pane(chunks[1], lines.len() + tail_rows, help.scroll);
    let width = pane.content.width as usize;
    if shown == 0 {
        lines.push(Line::from(Span::styled(
            truncate(
                &format!(" no keybindings match \"{}\"", help.query.trim()),
                width,
            ),
            Style::default().fg(MUTED),
        )));
        lines.push(Line::from(Span::styled(
            truncate(" Esc clears the search", width),
            Style::default().fg(MUTED),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            truncate(
                &format!(
                    " {shown}/{total} keybindings · all actions are also available non-interactively: hegn --help"
                ),
                width,
            ),
            Style::default().fg(MUTED),
        )));
    }
    render_scrolled(frame, app, &pane, lines);

    let buttons = if help.searching {
        vec![FooterDef {
            key: "Esc",
            label: "Stop search",
            action: ActionId::HelpSearchStop,
        }]
    } else {
        vec![
            FooterDef {
                key: "/",
                label: "Search",
                action: ActionId::HelpSearch,
            },
            FooterDef {
                key: "Esc",
                label: "Close",
                action: ActionId::Close,
            },
        ]
    };
    (hint_buttons(frame, app, chunks[2], &buttons), pane.hit)
}

/// Below this body width the help table stacks key and description.
pub(crate) const HELP_NARROW_WIDTH: u16 = 56;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Modal;
    use crate::tui::ui::test_support::*;

    #[test]
    fn help_modal_renders_keybindings() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Help(HelpState::default()));
        let (text, hits) = render(&app);
        assert!(text.contains("hegn help"));
        assert!(text.contains("fuzzy filter"));
        assert!(text.contains("checkpoint"));
        assert!(text.contains("search keybindings"));
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::HelpSearch)
        );
    }

    #[test]
    fn help_modal_search_filters_sections_and_offers_stop() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Help(HelpState {
            query: "checkpoint".into(),
            searching: true,
            ..HelpState::default()
        }));
        let (text, hits) = render(&app);
        assert!(text.contains("checkpoint"), "matching row is kept");
        assert!(!text.contains("fuzzy filter"), "other rows are hidden");
        assert!(text.contains("Stop search"));
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::HelpSearchStop)
        );

        app.modal = Some(Modal::Help(HelpState {
            query: "zzz".into(),
            searching: false,
            ..HelpState::default()
        }));
        let (text, hits) = render(&app);
        assert!(text.contains("no keybindings match"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::HelpSearch)
        );
    }

    #[test]
    fn help_modal_stacks_key_and_description_on_narrow_terminals() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Help(HelpState::default()));
        let (text, hits) = render_sized(&app, 60, 24);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.windows(2).any(|window| {
                window[0].contains("↑/↓ j/k")
                    && !window[0].contains("move selection")
                    && window[1].contains("move selection")
            }),
            "narrow help stacks key and description:\n{text}"
        );
        let scroll = hits
            .modal_scroll
            .expect("a long help list needs a scrollbar");
        assert!(scroll.max > 0);
        assert!(scroll.offset == 0);
        assert!(scroll.rect.width == 1);
        assert!(scroll.rect.height > 1);
    }

    #[test]
    fn help_modal_keeps_two_columns_on_wide_terminals() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Help(HelpState::default()));
        let (text, hits) = render_sized(&app, 120, 42);
        assert!(
            text.lines()
                .any(|line| line.contains("↑/↓ j/k") && line.contains("move selection")),
            "wide help keeps the key column"
        );
        assert!(hits.modal_scroll.is_some());
    }

    #[test]
    fn help_modal_fits_tiny_terminals() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Help(HelpState::default()));
        let (text, hits) = render_sized(&app, 36, 10);
        assert!(text.contains("hegn help"), "{text}");
        let scroll = hits.modal_scroll.expect("an overflowing help list scrolls");
        assert!(scroll.rect.x + scroll.rect.width <= 36);
        assert!(scroll.rect.y + scroll.rect.height <= 10);
    }
}
