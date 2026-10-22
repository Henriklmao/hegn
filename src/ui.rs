//! Ratatui renderer. Pure functions over [`App`] make the UI testable with a
//! `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    ActionId, App, CreateFocus, Focus, FooterHit, Modal, RowHit, SandboxState, Screen, ToastLevel,
};

const BG: Color = Color::Rgb(13, 17, 23);
const PANEL: Color = Color::Rgb(22, 27, 34);
const BORDER: Color = Color::Rgb(48, 54, 61);
const TEXT: Color = Color::Rgb(192, 197, 213);
const MUTED: Color = Color::Rgb(110, 118, 145);
const ACCENT: Color = Color::Rgb(122, 162, 247);
const CYAN: Color = Color::Rgb(125, 207, 255);
const GREEN: Color = Color::Rgb(158, 206, 106);
const YELLOW: Color = Color::Rgb(224, 175, 104);
const RED: Color = Color::Rgb(247, 118, 142);

#[derive(Debug, Default)]
pub struct UiHits {
    pub footer: Vec<FooterHit>,
    pub projects: Vec<RowHit>,
    pub snapshots: Vec<RowHit>,
}

#[derive(Clone, Copy)]
struct FooterDef {
    key: &'static str,
    label: &'static str,
    action: ActionId,
}

pub fn draw(frame: &mut Frame, app: &App) -> UiHits {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);

    render_header(frame, app, chunks[0]);
    let hits = match app.screen {
        Screen::Dashboard => render_dashboard(frame, app, chunks[1]),
        Screen::Terminal => render_terminal(frame, app, chunks[1]),
    };
    let footer = render_footer(frame, app, chunks[2]);
    if let Some(modal) = &app.modal {
        render_modal(frame, app, modal, area);
    }
    render_toast(frame, app, chunks[1]);

    UiHits {
        footer,
        projects: hits.0,
        snapshots: hits.1,
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tabs = if app.screen == Screen::Dashboard {
        "dashboard"
    } else {
        "terminal"
    };
    let left = Line::from(vec![
        Span::styled(
            " hegn ",
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "Firecracker sandboxes for coding agents",
            Style::default().fg(MUTED),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{tabs}]"),
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(left).style(Style::default().bg(PANEL)),
        inner,
    );

    let status = match app.sandbox.state {
        SandboxState::Running => ("● running", GREEN),
        SandboxState::Starting => ("◐ starting", YELLOW),
        SandboxState::Paused => ("Ⅱ paused", YELLOW),
        SandboxState::Failed => ("✕ failed", RED),
        SandboxState::Stopped => ("○ stopped", MUTED),
    };
    let right_text = format!("{}  {} ", status.0, app.sandbox.backend);
    let width = right_text.width() as u16;
    if inner.width > width + 10 {
        let right = Rect {
            x: inner.x + inner.width.saturating_sub(width),
            y: inner.y,
            width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                right_text,
                Style::default().fg(status.1),
            )))
            .alignment(Alignment::Right),
            right,
        );
    }
}

fn render_dashboard(frame: &mut Frame, app: &App, area: Rect) -> (Vec<RowHit>, Vec<RowHit>) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(20)])
        .split(area);
    let sidebar = render_sidebar(frame, app, columns[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Min(6),
        ])
        .spacing(1)
        .split(columns[1]);

    render_project_panel(frame, app, right[0]);
    render_sandbox_panel(frame, app, right[1]);
    render_snapshot_detail(frame, app, right[2]);
    sidebar
}

fn render_sidebar(frame: &mut Frame, app: &App, area: Rect) -> (Vec<RowHit>, Vec<RowHit>) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .spacing(1)
        .split(area);
    let projects = render_projects(frame, app, sections[0]);
    let snapshots = render_snapshots(frame, app, sections[1]);
    (projects, snapshots)
}

