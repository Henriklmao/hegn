//! Ratatui renderer. Pure functions over [`App`] make the UI testable with a
//! `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    ActionId, App, ConfigStepHit, CreateFocus, DeleteSandboxStage, Focus, FooterHit, Modal, RowHit,
    SandboxState, Screen, TerminalMode, ToastLevel,
};
use crate::config_form::{ConfigField, ConfigForm, ConfigRow, FieldKind};

// Use the terminal's own palette and default background. Only ANSI colors
// are selected here; the user's terminal theme decides how they look.
const ON_ACCENT: Color = Color::Black;
const BORDER: Color = Color::DarkGray;
const TEXT: Color = Color::Reset;
const MUTED: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;
const CYAN: Color = Color::LightCyan;
const GREEN: Color = Color::Green;
const YELLOW: Color = Color::Yellow;
const RED: Color = Color::Red;

#[derive(Debug, Default)]
pub struct UiHits {
    pub footer: Vec<FooterHit>,
    pub projects: Vec<RowHit>,
    pub sandboxes: Vec<RowHit>,
    pub snapshots: Vec<RowHit>,
    /// Settings rows and `‹ ›` steppers inside the configuration modal.
    pub config: Vec<RowHit>,
    pub config_steppers: Vec<ConfigStepHit>,
}

#[derive(Default)]
struct ModalHits {
    footer: Vec<FooterHit>,
    config: Vec<RowHit>,
    config_steppers: Vec<ConfigStepHit>,
}

impl ModalHits {
    fn buttons(footer: Vec<FooterHit>) -> Self {
        Self {
            footer,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy)]
struct FooterDef {
    key: &'static str,
    label: &'static str,
    action: ActionId,
}

pub fn draw(frame: &mut Frame, app: &App) -> UiHits {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default()), area);

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
    let mut footer = render_footer(frame, app, chunks[2]);
    let mut config = Vec::new();
    let mut config_steppers = Vec::new();
    if let Some(modal) = &app.modal {
        let modal_hits = render_modal(frame, app, modal, area);
        footer.extend(modal_hits.footer);
        config = modal_hits.config;
        config_steppers = modal_hits.config_steppers;
    }
    render_toast(frame, app, chunks[1]);

    UiHits {
        footer,
        projects: hits.0,
        sandboxes: hits.1,
        snapshots: hits.2,
        config,
        config_steppers,
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default());
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
                .fg(ON_ACCENT)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "Cloud Hypervisor sandboxes for coding agents",
            Style::default().fg(MUTED),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{tabs}]"),
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(left).style(Style::default()), inner);

    let status = match app.sandbox.state {
        SandboxState::Running => ("● running".to_string(), GREEN),
        SandboxState::Starting => (format!("{} starting", spinner(app.tick())), YELLOW),
        SandboxState::Paused => ("Ⅱ paused".to_string(), YELLOW),
        SandboxState::Failed => ("✕ failed".to_string(), RED),
        SandboxState::Stopped => ("○ stopped".to_string(), MUTED),
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

fn render_dashboard(
    frame: &mut Frame,
    app: &App,
    area: Rect,
) -> (Vec<RowHit>, Vec<RowHit>, Vec<RowHit>) {
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

fn render_sidebar(
    frame: &mut Frame,
    app: &App,
    area: Rect,
) -> (Vec<RowHit>, Vec<RowHit>, Vec<RowHit>) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(42),
            Constraint::Percentage(30),
            Constraint::Percentage(28),
        ])
        .spacing(1)
        .split(area);
    let projects = render_projects(frame, app, sections[0]);
    let sandboxes = render_sandboxes(frame, app, sections[1]);
    let snapshots = render_snapshots(frame, app, sections[2]);
    (projects, sandboxes, snapshots)
}

fn render_projects(frame: &mut Frame, app: &App, area: Rect) -> Vec<RowHit> {
    let active = app.focus == Focus::Projects;
    let title = format!(" projects ({}) ", app.projects.len());
    let block = tab_block(title, 1, active);
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
        Paragraph::new(Line::from(query_spans)).style(Style::default()),
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
                .fg(ON_ACCENT)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
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
                    ON_ACCENT
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
                Style::default().fg(ON_ACCENT).bg(ACCENT)
            } else {
                Style::default().fg(MUTED)
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
        Paragraph::new(Text::from(lines)).style(Style::default()),
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

fn render_sandboxes(frame: &mut Frame, app: &App, area: Rect) -> Vec<RowHit> {
    let active = app.focus == Focus::Sandboxes;
    let title = format!(" sandboxes ({}) ", app.sandbox_visible.len());
    let block = tab_block(title, 2, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = Vec::new();
    if app.sandbox_visible.is_empty() {
        let (headline, hint) = match app.selected_project() {
            Some(project) => (
                format!(" ▾ no sandboxes for {}", truncate(&project.name, 18)),
                "   Enter on project boots one",
            ),
            None => (
                " ▾ no project selected".to_string(),
                "   pick a project to scope it",
            ),
        };
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(headline, Style::default().fg(MUTED))),
                Line::from(Span::styled(hint, Style::default().fg(MUTED))),
            ])),
            inner,
        );
        return hits;
    }

    let visible_rows = inner.height as usize;
    let offset = app
        .sandbox_selected
        .saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, &index) in app
        .sandbox_visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
    {
        let sandbox = &app.sandboxes[index];
        let selected = row == app.sandbox_selected && active;
        let base = if selected {
            Style::default()
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let state_color = match sandbox.state.as_str() {
            "running" => GREEN,
            "paused" => YELLOW,
            "failed" => RED,
            _ => MUTED,
        };
        let marker = if sandbox.state == "running" {
            "●"
        } else {
            "○"
        };
        let badge = if sandbox.ephemeral { " clone" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(state_color)
                },
            ),
            Span::styled(truncate(&sandbox.name, 18), base),
            Span::styled(
                badge,
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(ACCENT)
                },
            ),
            Span::styled(
                format!(" {}", sandbox.harness.label()),
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(MUTED)
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
        Paragraph::new(Text::from(lines)).style(Style::default()),
        inner,
    );
    hits
}