fn render_projects(frame: &mut Frame, app: &App, area: Rect) -> Vec<RowHit> {
    let active = app.focus == Focus::Projects;
    let title = format!(" projects ({}) ", app.projects.len());
    let block = panel_block(title, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let query_line = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let mut query_spans = vec![Span::styled(
        " / ",
        Style::default().fg(if app.searching { CYAN } else { MUTED }),
    )];
    if app.query.is_empty() {
        query_spans.push(Span::styled(
            if app.searching {
                "type to filter…"
            } else {
                "filter projects"
            },
            Style::default().fg(MUTED),
        ));
    } else {
        query_spans.push(Span::styled(app.query.clone(), Style::default().fg(TEXT)));
    }
    if app.searching {
        query_spans.push(Span::styled("▌", Style::default().fg(CYAN)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(query_spans)).style(Style::default().bg(PANEL)),
        query_line[0],
    );

    let list_area = query_line[1];
    let mut hits = Vec::new();
    if app.loading {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} scanning projects…", spinner(app.tick())),
                Style::default().fg(MUTED),
            ))),
            list_area,
        );
        return hits;
    }
    if app.visible.is_empty() {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    " no projects found",
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    " press c to create one",
                    Style::default().fg(MUTED),
                )),
            ])),
            list_area,
        );
        return hits;
    }

    // Two lines per project.
    let visible_rows = (list_area.height / 2).max(1) as usize;
    let offset = app.selected.saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, &project_index) in app
        .visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
    {
        let project = &app.projects[project_index];
        let selected = row == app.selected && active;
        let base = if selected {
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(PANEL)
        };
        let marker = if project.git.is_repo {
            if project.git.is_dirty() { "●" } else { "◆" }
        } else {
            "◇"
        };
        let name_line = Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                base.fg(if selected {
                    BG
                } else {
                    branch_color(&project.git)
                }),
            ),
            Span::styled(project.name.clone(), base),
        ]);
        lines.push(name_line);
        let detail = Line::from(Span::styled(
            format!("   {}", project.git.badge()),
            if selected {
                Style::default().fg(BG).bg(ACCENT)
            } else {
                Style::default().fg(MUTED).bg(PANEL)
            },
        ));
        lines.push(detail);
        hits.push(RowHit {
            rect: Rect {
                x: list_area.x,
                y: list_area.y + ((row - offset) as u16) * 2,
                width: list_area.width,
                height: 2,
            },
            index: row,
        });
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default().bg(PANEL)),
        list_area,
    );
    hits
}

fn branch_color(git: &crate::project::GitSummary) -> Color {
    if !git.is_repo {
        MUTED
    } else if git.is_dirty() {
        YELLOW
    } else {
        GREEN
    }
}

fn render_snapshots(frame: &mut Frame, app: &App, area: Rect) -> Vec<RowHit> {
    let active = app.focus == Focus::Snapshots;
    let title = format!(" snapshots ({}) ", app.snapshots.len());
    let block = panel_block(title, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = Vec::new();
    if app.snapshots.is_empty() {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(" ▾ no snapshots", Style::default().fg(MUTED))),
                Line::from(Span::styled(
                    "   hegn snapshot create",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return hits;
    }

    let visible_rows = inner.height as usize;
    let offset = app
        .selected_snapshot
        .saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, snapshot) in app
        .snapshots
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
    {
        let selected = row == app.selected_snapshot && active;
        let style = if selected {
            Style::default()
                .fg(BG)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(PANEL)
        };
        let prefix = if row == offset { " ▾ " } else { "   " };
        lines.push(Line::from(vec![
            Span::styled(prefix, style.fg(if selected { BG } else { CYAN })),
            Span::styled(truncate(&snapshot.name, 20), style),
            Span::styled(
                format!("  {}", snapshot.size_label()),
                if selected {
                    Style::default().fg(BG).bg(CYAN)
                } else {
                    Style::default().fg(MUTED).bg(PANEL)
                },
            ),
        ]));
        hits.push(RowHit {
            rect: Rect {
                x: inner.x,
                y: inner.y + (row - offset) as u16,
                width: inner.width,
                height: 1,
            },
            index: row,
        });
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default().bg(PANEL)),
        inner,
    );
    hits
}

fn render_project_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(" project ".to_string(), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(project) = app.selected_project() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Select a project, or press c to create one.",
                Style::default().fg(MUTED),
            ))),
            inner,
        );
        return;
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", project.name),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(project.display_path(), Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            Span::styled(" git  ", Style::default().fg(MUTED)),
            Span::styled(
                project.git.badge(),
                Style::default().fg(branch_color(&project.git)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" last ", Style::default().fg(MUTED)),
            Span::styled(
                project
                    .last_opened
                    .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "never opened".into()),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" runs ", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "{} inside {}",
                    app.config.harness.label(),
                    app.config.guest_mount
                ),
                Style::default().fg(TEXT),
            ),
        ]),
    ];
    if !project.git.is_repo {
        lines.push(Line::from(Span::styled(
            " not a git repository — checkpoints unavailable",
            Style::default().fg(YELLOW),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL)),
        inner,
    );
}

fn render_sandbox_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(" sandbox ".to_string(), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (state_label, state_color) = match app.sandbox.state {
        SandboxState::Running => ("RUNNING", GREEN),
        SandboxState::Starting => ("STARTING", YELLOW),
        SandboxState::Paused => ("PAUSED", YELLOW),
        SandboxState::Failed => ("FAILED", RED),
        SandboxState::Stopped => ("STOPPED", MUTED),
    };
    let uptime = app
        .sandbox
        .uptime()
        .map(|d| format!("{}s", d.as_secs()))
        .unwrap_or_else(|| "—".into());
    let socket = app
        .sandbox
        .socket
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "none".into());

    let lines = vec![
        Line::from(vec![
            Span::styled(" state    ", Style::default().fg(MUTED)),
            Span::styled(
                format!("● {state_label}"),
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" backend  ", Style::default().fg(MUTED)),
            Span::styled(app.sandbox.backend, Style::default().fg(TEXT)),
            Span::styled("   virtiofs ", Style::default().fg(MUTED)),
            Span::styled(app.sandbox.virtiofs, Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" resources ", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "{} vCPU · {} MiB RAM · up {uptime}",
                    app.sandbox.vcpus, app.sandbox.memory_mib
                ),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" socket   ", Style::default().fg(MUTED)),
            Span::styled(truncate(&socket, 52), Style::default().fg(TEXT)),
        ]),
        Line::from(Span::styled(
            app.sandbox.note.clone().unwrap_or_else(|| {
                "No active sandbox. Press Enter to launch the selected project.".into()
            }),
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL)),
        inner,
    );
}

fn render_snapshot_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(" snapshot manager ".to_string(), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(snapshot) = app.selected_snapshot() else {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    "No VM checkpoints yet.",
                    Style::default().fg(TEXT),
                )),
                Line::from(Span::styled(
                    "Create one with: hegn snapshot create <project>",
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    "Snapshots restore RAM + CPU state in milliseconds.",
                    Style::default().fg(MUTED),
                )),
            ]))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(" ◆ ", Style::default().fg(CYAN)),
            Span::styled(
                snapshot.name.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" created ", Style::default().fg(MUTED)),
            Span::styled(
                snapshot
                    .created_at
                    .format("%Y-%m-%d %H:%M:%S UTC")
                    .to_string(),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" state   ", Style::default().fg(MUTED)),
            Span::styled(snapshot.vm_state.clone(), Style::default().fg(TEXT)),
            Span::styled("   size ", Style::default().fg(MUTED)),
            Span::styled(snapshot.size_label(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" project ", Style::default().fg(MUTED)),
            Span::styled(
                snapshot
                    .project
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "—".into()),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(Span::styled(
            " Enter restore · d delete · n new",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL)),
        inner,
    );
}