fn render_snapshots(frame: &mut Frame, app: &App, area: Rect) -> Vec<RowHit> {
    let active = app.focus == Focus::Snapshots;
    let title = format!(" snapshots ({}) ", app.snapshots.len());
    let block = tab_block(title, 3, active);
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
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let prefix = if row == offset { " ▾ " } else { "   " };
        lines.push(Line::from(vec![
            Span::styled(prefix, style.fg(if selected { ON_ACCENT } else { CYAN })),
            Span::styled(truncate(&snapshot.name, 20), style),
            Span::styled(
                format!("  {}", snapshot.size_label()),
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(MUTED)
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
        Paragraph::new(Text::from(lines)).style(Style::default()),
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
    frame.render_widget(Paragraph::new(lines).style(Style::default()), inner);
}

fn render_sandbox_panel(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(" sandbox ".to_string(), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(sandbox) = app.selected_sandbox() else {
        let (headline, hint) = match app.selected_project() {
            Some(project) => (
                format!("No sandbox for {}.", project.name),
                "Press Enter on the project to boot one.",
            ),
            None => (
                "No project selected.".to_string(),
                "Select a project to see its sandboxes.",
            ),
        };
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(headline, Style::default().fg(TEXT))),
                Line::from(Span::styled(hint.to_string(), Style::default().fg(MUTED))),
                Line::from(Span::styled(
                    "Multi-agent clones appear in this list too.",
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    "F4 global settings · F5 sandbox settings",
                    Style::default().fg(MUTED),
                )),
            ]))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };

    let (state_color, state_label) = match sandbox.state.as_str() {
        "running" => (GREEN, "RUNNING"),
        "paused" => (YELLOW, "PAUSED"),
        "failed" => (RED, "FAILED"),
        _ => (MUTED, "STOPPED"),
    };
    let mode = if sandbox.ephemeral {
        "clone"
    } else {
        "project"
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", sandbox.name),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {mode} "),
                if sandbox.ephemeral {
                    Style::default().fg(ON_ACCENT).bg(ACCENT)
                } else {
                    Style::default().fg(MUTED)
                },
            ),
            Span::styled(
                format!("● {state_label}"),
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" project  ", Style::default().fg(MUTED)),
            Span::styled(
                truncate(&sandbox.project.display().to_string(), 48),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" workspace", Style::default().fg(MUTED)),
            Span::styled(
                truncate(&sandbox.workspace.display().to_string(), 48),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" harness  ", Style::default().fg(MUTED)),
            Span::styled(sandbox.harness.label(), Style::default().fg(TEXT)),
            Span::styled("   resources ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{} vCPU · {} MiB", sandbox.vcpus, sandbox.memory_mib),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" config   ", Style::default().fg(MUTED)),
            Span::styled(
                truncate(&sandbox.config_path.display().to_string(), 46),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(Span::styled(
            sandbox
                .source_sandbox
                .as_ref()
                .map(|source| format!("clone of {source}"))
                .unwrap_or_else(|| "Enter attach/start · S stop · d delete · F5 config".into()),
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).style(Style::default()), inner);
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
    frame.render_widget(Paragraph::new(lines).style(Style::default()), inner);
}