fn render_terminal(frame: &mut Frame, app: &App, area: Rect) -> (Vec<RowHit>, Vec<RowHit>) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(20)])
        .split(area);
    let sidebar = render_sidebar(frame, app, columns[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(columns[1]);

    let session_label = app
        .session
        .as_ref()
        .map(|s| format!("{} · {}", s.label(), s.project().display()))
        .unwrap_or_else(|| "no session".into());
    let mode = if app.raw_mode {
        Span::styled(
            " RAW ",
            Style::default().fg(BG).bg(RED).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            " INTERCEPT ",
            Style::default()
                .fg(BG)
                .bg(GREEN)
                .add_modifier(Modifier::BOLD),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ▸ ", Style::default().fg(ACCENT)),
            Span::styled(session_label, Style::default().fg(MUTED)),
            Span::raw(" "),
            mode,
        ])),
        right[0],
    );

    let block = panel_block(" terminal ".to_string(), true);
    let out = block.inner(right[1]);
    frame.render_widget(block, right[1]);
    let available = out.height.max(1) as usize;
    let total = app.terminal.len();
    let scroll = app.scroll.min(total.saturating_sub(available));
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(available);
    let visible: Vec<Line> = app
        .terminal
        .range(start, end)
        .into_iter()
        .map(|line| {
            let color = if line.starts_with("$ ") { GREEN } else { TEXT };
            Line::from(Span::styled(line, Style::default().fg(color)))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(visible)).style(Style::default().bg(PANEL)),
        out,
    );

    let input_block = panel_block(
        if app.raw_mode {
            " raw passthrough ".to_string()
        } else {
            " command (tirith-core) ".to_string()
        },
        true,
    );
    let input_inner = input_block.inner(right[2]);
    frame.render_widget(input_block, right[2]);
    if app.raw_mode {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Keys are forwarded directly. F2 re-enables interception, Esc returns.",
                Style::default().fg(YELLOW),
            ))),
            input_inner,
        );
    } else {
        let prompt = Span::styled("❯ ", Style::default().fg(ACCENT));
        let available = input_inner.width.saturating_sub(3) as usize;
        let shown = if app.input.width() > available {
            // Scroll by characters, not by display columns: skipping `width`
            // characters would cut wide (CJK) input in the wrong place.
            let mut width = app.input.width();
            let mut skip = 0;
            for ch in app.input.chars() {
                if width <= available {
                    break;
                }
                width -= ch.width().unwrap_or(0);
                skip += 1;
            }
            app.input.chars().skip(skip).collect::<String>()
        } else {
            app.input.clone()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                prompt,
                Span::styled(shown.clone(), Style::default().fg(TEXT)),
                Span::styled("▌", Style::default().fg(CYAN)),
            ])),
            input_inner,
        );
        let cursor_x = input_inner
            .x
            .saturating_add(2)
            .saturating_add(shown.width() as u16)
            .min(input_inner.x + input_inner.width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, input_inner.y));
    }
    sidebar
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) -> Vec<FooterHit> {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let buttons = footer_buttons(app);
    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut x = inner.x;
    for button in buttons {
        let text = format!("<{}> {}", button.key, button.label);
        let width = (text.width() as u16) + 2;
        if x + width > inner.x + inner.width {
            break;
        }
        let rect = Rect {
            x,
            y: inner.y,
            width,
            height: 1,
        };
        spans.push(Span::styled(
            format!(" {text} "),
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        hits.push(FooterHit {
            rect,
            action: button.action,
        });
        x += width + 1;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    if inner.height > 1 {
        let hint = if app.searching {
            " typing filter · ↑↓ select · Enter open · Esc cancel ".to_string()
        } else if app.screen == Screen::Terminal {
            " Esc dashboard · Ctrl-B dashboard · PgUp/PgDn scroll · Ctrl-L clear ".to_string()
        } else {
            " / filter · Tab focus · Enter open · c create · ? help ".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(MUTED)))),
            Rect {
                x: inner.x + 1,
                y: inner.y + 1,
                width: inner.width.saturating_sub(1),
                height: 1,
            },
        );
    }
    hits
}