fn render_terminal(
    frame: &mut Frame,
    app: &App,
    area: Rect,
) -> (Vec<RowHit>, Vec<RowHit>, Vec<RowHit>) {
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
    let mode = match app.terminal_mode {
        TerminalMode::Normal => Span::styled(
            " NORMAL ",
            Style::default()
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD),
        ),
        TerminalMode::Insert => Span::styled(
            " INSERT ",
            Style::default()
                .fg(ON_ACCENT)
                .bg(GREEN)
                .add_modifier(Modifier::BOLD),
        ),
        TerminalMode::Raw => Span::styled(
            " RAW ",
            Style::default()
                .fg(ON_ACCENT)
                .bg(RED)
                .add_modifier(Modifier::BOLD),
        ),
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
        Paragraph::new(Text::from(visible)).style(Style::default()),
        out,
    );

    let input_block = panel_block(
        match app.terminal_mode {
            TerminalMode::Normal => " normal mode ".to_string(),
            TerminalMode::Insert => " insert · command (tirith-core) ".to_string(),
            TerminalMode::Raw => " raw passthrough ".to_string(),
        },
        true,
    );
    let input_inner = input_block.inner(right[2]);
    frame.render_widget(input_block, right[2]);
    if app.terminal_mode == TerminalMode::Raw {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Every key goes to the guest. F2 returns to normal, Esc leaves the project.",
                Style::default().fg(YELLOW),
            ))),
            input_inner,
        );
    } else if app.terminal_mode == TerminalMode::Normal {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "i or a to type · M multi-agent · PgUp/PgDn scroll · F2 raw · Ctrl-B dashboard",
                Style::default().fg(MUTED),
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
        .style(Style::default());
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
        let style = if app.hovered == Some(button.action) {
            // Inverted: the terminal accent becomes the text color and the
            // default background becomes the button background.
            Style::default()
                .fg(ACCENT)
                .bg(ON_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(ON_ACCENT)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(format!(" {text} "), style));
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
            " Esc/← dashboard · Ctrl-B/^] dashboard · PgUp/PgDn scroll · Ctrl-L clear ".to_string()
        } else {
            " / filter · 1-3 focus · Tab cycle · Enter open · c create · ? help ".to_string()
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
        Some(Modal::ImportProject(_)) => vec![
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
        Some(Modal::Doctor(modal)) => {
            let mut buttons = Vec::new();
            if modal.report.can_setup() && !modal.downloading && !modal.setting_up {
                buttons.push(FooterDef {
                    key: "s",
                    label: "Set up everything",
                    action: ActionId::DoctorSetup,
                });
            }
            buttons.push(FooterDef {
                key: "r",
                label: "Re-run",
                action: ActionId::Doctor,
            });
            if modal.report.can_download() && !modal.downloading {
                buttons.push(FooterDef {
                    key: "d",
                    label: "Download images",
                    action: ActionId::DownloadImages,
                });
            }
            buttons.push(FooterDef {
                key: "Esc",
                label: "Close",
                action: ActionId::Close,
            });
            buttons
        }
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
        Some(Modal::Config(_)) => vec![
            FooterDef {
                key: "Ctrl-S",
                label: "Save",
                action: ActionId::SaveConfig,
            },
            FooterDef {
                key: "e",
                label: "Edit file",
                action: ActionId::EditConfig,
            },
            FooterDef {
                key: "r",
                label: "Revert",
                action: ActionId::ReloadConfig,
            },
            FooterDef {
                key: "Esc",
                label: "Close",
                action: ActionId::Close,
            },
        ],
        Some(Modal::DeleteSandbox(modal)) => match modal.stage {
            DeleteSandboxStage::Confirm => vec![
                FooterDef {
                    key: "y",
                    label: "Continue",
                    action: ActionId::Confirm,
                },
                FooterDef {
                    key: "n",
                    label: "Cancel",
                    action: ActionId::Cancel,
                },
            ],
            DeleteSandboxStage::Files => vec![
                FooterDef {
                    key: "y",
                    label: "Delete files",
                    action: ActionId::Confirm,
                },
                FooterDef {
                    key: "n",
                    label: "Keep files",
                    action: ActionId::KeepFiles,
                },
                FooterDef {
                    key: "Esc",
                    label: "Cancel",
                    action: ActionId::Cancel,
                },
            ],
        },
        Some(Modal::MultiAgent(_)) => vec![
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
        Some(Modal::Help | Modal::Error { .. }) => {
            vec![FooterDef {
                key: "Esc",
                label: "Close",
                action: ActionId::Close,
            }]
        }
        None if app.screen == Screen::Terminal => vec![
            if app.terminal_mode == TerminalMode::Insert {
                FooterDef {
                    key: "Esc",
                    label: "Normal",
                    action: ActionId::NormalMode,
                }
            } else {
                FooterDef {
                    key: "i",
                    label: "Type",
                    action: ActionId::InsertMode,
                }
            },
            FooterDef {
                key: "M",
                label: "Multi-agent",
                action: ActionId::MultiAgent,
            },
            FooterDef {
                key: "F2",
                label: if app.terminal_mode == TerminalMode::Raw {
                    "Normal"
                } else {
                    "Raw"
                },
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
            let mut buttons = match app.focus {
                Focus::Projects => vec![
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
                        key: "h",
                        label: "Import",
                        action: ActionId::Import,
                    },
                    FooterDef {
                        key: "e",
                        label: "Editor",
                        action: ActionId::Edit,
                    },
                ],
                Focus::Sandboxes => vec![
                    FooterDef {
                        key: "Enter",
                        label: "Attach",
                        action: ActionId::StartSandbox,
                    },
                    FooterDef {
                        key: "S",
                        label: "Stop",
                        action: ActionId::StopSandbox,
                    },
                    FooterDef {
                        key: "d",
                        label: "Delete",
                        action: ActionId::DeleteSandbox,
                    },
                    FooterDef {
                        key: "F5",
                        label: "Sandbox config",
                        action: ActionId::SandboxConfig,
                    },
                ],
                Focus::Snapshots => vec![
                    FooterDef {
                        key: "n",
                        label: "New snapshot",
                        action: ActionId::NewSnapshot,
                    },
                    FooterDef {
                        key: "d",
                        label: "Delete",
                        action: ActionId::DeleteSnapshot,
                    },
                ],
            };
            buttons.push(FooterDef {
                key: "F4",
                label: "Config",
                action: ActionId::GlobalConfig,
            });
            buttons.push(FooterDef {
                key: "F3",
                label: "Doctor",
                action: ActionId::Doctor,
            });
            buttons.push(FooterDef {
                key: "r",
                label: "Refresh",
                action: ActionId::Refresh,
            });
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

/// Draw a row of clickable keybinding hints. The buttons use the same hover
/// treatment as the footer and wrap onto following rows when space runs out.
/// The returned hit boxes are merged with the footer hits so mouse clicks
/// dispatch exactly the same action as the keyboard shortcut.
fn hint_buttons(frame: &mut Frame, app: &App, area: Rect, buttons: &[FooterDef]) -> Vec<FooterHit> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let widths: Vec<u16> = buttons
        .iter()
        .map(|button| (format!("<{}> {}", button.key, button.label).width() as u16) + 2)
        .collect();
    let total = widths.iter().sum::<u16>() + buttons.len().saturating_sub(1) as u16;
    let mut x = area.x + area.width.saturating_sub(total) / 2;
    let mut y = area.y;
    let mut hits = Vec::new();
    for (button, width) in buttons.iter().zip(widths) {
        if x.saturating_add(width) > area.x + area.width {
            y += 1;
            x = area.x;
            if y >= area.y + area.height {
                break;
            }
        }
        let style = if app.hovered == Some(button.action) {
            // Inverted: the terminal accent becomes the text color and the
            // default background becomes the button background.
            Style::default()
                .fg(ACCENT)
                .bg(ON_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(ON_ACCENT)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        };
        let rect = Rect {
            x,
            y,
            width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" <{}> {} ", button.key, button.label),
                style,
            ))),
            rect,
        );
        hits.push(FooterHit {
            rect,
            action: button.action,
        });
        x += width + 1;
    }
    hits
}

fn render_modal(frame: &mut Frame, app: &App, modal: &Modal, area: Rect) -> ModalHits {
    if let Modal::Config(form) = modal {
        return render_config_form(frame, app, form, area);
    }
    ModalHits::buttons(render_modal_buttons(frame, app, modal, area))
}

fn render_modal_buttons(frame: &mut Frame, app: &App, modal: &Modal, area: Rect) -> Vec<FooterHit> {
    match modal {
        Modal::Help => render_help(frame, app, area),
        Modal::Error { title, message } => render_message(
            frame,
            app,
            area,
            60,
            30,
            1,
            title,
            message,
            RED,
            &[FooterDef {
                key: "Esc",
                label: "Close",
                action: ActionId::Close,
            }],
        ),
        Modal::Confirm(prompt) => {
            let body = format!("{}\n\nConfirm: {}", prompt.message, prompt.confirm_label);
            render_message(
                frame,
                app,
                area,
                64,
                34,
                1,
                &prompt.title,
                &body,
                YELLOW,
                &[
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
            )
        }
        Modal::CreateProject(form) => render_create_form(frame, app, form, area),
        Modal::ImportProject(form) => render_import_form(frame, app, form, area),
        Modal::Doctor(modal) => render_doctor(frame, app, modal, area),
        Modal::Security(prompt) => render_security(frame, app, prompt, area),
        // Rendered by `render_config_form` before this dispatcher.
        Modal::Config(_) => Vec::new(),
        Modal::DeleteSandbox(modal) => {
            let (body, color, buttons): (String, Color, &[FooterDef]) = match modal.stage {
                DeleteSandboxStage::Confirm => (
                    format!(
                        "Delete sandbox '{}'?\n\nProject: {}\nWorkspace: {}\n\nThe VM is stopped first.",
                        modal.sandbox.name,
                        modal.sandbox.project.display(),
                        modal.sandbox.workspace.display()
                    ),
                    YELLOW,
                    &[
                        FooterDef {
                            key: "y",
                            label: "Continue",
                            action: ActionId::Confirm,
                        },
                        FooterDef {
                            key: "n",
                            label: "Cancel",
                            action: ActionId::Cancel,
                        },
                    ],
                ),
                DeleteSandboxStage::Files => (
                    format!(
                        "Also delete the cloned workspace files?\n\n{}\n\nChoose 'Keep files' to unregister the sandbox but keep the clone.",
                        modal.sandbox.workspace.display()
                    ),
                    RED,
                    &[
                        FooterDef {
                            key: "y",
                            label: "Delete files",
                            action: ActionId::Confirm,
                        },
                        FooterDef {
                            key: "n",
                            label: "Keep files",
                            action: ActionId::KeepFiles,
                        },
                        FooterDef {
                            key: "Esc",
                            label: "Cancel",
                            action: ActionId::Cancel,
                        },
                    ],
                ),
            };
            let hits = render_message(
                frame,
                app,
                area,
                72,
                42,
                1,
                "delete sandbox",
                &body,
                color,
                buttons,
            );
            if let Some(error) = &modal.error {
                let rect = centered(area, 72, 42);
                frame.render_widget(
                    Paragraph::new(Span::styled(format!(" {error}"), Style::default().fg(RED))),
                    Rect {
                        x: rect.x + 2,
                        y: rect.y + rect.height.saturating_sub(3),
                        width: rect.width.saturating_sub(4),
                        height: 1,
                    },
                );
            }
            hits
        }
        Modal::MultiAgent(form) => {
            let body = format!(
                "Create temporary clones for parallel agents.\n\nSandbox: {}\nClones:  {}\nHarness: {}\nStart:   {}\n\nEach clone gets its own workspace; commits cannot collide.",
                form.source,
                form.count,
                form.harness.label(),
                if form.start { "yes" } else { "no" }
            );
            let hits = render_message(
                frame,
                app,
                area,
                72,
                50,
                2,
                "multi-agent clones",
                &body,
                CYAN,
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
            hits
        }
    }
}

const CONFIG_LABEL_WIDTH: usize = 20;
/// ` ‹ ›` plus the value column: two chevrons with breathing room.
const CONFIG_CONTROLS_WIDTH: usize = 4;

fn render_config_form(frame: &mut Frame, app: &App, form: &ConfigForm, area: Rect) -> ModalHits {
    let rect = centered(area, 84, 88);
    frame.render_widget(Clear, rect);
    let title = if form.dirty() {
        format!(" {} · unsaved ", form.title)
    } else {
        format!(" {} ", form.title)
    };
    let border = if form.error.is_some() { RED } else { ACCENT };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title,
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width < 24 || inner.height < 4 {
        return ModalHits::default();
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let list = chunks[0];
    let height = list.height.max(1) as usize;
    let rows = form.rows();
    let selected = form.selected_row();
    // Stateless scrolling: keep the cursor visible. The wheel moves the
    // selection, so the viewport follows automatically.
    let offset = selected.saturating_sub(height.saturating_sub(1));

    let mut lines = Vec::with_capacity(height);
    let mut config_hits = Vec::new();
    let mut steppers = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(offset).take(height) {
        let y = list.y + (index - offset) as u16;
        let row_rect = Rect {
            x: list.x,
            y,
            width: list.width,
            height: 1,
        };
        match row {
            ConfigRow::Group(name) => {
                let label = format!(" ── {name} ");
                let fill = (list.width as usize)
                    .saturating_sub(label.width())
                    .min(list.width as usize);
                lines.push(Line::from(vec![
                    Span::styled(
                        label,
                        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("─".repeat(fill), Style::default().fg(BORDER)),
                ]));
            }
            ConfigRow::Note(note) => {
                lines.push(Line::from(Span::styled(
                    truncate(&format!("   {note}"), list.width as usize),
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                )));
            }
            ConfigRow::Field(field) => {
                config_hits.push(RowHit {
                    rect: row_rect,
                    index,
                });
                let (line, mut row_steppers) =
                    config_field_line(app, form, *field, index, row_rect);
                lines.push(line);
                steppers.append(&mut row_steppers);
            }
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default()),
        list,
    );

    // File path on the left, dirty/saved marker on the right.
    let info = chunks[1];
    let marker = if form.dirty() {
        ("● unsaved", YELLOW)
    } else if form.status.is_some() {
        (form.status.as_deref().unwrap_or_default(), GREEN)
    } else {
        ("", MUTED)
    };
    let reserved = (marker.0.width() as u16).saturating_add(2);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(
                " {}",
                truncate(
                    &form.path.display().to_string(),
                    info.width.saturating_sub(reserved) as usize
                )
            ),
            Style::default().fg(MUTED),
        )),
        info,
    );
    if !marker.0.is_empty() {
        let width = marker.0.width() as u16;
        frame.render_widget(
            Paragraph::new(Span::styled(marker.0, Style::default().fg(marker.1))),
            Rect {
                x: info.x + info.width.saturating_sub(width),
                y: info.y,
                width,
                height: 1,
            },
        );
    }

    let hint = if let Some(error) = &form.error {
        Span::styled(
            format!(" {error}"),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )
    } else if form.is_editing() {
        Span::styled(
            " Enter apply · Esc cancel".to_string(),
            Style::default().fg(YELLOW),
        )
    } else {
        Span::styled(
            " ↑↓ select · ←→ or click change · Enter edit/toggle · Esc close",
            Style::default().fg(MUTED),
        )
    };
    frame.render_widget(Paragraph::new(hint), chunks[2]);

    let footer = hint_buttons(
        frame,
        app,
        chunks[3],
        &[
            FooterDef {
                key: "Ctrl-S",
                label: "Save",
                action: ActionId::SaveConfig,
            },
            FooterDef {
                key: "e",
                label: "Edit file",
                action: ActionId::EditConfig,
            },
            FooterDef {
                key: "r",
                label: "Revert",
                action: ActionId::ReloadConfig,
            },
            FooterDef {
                key: "Esc",
                label: "Close",
                action: ActionId::Close,
            },
        ],
    );

    ModalHits {
        footer,
        config: config_hits,
        config_steppers: steppers,
    }
}

/// Build one settings row and its chevron hit boxes.
fn config_field_line(
    app: &App,
    form: &ConfigForm,
    field: ConfigField,
    index: usize,
    rect: Rect,
) -> (Line<'static>, Vec<ConfigStepHit>) {
    let selected = index == form.selected_row();
    let hovered = app.config_hover == Some(index) && !selected;
    let base = if selected {
        Style::default()
            .fg(ON_ACCENT)
            .bg(CYAN)
            .add_modifier(Modifier::BOLD)
    } else if hovered {
        Style::default().fg(CYAN)
    } else {
        Style::default().fg(TEXT)
    };
    let muted = if selected {
        Style::default().fg(ON_ACCENT).bg(CYAN)
    } else if hovered {
        Style::default().fg(CYAN)
    } else {
        Style::default().fg(MUTED)
    };
    let accent = if selected {
        Style::default()
            .fg(ON_ACCENT)
            .bg(CYAN)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    };
    let editing = selected && form.editing_buffer(field).is_some();
    let value_style = if editing {
        Style::default()
            .fg(if selected { ON_ACCENT } else { YELLOW })
            .bg(if selected { CYAN } else { Color::Reset })
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        base
    };

    let width = rect.width as usize;
    let label_width = CONFIG_LABEL_WIDTH.min(width.saturating_sub(CONFIG_CONTROLS_WIDTH + 4));
    let value_width = width
        .saturating_sub(3 + label_width + CONFIG_CONTROLS_WIDTH)
        .max(2);
    let kind = field.kind();

    let raw = form.value(field);
    let display = if let Some(buffer) = form.editing_buffer(field) {
        format!("[{}▌]", tail(buffer, value_width.saturating_sub(3)))
    } else if kind == FieldKind::Bool {
        format!("[{}] {raw}", if raw == "on" { "x" } else { " " })
    } else {
        raw
    };
    let shown = truncate(&display, value_width);
    let mut value = shown.clone();
    value.push_str(&" ".repeat(value_width.saturating_sub(shown.width())));

    let label = format!("{:<label_width$}", field.label());
    let marker = if selected { "▸" } else { " " };
    let mut spans = vec![
        Span::styled(format!(" {marker} "), base),
        Span::styled(label, base),
        Span::styled(value, value_style),
    ];

    let mut steppers = Vec::new();
    let right = rect.x + rect.width.saturating_sub(1);
    match kind {
        FieldKind::Choice | FieldKind::Integer { .. } => {
            spans.push(Span::styled(" ", base));
            spans.push(Span::styled("‹", accent));
            spans.push(Span::styled(" ", base));
            spans.push(Span::styled("›", accent));
            steppers.push(ConfigStepHit {
                rect: Rect {
                    x: right.saturating_sub(2),
                    y: rect.y,
                    width: 1,
                    height: 1,
                },
                field,
                step: -1,
            });
            steppers.push(ConfigStepHit {
                rect: Rect {
                    x: right,
                    y: rect.y,
                    width: 1,
                    height: 1,
                },
                field,
                step: 1,
            });
        }
        FieldKind::Locked => spans.push(Span::styled("    ", muted)),
        FieldKind::Bool | FieldKind::Text => spans.push(Span::styled("    ", base)),
    }

    (Line::from(spans), steppers)
}

/// Last `max` display columns of `text`, so an inline editor keeps the cursor
/// visible in long paths.
fn tail(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let char_width = ch.width().unwrap_or(0);
        if width + char_width > max {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.chars().rev().collect()
}

#[allow(clippy::too_many_arguments)]
fn render_message(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    width_pct: u16,
    height_pct: u16,
    hint_rows: u16,
    title: &str,
    body: &str,
    color: Color,
    buttons: &[FooterDef],
) -> Vec<FooterHit> {
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
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(hint_rows.max(1))])
        .split(inner);
    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    hint_buttons(frame, app, chunks[1], buttons)
}

fn render_help(frame: &mut Frame, app: &App, area: Rect) -> Vec<FooterHit> {
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
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    let sections: Vec<(&str, &[(&str, &str)])> = vec![
        (
            "Projects",
            &[
                ("↑/↓ j/k", "move selection"),
                ("Enter", "open project in sandbox"),
                ("/", "fuzzy filter projects"),
                ("Tab", "switch projects → sandboxes → snapshots"),
                ("1 / 2 / 3", "jump to projects / sandboxes / snapshots"),
                ("c", "create project"),
                ("h / ←", "import existing project"),
                ("F4", "global settings (arrows/mouse · Ctrl-S saves)"),
                ("e", "open in $EDITOR"),
                ("g", "open in lazygit"),
                ("r", "refresh projects + sandboxes"),
                ("F3", "doctor (host checks, image download)"),
            ],
        ),
        (
            "Sandboxes",
            &[
                ("b", "focus sandbox list (selected project only)"),
                ("Enter", "attach/start selected sandbox or clone"),
                ("S", "stop selected sandbox"),
                ("d", "delete sandbox (double confirmation)"),
                ("F5", "sandbox settings (arrows/mouse · Ctrl-S saves)"),
                ("F4", "global settings"),
            ],
        ),
        (
            "Terminal",
            &[
                ("i / a", "insert mode — write in the guest terminal"),
                ("Esc", "leave insert mode (dashboard in normal mode)"),
                ("←", "return to dashboard (normal mode)"),
                ("M", "multi-agent clones for this project"),
                (
                    "Enter",
                    "in insert mode: inspect with tirith-core, then send",
                ),
                ("F2", "toggle raw passthrough (no interception)"),
                ("Ctrl-B / Ctrl-]", "return to dashboard"),
                ("PgUp/PgDn", "scroll output"),
                ("Ctrl-L", "clear scrollback"),
                ("security", "a allow · c checkpoint + allow · d deny"),
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
    frame.render_widget(Paragraph::new(lines).style(Style::default()), chunks[0]);
    hint_buttons(
        frame,
        app,
        chunks[1],
        &[FooterDef {
            key: "Esc",
            label: "Close",
            action: ActionId::Close,
        }],
    )
}

fn render_create_form(
    frame: &mut Frame,
    app: &App,
    form: &crate::app::CreateProjectForm,
    area: Rect,
) -> Vec<FooterHit> {
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
    frame.render_widget(Paragraph::new(pkg_lines), pkg_inner);

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
                label: "Create",
                action: ActionId::SubmitCreate,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Close,
            },
        ],
    )
}

fn render_import_form(
    frame: &mut Frame,
    app: &App,
    form: &crate::app::ImportProjectForm,
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
        .border_style(field_style(form.focus == crate::app::ImportFocus::Path))
        .title(Span::styled(
            " existing directory (~ allowed) ",
            field_style(form.focus == crate::app::ImportFocus::Path),
        ))
        .style(Style::default());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(form.path.clone(), Style::default().fg(TEXT)),
            Span::styled(
                if form.focus == crate::app::ImportFocus::Path {
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
        .border_style(field_style(form.focus == crate::app::ImportFocus::Name))
        .title(Span::styled(
            " picker name (optional) ",
            field_style(form.focus == crate::app::ImportFocus::Name),
        ))
        .style(Style::default());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(form.name.clone(), Style::default().fg(TEXT)),
            Span::styled(
                if form.focus == crate::app::ImportFocus::Name {
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

fn render_doctor(
    frame: &mut Frame,
    app: &App,
    modal: &crate::app::DoctorModal,
    area: Rect,
) -> Vec<FooterHit> {
    let rect = centered(area, 92, 82);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if modal.report.ok { GREEN } else { YELLOW }))
        .title(Span::styled(
            " doctor ",
            Style::default()
                .fg(if modal.report.ok { GREEN } else { YELLOW })
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines = Vec::new();
    if let Some(error) = &modal.error {
        lines.push(Line::from(Span::styled(
            format!(" failure: {error}"),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
    }
    for check in &modal.report.checks {
        let color = if check.ok {
            GREEN
        } else if check.required {
            RED
        } else {
            YELLOW
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", if check.ok { "✓" } else { "✗" }),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{:<18}", check.name), Style::default().fg(TEXT)),
            Span::styled(
                check.detail.clone(),
                Style::default().fg(if check.ok { MUTED } else { color }),
            ),
            Span::styled(
                if check.required { "" } else { "  (optional)" },
                Style::default().fg(MUTED),
            ),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " VM images",
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
    )));
    for asset in &modal.report.assets {
        let size = asset
            .size_bytes
            .map(|bytes| format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)))
            .unwrap_or_else(|| "missing".into());
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<8}", asset.name),
                Style::default().fg(if asset.exists { GREEN } else { RED }),
            ),
            Span::styled(
                format!("{size:<12} "),
                Style::default().fg(if asset.exists { MUTED } else { RED }),
            ),
            Span::styled(asset.path.display().to_string(), Style::default().fg(TEXT)),
        ]));
        if !asset.exists && !asset.url.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("          {}", asset.url),
                Style::default().fg(MUTED),
            )));
        }
    }
    if modal.report.can_setup() && !modal.downloading && !modal.setting_up {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " Not ready yet? Press s to prepare directories, images and the guarded network.",
            Style::default().fg(CYAN),
        )));
    }
    if modal.downloading {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " Downloading and verifying VM images…",
            Style::default().fg(CYAN),
        )));
    }
    if modal.setting_up {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " Setting up hegn… (privileged steps prompt in the terminal)",
            Style::default().fg(CYAN),
        )));
    }

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(2)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default())
            .wrap(Wrap { trim: false }),
        inner_chunks[0],
    );
    let mut buttons = Vec::new();
    if modal.report.can_setup() && !modal.downloading && !modal.setting_up {
        buttons.push(FooterDef {
            key: "s",
            label: "Set up everything",
            action: ActionId::DoctorSetup,
        });
    }
    if modal.report.can_download() && !modal.downloading {
        buttons.push(FooterDef {
            key: "d",
            label: "Download images",
            action: ActionId::DownloadImages,
        });
    }
    buttons.push(FooterDef {
        key: "r",
        label: "Re-run",
        action: ActionId::Doctor,
    });
    buttons.push(FooterDef {
        key: "Esc",
        label: "Close",
        action: ActionId::Close,
    });
    hint_buttons(frame, app, inner_chunks[1], &buttons)
}