fn footer_buttons(app: &App) -> Vec<FooterDef> {
    match &app.modal {
        Some(Modal::Security(_)) => vec![
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
        Some(Modal::CreateProject(_)) => vec![
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
        Some(Modal::Confirm(_)) => vec![
            FooterDef {
                key: "y",
                label: "Confirm",
                action: ActionId::Confirm,
            },
            FooterDef {
                key: "n",
                label: "Cancel",
                action: ActionId::Cancel,
            },
        ],
        Some(Modal::Help | Modal::Error { .. }) => {
            vec![FooterDef {
                key: "Esc",
                label: "Close",
                action: ActionId::Close,
            }]
        }
        None if app.screen == Screen::Terminal => vec![
            FooterDef {
                key: "F2",
                label: if app.raw_mode { "Intercept" } else { "Raw" },
                action: ActionId::ToggleRaw,
            },
            FooterDef {
                key: "Ctrl-B",
                label: "Dashboard",
                action: ActionId::Back,
            },
            FooterDef {
                key: "PgUp",
                label: "Scroll",
                action: ActionId::ScrollUp,
            },
            FooterDef {
                key: "?",
                label: "Help",
                action: ActionId::Help,
            },
        ],
        None => {
            let mut buttons = vec![
                FooterDef {
                    key: "Enter",
                    label: "Open",
                    action: ActionId::Open,
                },
                FooterDef {
                    key: "c",
                    label: "Create",
                    action: ActionId::Create,
                },
                FooterDef {
                    key: "e",
                    label: "Editor",
                    action: ActionId::Edit,
                },
                FooterDef {
                    key: "g",
                    label: "Lazygit",
                    action: ActionId::Lazygit,
                },
                FooterDef {
                    key: "r",
                    label: "Refresh",
                    action: ActionId::Refresh,
                },
            ];
            if app.focus == Focus::Snapshots {
                buttons.push(FooterDef {
                    key: "n",
                    label: "New snapshot",
                    action: ActionId::NewSnapshot,
                });
                buttons.push(FooterDef {
                    key: "d",
                    label: "Delete",
                    action: ActionId::DeleteSnapshot,
                });
            }
            buttons.push(FooterDef {
                key: "?",
                label: "Help",
                action: ActionId::Help,
            });
            buttons.push(FooterDef {
                key: "q",
                label: "Quit",
                action: ActionId::Quit,
            });
            buttons
        }
    }
}

fn render_modal(frame: &mut Frame, app: &App, modal: &Modal, area: Rect) {
    match modal {
        Modal::Help => render_help(frame, area),
        Modal::Error { title, message } => render_message(
            frame,
            area,
            60,
            30,
            title,
            message,
            RED,
            "Press any key to close",
        ),
        Modal::Confirm(prompt) => {
            let body = format!("{}\n\nConfirm: {}", prompt.message, prompt.confirm_label);
            render_message(
                frame,
                area,
                64,
                34,
                &prompt.title,
                &body,
                YELLOW,
                "y confirm · n/Esc cancel",
            );
        }
        Modal::CreateProject(form) => render_create_form(frame, app, form, area),
        Modal::Security(prompt) => render_security(frame, prompt, area),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_message(
    frame: &mut Frame,
    area: Rect,
    width_pct: u16,
    height_pct: u16,
    title: &str,
    body: &str,
    color: Color,
    hint: &str,
) {
    let rect = centered(area, width_pct, height_pct);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(MUTED))))
            .alignment(Alignment::Center),
        chunks[1],
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let rect = centered(area, 72, 82);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " hegn help ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let sections: Vec<(&str, &[(&str, &str)])> = vec![
        (
            "Projects",
            &[
                ("↑/↓ j/k", "move selection"),
                ("Enter", "open project in sandbox"),
                ("/", "fuzzy filter projects"),
                ("Tab", "switch projects / snapshots"),
                ("c", "create project"),
                ("e", "open in $EDITOR"),
                ("g", "open in lazygit"),
                ("r", "refresh git status"),
            ],
        ),
        (
            "Terminal",
            &[
                ("Enter", "inspect command with tirith-core, then send"),
                ("F2", "toggle raw passthrough (no interception)"),
                ("Ctrl-B / Esc", "return to dashboard"),
                ("PgUp/PgDn", "scroll output"),
                ("Ctrl-L", "clear scrollback"),
            ],
        ),
        (
            "Snapshots",
            &[
                ("Tab", "focus snapshot manager"),
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
    ];

    let mut lines = Vec::new();
    for (title, entries) in sections {
        lines.push(Line::from(Span::styled(
            title,
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )));
        for (key, description) in entries {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<12}"), Style::default().fg(ACCENT)),
                Span::styled(*description, Style::default().fg(TEXT)),
            ]));
        }
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        " All actions are also available non-interactively: hegn --help",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL)),
        inner,
    );
}