fn render_security(
    frame: &mut Frame,
    app: &App,
    prompt: &crate::app::SecurityPrompt,
    area: Rect,
) -> Vec<FooterHit> {
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
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);

    hint_buttons(
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
    )
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
                .fg(ON_ACCENT)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .style(Style::default()),
        ),
        rect,
    );
}

fn panel_block(title: String, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if active { ACCENT } else { BORDER }))
        .title_top(Span::styled(
            title,
            Style::default()
                .fg(if active { ACCENT } else { MUTED })
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default())
}

/// A focusable sidebar tab: like [`panel_block`], but with a coloured
/// `[n]` shortcut badge in the top-right corner.
fn tab_block(title: String, number: usize, active: bool) -> Block<'static> {
    panel_block(title, active).title_top(
        Line::from(Span::styled(
            format!("[{number}] "),
            Style::default()
                .fg(if active { GREEN } else { YELLOW })
                .add_modifier(Modifier::BOLD),
        ))
        .right_aligned(),
    )
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
    use crate::config::Config;
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
        config.backend = crate::config::BackendKind::Auto;
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
        assert!(text.contains("[1]"));
        assert!(text.contains("[2]"));
        assert!(text.contains("[3]"));
        assert!(text.contains("demo"));
        assert!(text.contains("sandbox"));
        assert!(text.contains("Open"));
        assert!(!hits.footer.is_empty());
        assert_eq!(hits.projects.len(), 1);
    }

    #[test]
    fn modal_keybinding_hints_are_clickable() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Confirm(crate::app::ConfirmPrompt {
            title: "Delete snapshot".into(),
            message: "Delete 'demo'?".into(),
            confirm_label: "Delete".into(),
            action: crate::app::ConfirmAction::DeleteSnapshot("demo".into()),
        }));
        let (text, hits) = render(&app);
        assert!(text.contains("<y> Confirm"), "{text}");
        assert!(text.contains("<n> Cancel"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::Confirm)
        );
        assert!(hits.footer.iter().any(|hit| hit.action == ActionId::Cancel));

        let engine = SecurityEngine::default();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/projects/demo"),
            inspection: engine.inspect("rm -rf /workspace", None),
            error: None,
        }));
        let (text, hits) = render(&app);
        assert!(text.contains("<a> Allow once"), "{text}");
        assert!(text.contains("<d> Deny"), "{text}");
        assert!(hits.footer.iter().any(|hit| hit.action == ActionId::Allow));
        assert!(hits.footer.iter().any(|hit| hit.action == ActionId::Deny));
    }

    #[test]
    fn import_modal_renders_the_symlink_explanation() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::ImportProject(crate::app::ImportProjectForm::new()));
        let (text, hits) = render(&app);
        assert!(text.contains("import project"), "{text}");
        assert!(text.contains("Creates a symlink"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::SubmitImport)
        );
    }

    #[test]
    fn doctor_modal_renders_failures_and_missing_images() {
        let (_dir, mut app) = app();
        let report = crate::doctor::DoctorReport {
            ok: false,
            checks: vec![
                crate::doctor::Check::new("kernel image", false, true, "missing"),
                crate::doctor::Check::new("lazygit", false, false, "optional"),
            ],
            assets: vec![crate::images::AssetStatus {
                name: "kernel",
                path: PathBuf::from("/images/vmlinux-x86_64"),
                url: "https://example.com/vmlinux-x86_64".into(),
                sha256: "0".repeat(64),
                exists: false,
                size_bytes: None,
            }],
            setup: crate::doctor::SetupActions {
                download_images: true,
                ..crate::doctor::SetupActions::default()
            },
        };
        app.modal = Some(Modal::Doctor(crate::app::DoctorModal::new(report)));
        let (text, hits) = render(&app);
        assert!(text.contains("doctor"), "{text}");
        assert!(text.contains("kernel image"), "{text}");
        assert!(text.contains("Download images"), "{text}");
        assert!(text.contains("Set up everything"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::DownloadImages)
        );
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::DoctorSetup)
        );

        if let Some(Modal::Doctor(modal)) = &mut app.modal {
            modal.downloading = true;
        }
        let (_, hits) = render(&app);
        assert!(
            !hits
                .footer
                .iter()
                .any(|hit| hit.action == ActionId::DownloadImages),
            "the download button must disappear while a download is in flight"
        );
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
    fn config_modal_renders_editable_rows_and_hit_boxes() {
        let (_dir, mut app) = app();
        let path = app.config.state_dir.join("config.toml");
        app.modal = Some(Modal::Config(crate::config_form::ConfigForm::global(
            app.config.clone(),
            path,
        )));
        let (text, hits) = render(&app);
        assert!(text.contains("global settings"), "{text}");
        assert!(text.contains("Runtime"), "{text}");
        assert!(text.contains("Harness"), "{text}");
        assert!(text.contains("shell"), "{text}");
        assert!(!text.contains("OpenCode")); // only the current value is shown
        assert!(text.contains('‹'), "{text}");
        assert!(text.contains("Ctrl-S"), "{text}");
        assert!(!hits.config.is_empty(), "settings rows must be clickable");
        assert!(
            !hits.config_steppers.is_empty(),
            "choice and number rows expose steppers"
        );
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::SaveConfig)
        );
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::EditConfig)
        );
        let mut ys: Vec<u16> = hits.config.iter().map(|row| row.rect.y).collect();
        ys.sort_unstable();
        let distinct = ys.len();
        ys.dedup();
        assert_eq!(ys.len(), distinct, "every settings row gets its own line");
    }

    #[test]
    fn sandbox_config_modal_marks_read_only_context() {
        let (_dir, mut app) = app();
        let sandbox = crate::sandbox_config::SandboxConfig {
            name: "demo".into(),
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from("/projects/demo"),
            ..crate::sandbox_config::SandboxConfig::default()
        };
        app.modal = Some(Modal::Config(crate::config_form::ConfigForm::sandbox(
            sandbox,
            app.config.clone(),
        )));
        let (text, hits) = render(&app);
        assert!(text.contains("sandbox 'demo' settings"), "{text}");
        assert!(text.contains("read-only"), "{text}");
        assert!(text.contains("/projects/demo"), "{text}");
        assert!(
            hits.config.iter().any(|row| row.rect.height == 1),
            "sandbox settings must be clickable"
        );
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
        app.terminal
            .push_bytes(b"first output line\r\nsecond output line\r\n");
        let (text, _) = render(&app);
        assert!(text.contains("first output line"), "{text}");
        assert!(text.contains("second output line"), "{text}");
        assert!(text.contains("NORMAL"));
        assert!(text.contains("i or a to type"));

        app.terminal_mode = TerminalMode::Insert;
        app.input = "cargo test".into();
        let (text, _) = render(&app);
        assert!(text.contains("cargo test"));
        assert!(text.contains("INSERT"));
    }

    #[test]
    fn terminal_renders_the_full_scrollback_window() {
        let (_dir, mut app) = app();
        app.screen = Screen::Terminal;
        for index in 0..40 {
            app.terminal
                .push_bytes(format!("scrollback line {index}\r\n").as_bytes());
        }
        let (text, _) = render(&app);
        // The newest lines are visible; earlier lines scrolled out of the pane.
        assert!(text.contains("scrollback line 39"), "{text}");
        assert!(text.contains("scrollback line 38"), "{text}");
        assert!(!text.contains("scrollback line 0\n"), "{text}");
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
            dir: PathBuf::from("/snap/demo"),
            sandbox: None,
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