fn render_create_form(
    frame: &mut Frame,
    app: &App,
    form: &crate::app::CreateProjectForm,
    area: Rect,
) {
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
        .style(Style::default().bg(PANEL));
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
        .style(Style::default().bg(PANEL));
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
        .style(Style::default().bg(PANEL));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◀ ", Style::default().fg(MUTED)),
            Span::styled(
                form.harness.label(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ▶", Style::default().fg(MUTED)),
            Span::styled(
                format!("  preinstalls: {}", form.harness.packages().join(", ")),
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
            " packages (space toggles) ",
            field_style(form.focus == CreateFocus::Packages),
        ))
        .style(Style::default().bg(PANEL));
    let pkg_inner = pkg_block.inner(chunks[2]);
    frame.render_widget(pkg_block, chunks[2]);
    let mut pkg_lines = Vec::new();
    for (index, package) in form.packages.iter().enumerate() {
        let cursor = index == form.package_cursor && form.focus == CreateFocus::Packages;
        let marker = if package.selected { "[x]" } else { "[ ]" };
        let style = if cursor {
            Style::default().fg(BG).bg(CYAN)
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
    frame.render_widget(Paragraph::new(pkg_lines), pkg_inner);

    // Errors + submit.
    let mut bottom = Vec::new();
    if let Some(error) = &form.error {
        bottom.push(Line::from(Span::styled(
            format!(" error: {error}"),
            Style::default().fg(RED),
        )));
    }
    let submit_style = if form.focus == CreateFocus::Create {
        Style::default()
            .fg(BG)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    bottom.push(Line::from(Span::styled(
        "   [ Create project ]",
        submit_style,
    )));
    frame.render_widget(Paragraph::new(bottom), chunks[3]);
}

fn render_security(frame: &mut Frame, prompt: &crate::app::SecurityPrompt, area: Rect) {
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
        .style(Style::default().bg(PANEL));
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

    frame.render_widget(
        Paragraph::new(Text::from(vec![
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
        ])),
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
                    .fg(BG)
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
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "a allow once   c checkpoint + allow   d / Esc deny",
            Style::default().fg(MUTED),
        )))
        .alignment(Alignment::Center),
        chunks[2],
    );
}

fn render_toast(frame: &mut Frame, app: &App, area: Rect) {
    let Some(toast) = &app.toast else { return };
    let color = match toast.level {
        ToastLevel::Info => GREEN,
        ToastLevel::Warn => YELLOW,
        ToastLevel::Error => RED,
    };
    let width = (toast.text.width() as u16 + 4)
        .min(area.width.saturating_sub(4))
        .max(20);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width + 2),
        y: area.y + area.height.saturating_sub(3),
        width,
        height: 3,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                " {} ",
                truncate(&toast.text, width.saturating_sub(2) as usize)
            ),
            Style::default()
                .fg(BG)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .style(Style::default().bg(PANEL)),
        ),
        rect,
    );
}

fn panel_block(title: String, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if active { ACCENT } else { BORDER }))
        .title(Span::styled(
            title,
            Style::default()
                .fg(if active { ACCENT } else { MUTED })
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(PANEL))
}

fn spinner(tick: u64) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    FRAMES[(tick as usize / 2) % FRAMES.len()]
}

fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn truncate(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = ch.width().unwrap_or(0);
        // Reserve one column for the ellipsis.
        if width + char_width + 1 > max {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::app::SecurityPrompt;
    use crate::config::{BackendKind, Config};
    use crate::project::{GitSummary, Project};
    use crate::security::SecurityEngine;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.projects_root = dir.path().join("projects");
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.templates_dir = dir.path().join("templates");
        config.backend = BackendKind::Pty;
        config.auto_branch = false;
        std::fs::create_dir_all(&config.projects_root).unwrap();
        let mut app = App::new(config);
        app.set_projects(vec![Project {
            name: "demo".into(),
            path: PathBuf::from("/projects/demo"),
            git: GitSummary {
                is_repo: true,
                branch: Some("main".into()),
                modified: 1,
                ..GitSummary::default()
            },
            last_opened: None,
        }]);
        (dir, app)
    }

    fn render(app: &App) -> (String, UiHits) {
        let backend = TestBackend::new(120, 42);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = UiHits::default();
        terminal
            .draw(|frame| {
                hits = draw(frame, app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        (text, hits)
    }

    #[test]
    fn dashboard_renders_header_sidebar_and_footer() {
        let (_dir, app) = app();
        let (text, hits) = render(&app);
        assert!(text.contains("hegn"));
        assert!(text.contains("projects (1)"));
        assert!(text.contains("demo"));
        assert!(text.contains("sandbox"));
        assert!(text.contains("Open"));
        assert!(!hits.footer.is_empty());
        assert_eq!(hits.projects.len(), 1);
    }

    #[test]
    fn help_modal_renders_keybindings() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Help);
        let (text, _) = render(&app);
        assert!(text.contains("hegn help"));
        assert!(text.contains("fuzzy filter"));
        assert!(text.contains("checkpoint"));
    }

    #[test]
    fn create_modal_renders_fields_and_packages() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::CreateProject(crate::app::CreateProjectForm::new(
            crate::config::Harness::Opencode,
        )));
        let (text, hits) = render(&app);
        assert!(text.contains("new project"));
        assert!(text.contains("OpenCode"));
        assert!(text.contains("nodejs"));
        assert!(hits.footer.iter().any(|h| h.action == ActionId::Close));
    }

    #[test]
    fn security_modal_renders_findings() {
        let (_dir, mut app) = app();
        let engine = SecurityEngine::default();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/projects/demo"),
            inspection: engine.inspect("rm -rf /workspace", None),
            error: None,
        }));
        let (text, _) = render(&app);
        assert!(text.contains("security alert"));
        assert!(text.contains("BLOCKED"));
        assert!(text.contains("rm -rf /workspace"));
        assert!(text.contains("workspace"));
    }

    #[test]
    fn terminal_view_renders_buffer_and_prompt() {
        let (_dir, mut app) = app();
        app.screen = Screen::Terminal;
        app.terminal.push_bytes(b"hello from the sandbox\n");
        app.input = "cargo test".into();
        let (text, _) = render(&app);
        assert!(text.contains("hello from the sandbox"));
        assert!(text.contains("cargo test"));
        assert!(text.contains("INTERCEPT"));
    }

    #[test]
    fn empty_project_list_has_guidance() {
        let (_dir, mut app) = app();
        app.set_projects(vec![]);
        let (text, _) = render(&app);
        assert!(text.contains("no projects found"));
        assert!(text.contains("press c to create one"));
    }

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abc", 0), "");
        let wide = truncate("日本語", 4);
        assert!(wide.ends_with('…'), "{wide}");
        assert!(wide.width() <= 4, "{wide:?} has width {}", wide.width());
        let narrow = truncate("abcdef", 4);
        assert_eq!(narrow, "abc…");
    }

    #[test]
    fn snapshots_render_when_present() {
        let (_dir, mut app) = app();
        app.snapshots = vec![crate::snapshot::SnapshotMeta {
            name: "before-refactor".into(),
            created_at: chrono::Utc::now(),
            vm_state: "Paused".into(),
            snapshot_path: PathBuf::from("/snap/vm.snap"),
            mem_file_path: PathBuf::from("/snap/vm.mem"),
            project: Some(PathBuf::from("/projects/demo")),
            size_bytes: 1024 * 1024,
            note: None,
        }];
        let (text, hits) = render(&app);
        assert!(text.contains("before-refactor"));
        assert!(text.contains("1.0 MiB"));
        assert_eq!(hits.snapshots.len(), 1);
    }
}
