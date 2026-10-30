//! Ratatui renderer. Pure functions over [`App`] make the UI testable with a
//! `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    ActionId, App, ConfigStepHit, ConfigTabHit, CreateFocus, DeleteInstanceStage, Focus, FooterHit,
    HelpState, InstanceState, Modal, ProjectSection, ProjectView, RowHit, Screen, ScrollHit,
    SectionHit, TerminalMode, ToastLevel,
};
use crate::config_form::{ConfigField, ConfigForm, ConfigRow, FieldKind};
use crate::images::DownloadProgress;

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
    pub instances: Vec<RowHit>,
    pub snapshots: Vec<RowHit>,
    /// Clickable project section headers/panels.
    pub sections: Vec<SectionHit>,
    /// Clickable terminal pane and collapsed terminal strip.
    pub terminal: Option<Rect>,
    pub terminal_strip: Option<Rect>,
    /// Settings rows and `‹ ›` steppers inside the configuration modal.
    pub config: Vec<RowHit>,
    pub config_steppers: Vec<ConfigStepHit>,
    pub config_tabs: Vec<ConfigTabHit>,
    /// Scrollbar track of the open modal, when its body overflows.
    pub modal_scroll: Option<ScrollHit>,
}

#[derive(Default)]
struct ModalHits {
    footer: Vec<FooterHit>,
    config: Vec<RowHit>,
    config_steppers: Vec<ConfigStepHit>,
    config_tabs: Vec<ConfigTabHit>,
    scroll: Option<ScrollHit>,
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
        Screen::Dashboard => {
            let rows = render_dashboard(frame, app, chunks[1]);
            RenderHits {
                projects: rows.0,
                instances: rows.1,
                snapshots: rows.2,
                ..RenderHits::default()
            }
        }
        Screen::Project => render_project(frame, app, chunks[1]),
    };
    let mut footer = render_footer(frame, app, chunks[2]);
    let mut config = Vec::new();
    let mut config_steppers = Vec::new();
    let mut config_tabs = Vec::new();
    let mut modal_scroll = None;
    if let Some(modal) = &app.modal {
        let modal_hits = render_modal(frame, app, modal, area);
        footer.extend(modal_hits.footer);
        config = modal_hits.config;
        config_steppers = modal_hits.config_steppers;
        config_tabs = modal_hits.config_tabs;
        modal_scroll = modal_hits.scroll;
    }
    render_toast(frame, app, chunks[1]);

    UiHits {
        footer,
        projects: hits.projects,
        instances: hits.instances,
        snapshots: hits.snapshots,
        sections: hits.sections,
        terminal: hits.terminal,
        terminal_strip: hits.terminal_strip,
        config,
        config_steppers,
        config_tabs,
        modal_scroll,
    }
}

#[derive(Default)]
struct RenderHits {
    projects: Vec<RowHit>,
    instances: Vec<RowHit>,
    snapshots: Vec<RowHit>,
    sections: Vec<SectionHit>,
    terminal: Option<Rect>,
    terminal_strip: Option<Rect>,
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let project = (app.screen == Screen::Project)
        .then(|| app.active_project())
        .flatten();
    let block = if project.is_some() {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default())
    } else {
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default())
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let left = if let Some(project) = project {
        Line::from(vec![
            Span::styled(
                " hegn ",
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ▸ ", Style::default().fg(ACCENT)),
            Span::styled(
                format!(" {} ", project.name),
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", project.display_path()),
                Style::default().fg(MUTED),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " hegn ",
                Style::default()
                    .fg(ON_ACCENT)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "[dashboard]",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(left).style(Style::default()), inner);

    let status = match app.instance.state {
        InstanceState::Running => ("● running".to_string(), GREEN),
        InstanceState::Starting => (format!("{} starting", spinner(app.tick())), YELLOW),
        InstanceState::Paused => ("Ⅱ paused".to_string(), YELLOW),
        InstanceState::Failed => ("✕ failed".to_string(), RED),
        InstanceState::Stopped => {
            if app.active_project().is_some() {
                ("○ no instance".to_string(), MUTED)
            } else {
                ("○ stopped".to_string(), MUTED)
            }
        }
    };
    let right_text = format!("{}  {} ", status.0, app.instance.backend);
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
    let projects = render_projects(frame, app, columns[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(6)])
        .spacing(1)
        .split(columns[1]);
    render_project_panel(frame, app, right[0]);
    render_dashboard_preview(frame, app, right[1]);
    (projects, Vec::new(), Vec::new())
}

fn render_dashboard_preview(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(" preview ".to_string(), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(project) = app.selected_project() else {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    "No project selected.",
                    Style::default().fg(TEXT),
                )),
                Line::from(Span::styled(
                    "Enter opens a project; c creates one.",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return;
    };

    let instances: Vec<_> = app
        .instances
        .iter()
        .filter(|instance| crate::instance::same_project(&instance.project, &project.path))
        .collect();
    let running = instances
        .iter()
        .filter(|instance| instance.state == "running")
        .count();
    let snapshots = app
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.project.as_deref() == Some(project.path.as_path()))
        .count();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" instances ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{} ({} running)", instances.len(), running),
                Style::default().fg(if running > 0 { GREEN } else { TEXT }),
            ),
        ]),
        Line::from(vec![
            Span::styled(" backups   ", Style::default().fg(MUTED)),
            Span::styled(snapshots.to_string(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" last open ", Style::default().fg(MUTED)),
            Span::styled(
                project
                    .last_opened
                    .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "never".into()),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" harness   ", Style::default().fg(MUTED)),
            Span::styled(app.config.harness.label(), Style::default().fg(TEXT)),
        ]),
        Line::from(Span::styled(
            "Enter opens the project · e editor · g lazygit",
            Style::default().fg(MUTED),
        )),
    ];
    for instance in instances.iter().take(3) {
        lines.push(Line::from(vec![
            Span::styled(
                " ● ",
                Style::default().fg(if instance.state == "running" {
                    GREEN
                } else {
                    MUTED
                }),
            ),
            Span::styled(truncate(&instance.name, 24), Style::default().fg(TEXT)),
            Span::styled(format!("  {}", instance.state), Style::default().fg(MUTED)),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
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

fn render_instances(frame: &mut Frame, app: &App, area: Rect, number: usize) -> Vec<RowHit> {
    let active = app.focus == Focus::Instances;
    let title = format!(" instances ({}) ", app.instance_visible.len());
    let block = tab_block(title, number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = Vec::new();
    if app.instance_visible.is_empty() {
        let (headline, hint) = match app.selected_project() {
            Some(project) => (
                format!(" ▾ no instances for {}", truncate(&project.name, 18)),
                "   Enter or n starts one",
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
        .instance_selected
        .saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, &index) in app
        .instance_visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
    {
        let instance = &app.instances[index];
        let selected = row == app.instance_selected && active;
        let base = if selected {
            Style::default()
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let state_color = match instance.state.as_str() {
            "running" => GREEN,
            "paused" => YELLOW,
            "failed" => RED,
            _ => MUTED,
        };
        let marker = if instance.state == "running" {
            "●"
        } else {
            "○"
        };
        let badge = if instance.ephemeral { " clone" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(state_color)
                },
            ),
            Span::styled(truncate(&instance.name, 18), base),
            Span::styled(
                badge,
                if selected {
                    Style::default().fg(ON_ACCENT).bg(CYAN)
                } else {
                    Style::default().fg(ACCENT)
                },
            ),
            Span::styled(
                format!(" {}", instance.harness.label()),
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

fn render_snapshots(frame: &mut Frame, app: &App, area: Rect, number: usize) -> Vec<RowHit> {
    let active = app.focus == Focus::Snapshots;
    let indices: Vec<usize> = if app.screen == Screen::Project {
        app.project_snapshots.clone()
    } else {
        (0..app.snapshots.len()).collect()
    };
    let selected_index = if app.screen == Screen::Project {
        app.project_snapshot_selected
    } else {
        app.selected_snapshot
    };
    let title = format!(" snapshots ({}) ", indices.len());
    let block = tab_block(title, number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = Vec::new();
    if indices.is_empty() {
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
    let offset = selected_index.saturating_sub(visible_rows.saturating_sub(1));
    let mut lines = Vec::new();
    for (row, &index) in indices.iter().enumerate().skip(offset).take(visible_rows) {
        let Some(snapshot) = app.snapshots.get(index) else {
            continue;
        };
        let selected = row == selected_index && active;
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

fn render_project(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
    match app.project_view {
        ProjectView::Compact => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(1)])
                .split(area);
            let mut hits = render_composed_project(frame, app, rows[0]);
            hits.terminal_strip = render_terminal_strip(frame, app, rows[1]);
            hits
        }
        ProjectView::Split | ProjectView::Section => render_project_split(frame, app, area),
    }
}

fn render_composed_project(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .spacing(1)
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .spacing(1)
        .split(columns[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .spacing(1)
        .split(columns[1]);

    let mut hits = RenderHits {
        instances: render_instances(frame, app, left[0], 1),
        snapshots: render_snapshots(frame, app, left[1], 2),
        ..RenderHits::default()
    };
    render_health(frame, app, right[0], 3);
    render_git(frame, app, right[1], 4);
    hits.sections = vec![
        SectionHit {
            rect: left[0],
            section: ProjectSection::Instances,
        },
        SectionHit {
            rect: left[1],
            section: ProjectSection::Snapshots,
        },
        SectionHit {
            rect: right[0],
            section: ProjectSection::Health,
        },
        SectionHit {
            rect: right[1],
            section: ProjectSection::Git,
        },
    ];
    hits
}

fn render_project_split(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
    // Keep the terminal usable on small terminals by giving it the full width.
    if area.width < 80 {
        let mut hits = RenderHits::default();
        render_terminal_pane(frame, app, area);
        hits.terminal = Some(area);
        return hits;
    }
    let (left, right) = match app.project_view {
        ProjectView::Section => (70, 30),
        _ => (45, 55),
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(left), Constraint::Percentage(right)])
        .split(area);
    let mut hits = if app.project_view == ProjectView::Section {
        render_section_stack(frame, app, columns[0])
    } else {
        render_section_headers(frame, app, columns[0])
    };
    render_terminal_pane(frame, app, columns[1]);
    hits.terminal = Some(columns[1]);
    hits
}

fn render_section_headers(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
    let block = panel_block(" project sections ".to_string(), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut hits = RenderHits::default();
    for (row, section) in ProjectSection::ALL.iter().copied().enumerate() {
        if row as u16 >= inner.height {
            break;
        }
        let count = match section {
            ProjectSection::Instances => Some(app.instance_visible.len()),
            ProjectSection::Snapshots => Some(app.project_snapshots.len()),
            ProjectSection::Health => None,
            ProjectSection::Git => None,
        };
        let active = app.focus == section.focus();
        let mut spans = vec![
            Span::styled(
                format!(" [{}] ", section.number()),
                Style::default()
                    .fg(if active { GREEN } else { YELLOW })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                section.label(),
                Style::default()
                    .fg(if active { ACCENT } else { TEXT })
                    .add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];
        if let Some(count) = count {
            spans.push(Span::styled(
                format!("  {count}"),
                Style::default().fg(MUTED),
            ));
        }
        let rect = Rect {
            x: inner.x,
            y: inner.y + row as u16,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(spans)), rect);
        hits.sections.push(SectionHit { rect, section });
    }
    hits
}

/// Section view keeps every section visible: the focused one gets the remaining
/// height, every other section collapses to its tab border and one content row.
fn render_section_stack(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
    let active = app.project_section();
    let compact = 3u16;
    let total = ProjectSection::ALL.len() as u16;
    // Too little height for four readable panels: keep the focused section
    // usable instead of drawing one-row slivers.
    if area.height < compact * total {
        return render_section_expanded(frame, app, area, active);
    }
    let reserved = compact * (total - 1);
    let expanded = area.height - reserved;
    let constraints: Vec<Constraint> = ProjectSection::ALL
        .iter()
        .map(|section| {
            Constraint::Length(if *section == active {
                expanded
            } else {
                compact
            })
        })
        .collect();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut hits = RenderHits::default();
    for (index, section) in ProjectSection::ALL.iter().copied().enumerate() {
        let rect = rows[index];
        match section {
            ProjectSection::Instances => {
                hits.instances = render_instances(frame, app, rect, section.number());
            }
            ProjectSection::Snapshots => {
                hits.snapshots = render_snapshots(frame, app, rect, section.number());
            }
            ProjectSection::Health => render_health(frame, app, rect, section.number()),
            ProjectSection::Git => render_git(frame, app, rect, section.number()),
        }
        hits.sections.push(SectionHit { rect, section });
    }
    hits
}

fn render_section_expanded(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    section: ProjectSection,
) -> RenderHits {
    let mut hits = RenderHits::default();
    match section {
        ProjectSection::Instances => {
            hits.instances = render_instances(frame, app, area, 1);
        }
        ProjectSection::Snapshots => {
            hits.snapshots = render_snapshots(frame, app, area, 2);
        }
        ProjectSection::Health => render_health(frame, app, area, 3),
        ProjectSection::Git => render_git(frame, app, area, 4),
    }
    hits.sections.push(SectionHit {
        rect: area,
        section,
    });
    hits
}

fn render_terminal_strip(frame: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    if area.height == 0 || area.width == 0 {
        return None;
    }
    let selected = app
        .selected_instance()
        .map(|instance| (instance.name.clone(), instance.state.clone()));
    let selected = selected.or_else(|| {
        app.session
            .as_ref()
            .map(|session| (session.instance_name().to_string(), "running".to_string()))
    });
    let (text, color) = match selected {
        Some((name, state)) => {
            let color = match state.as_str() {
                "running" => GREEN,
                "failed" => RED,
                _ => MUTED,
            };
            let action = if app.session.is_some() {
                "i to open · F2 attach"
            } else {
                "Enter to open · F2 attach"
            };
            (format!(" terminal · {name} · {state} · {action} "), color)
        }
        None => (
            " terminal · no instance · Enter to start ".to_string(),
            MUTED,
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))),
        area,
    );
    Some(area)
}

fn render_terminal_pane(frame: &mut Frame, app: &App, area: Rect) {
    let name = app
        .session
        .as_ref()
        .map(|session| session.instance_name().to_string())
        .or_else(|| {
            app.selected_instance()
                .map(|instance| instance.name.clone())
        })
        .unwrap_or_else(|| "no instance".into());
    let mode_label = match app.terminal_mode {
        TerminalMode::Normal => "normal",
        TerminalMode::Insert => "insert",
        TerminalMode::Raw => "raw",
    };
    let block = panel_block(
        format!(" terminal · {name} · {mode_label} "),
        app.terminal_focus,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 4 || inner.width < 16 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "terminal too small",
                Style::default().fg(MUTED),
            ))),
            inner,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(3)])
        .split(inner);
    let out = rows[0];
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
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no console output yet — start an instance with Enter",
                Style::default().fg(MUTED),
            ))),
            out,
        );
    } else {
        frame.render_widget(Paragraph::new(Text::from(visible)), out);
    }

    let input_block = panel_block(
        match app.terminal_mode {
            TerminalMode::Normal => " normal mode ".to_string(),
            TerminalMode::Insert => " insert · command (tirith-core) ".to_string(),
            TerminalMode::Raw => " raw passthrough ".to_string(),
        },
        app.terminal_focus,
    );
    let input_inner = input_block.inner(rows[1]);
    frame.render_widget(input_block, rows[1]);
    if app.terminal_mode == TerminalMode::Normal {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "i type · Esc sections · F2 attach · Ctrl-B back",
                Style::default().fg(MUTED),
            ))),
            input_inner,
        );
    } else if app.terminal_mode == TerminalMode::Raw {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "raw passthrough · F2 attach · Ctrl-B back",
                Style::default().fg(YELLOW),
            ))),
            input_inner,
        );
    } else {
        let prompt = Span::styled("❯ ", Style::default().fg(ACCENT));
        let available = input_inner.width.saturating_sub(3) as usize;
        let shown = if app.input.width() > available {
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
}

fn render_health(frame: &mut Frame, app: &App, area: Rect, number: usize) {
    let active = app.focus == Focus::Health;
    let block = tab_block(" health ".to_string(), number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let project = app.active_project();
    let instance = app.selected_instance();
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" hegn      ", Style::default().fg(MUTED)),
        Span::styled(env!("CARGO_PKG_VERSION"), Style::default().fg(TEXT)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" images    ", Style::default().fg(MUTED)),
        Span::styled(
            if app.config.vm_assets_ready() {
                "pinned · ready"
            } else {
                "missing · run doctor"
            },
            Style::default().fg(if app.config.vm_assets_ready() {
                GREEN
            } else {
                YELLOW
            }),
        ),
    ]));
    let age = instance
        .map(|instance| {
            let created = instance.created_at.format("%Y-%m-%d").to_string();
            if instance.state == "running" {
                format!("{created} · running")
            } else {
                created
            }
        })
        .or_else(|| {
            project.and_then(|project| {
                project
                    .last_opened
                    .map(|t| t.format("%Y-%m-%d").to_string())
            })
        })
        .unwrap_or_else(|| "—".into());
    lines.push(Line::from(vec![
        Span::styled(" age       ", Style::default().fg(MUTED)),
        Span::styled(age, Style::default().fg(TEXT)),
    ]));
    let backup = app
        .project_snapshots
        .first()
        .and_then(|&index| app.snapshots.get(index))
        .map(|snapshot| {
            format!(
                "{} · {}",
                snapshot.created_at.format("%Y-%m-%d %H:%M"),
                snapshot.size_label()
            )
        })
        .unwrap_or_else(|| "none".into());
    lines.push(Line::from(vec![
        Span::styled(" backup    ", Style::default().fg(MUTED)),
        Span::styled(backup, Style::default().fg(TEXT)),
    ]));
    let apt = app
        .health
        .as_ref()
        .map(|health| health.label())
        .unwrap_or_else(|| "not checked".into());
    lines.push(Line::from(vec![
        Span::styled(" apt       ", Style::default().fg(MUTED)),
        Span::styled(apt, Style::default().fg(TEXT)),
    ]));
    let template = if let Some(template) = &app.project_meta.template {
        template.clone()
    } else {
        "—".into()
    };
    lines.push(Line::from(vec![
        Span::styled(" template  ", Style::default().fg(MUTED)),
        Span::styled(
            format!(
                "{template} · {}",
                app.project_meta
                    .harness
                    .unwrap_or(app.config.harness)
                    .label()
            ),
            Style::default().fg(TEXT),
        ),
    ]));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_git(frame: &mut Frame, app: &App, area: Rect, number: usize) {
    let active = app.focus == Focus::Git;
    let block = tab_block(" git ".to_string(), number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(project) = app.active_project() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no project",
                Style::default().fg(MUTED),
            ))),
            inner,
        );
        return;
    };
    if !project.git.is_repo {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    "not a git repository",
                    Style::default().fg(YELLOW),
                )),
                Line::from(Span::styled(
                    "checkpoints are unavailable",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return;
    }
    let lines = vec![
        Line::from(vec![
            Span::styled(" branch ", Style::default().fg(MUTED)),
            Span::styled(
                project.git.badge(),
                Style::default().fg(branch_color(&project.git)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" sync   ", Style::default().fg(MUTED)),
            Span::styled(
                format!("↑{} ↓{}", project.git.ahead, project.git.behind),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" change ", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "{} staged · {} modified · {} untracked",
                    project.git.staged, project.git.modified, project.git.untracked
                ),
                Style::default().fg(TEXT),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
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
        } else if app.screen == Screen::Project {
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
        Some(Modal::DeleteInstance(modal)) => match modal.stage {
            DeleteInstanceStage::Confirm => vec![
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
            DeleteInstanceStage::Files => vec![
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
        Some(Modal::DeleteProject(_)) => vec![
            FooterDef {
                key: "y",
                label: "Remove, keep files",
                action: ActionId::Confirm,
            },
            FooterDef {
                key: "f",
                label: "Delete files",
                action: ActionId::DeleteProjectFiles,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Cancel,
            },
        ],
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
        Some(Modal::ProjectConfig(_)) => vec![
            FooterDef {
                key: "Enter",
                label: "Save",
                action: ActionId::ProjectConfigSave,
            },
            FooterDef {
                key: "r",
                label: "Save + launch",
                action: ActionId::ProjectConfigLaunch,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Cancel,
            },
        ],
        Some(Modal::Help(help)) if help.searching => vec![FooterDef {
            key: "Esc",
            label: "Stop search",
            action: ActionId::HelpSearchStop,
        }],
        Some(Modal::Help(_)) => vec![
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
        ],
        Some(Modal::Error { .. }) => {
            vec![FooterDef {
                key: "Esc",
                label: "Close",
                action: ActionId::Close,
            }]
        }
        None if app.screen == Screen::Project => {
            let mut buttons = match app.project_view {
                ProjectView::Compact => match app.focus {
                    Focus::Instances => {
                        let label = if app.selected_instance().is_some() {
                            "Open"
                        } else {
                            "Start"
                        };
                        vec![
                            FooterDef {
                                key: "Enter",
                                label,
                                action: ActionId::StartInstance,
                            },
                            FooterDef {
                                key: "i",
                                label: "Terminal",
                                action: ActionId::InsertMode,
                            },
                            FooterDef {
                                key: "S",
                                label: "Stop",
                                action: ActionId::StopInstance,
                            },
                            FooterDef {
                                key: "d",
                                label: "Delete",
                                action: ActionId::DeleteInstance,
                            },
                        ]
                    }
                    Focus::Snapshots => vec![
                        FooterDef {
                            key: "Enter",
                            label: "Restore",
                            action: ActionId::Restore,
                        },
                        FooterDef {
                            key: "n",
                            label: "New",
                            action: ActionId::NewSnapshot,
                        },
                        FooterDef {
                            key: "d",
                            label: "Delete",
                            action: ActionId::DeleteSnapshot,
                        },
                        FooterDef {
                            key: "i",
                            label: "Terminal",
                            action: ActionId::InsertMode,
                        },
                    ],
                    Focus::Health | Focus::Git | Focus::Projects => vec![FooterDef {
                        key: "i",
                        label: "Terminal",
                        action: ActionId::InsertMode,
                    }],
                },
                ProjectView::Split => {
                    if app.terminal_mode == TerminalMode::Insert {
                        vec![FooterDef {
                            key: "Esc",
                            label: "Normal",
                            action: ActionId::NormalMode,
                        }]
                    } else {
                        vec![FooterDef {
                            key: "i",
                            label: "Type",
                            action: ActionId::InsertMode,
                        }]
                    }
                }
                ProjectView::Section => vec![FooterDef {
                    key: "i",
                    label: "Terminal",
                    action: ActionId::InsertMode,
                }],
            };
            buttons.push(FooterDef {
                key: "L",
                label: "Launch",
                action: ActionId::Launch,
            });
            buttons.push(FooterDef {
                key: "F2",
                label: "Attach",
                action: ActionId::ToggleRaw,
            });
            buttons.push(FooterDef {
                key: "F6",
                label: "Project config",
                action: ActionId::ProjectConfig,
            });
            buttons.push(FooterDef {
                key: "Ctrl-B",
                label: "Back",
                action: ActionId::Back,
            });
            buttons.push(FooterDef {
                key: "?",
                label: "Help",
                action: ActionId::Help,
            });
            buttons
        }
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
                        key: "d",
                        label: "Delete project",
                        action: ActionId::DeleteProject,
                    },
                    FooterDef {
                        key: "e",
                        label: "Editor",
                        action: ActionId::Edit,
                    },
                ],
                Focus::Instances => vec![
                    FooterDef {
                        key: "Enter",
                        label: "Attach",
                        action: ActionId::StartInstance,
                    },
                    FooterDef {
                        key: "S",
                        label: "Stop",
                        action: ActionId::StopInstance,
                    },
                    FooterDef {
                        key: "d",
                        label: "Delete",
                        action: ActionId::DeleteInstance,
                    },
                    FooterDef {
                        key: "F5",
                        label: "Instance config",
                        action: ActionId::InstanceConfig,
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
                Focus::Health | Focus::Git => Vec::new(),
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
    match modal {
        Modal::Config(form) => render_config_form(frame, app, form, area),
        Modal::Help(help) => {
            let (footer, scroll) = render_help(frame, app, help, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::Doctor(modal) => {
            let (footer, scroll) = render_doctor(frame, app, modal, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::Security(prompt) => {
            let (footer, scroll) = render_security(frame, app, prompt, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::Error {
            title,
            message,
            scroll,
        } => {
            let (footer, scroll) = render_message(
                frame,
                app,
                area,
                60,
                30,
                1,
                title,
                message,
                RED,
                *scroll,
                &[FooterDef {
                    key: "Esc",
                    label: "Close",
                    action: ActionId::Close,
                }],
            );
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::Confirm(prompt) => {
            let body = format!("{}\n\nConfirm: {}", prompt.message, prompt.confirm_label);
            let (footer, scroll) = render_message(
                frame,
                app,
                area,
                64,
                34,
                1,
                &prompt.title,
                &body,
                YELLOW,
                prompt.scroll,
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
            );
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::CreateProject(form) => {
            let (footer, scroll) = render_create_form(frame, app, form, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::ImportProject(form) => {
            ModalHits::buttons(render_import_form(frame, app, form, area))
        }
        Modal::DeleteInstance(modal) => {
            let (body, color, buttons): (String, Color, &[FooterDef]) = match modal.stage {
                DeleteInstanceStage::Confirm => (
                    format!(
                        "Delete instance '{}'?\n\nProject: {}\nWorkspace: {}\n\nThe VM is stopped first.",
                        modal.instance.name,
                        modal.instance.project.display(),
                        modal.instance.workspace.display()
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
                DeleteInstanceStage::Files => (
                    format!(
                        "Also delete the cloned workspace files?\n\n{}\n\nChoose 'Keep files' to unregister the instance but keep the clone.",
                        modal.instance.workspace.display()
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
            let (footer, scroll) = render_message(
                frame,
                app,
                area,
                72,
                42,
                1,
                "delete instance",
                &body,
                color,
                modal.scroll,
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
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::DeleteProject(modal) => {
            let git = &modal.git;
            let git_line = if git.is_repo {
                format!(
                    "{} ↑{} ↓{} · {} staged · {} modified · {} untracked",
                    git.badge(),
                    git.ahead,
                    git.behind,
                    git.staged,
                    git.modified,
                    git.untracked
                )
            } else if let Some(error) = &git.error {
                format!("not a git repository ({error})")
            } else {
                "not a git repository".to_string()
            };
            let mut body = format!(
                "Delete project '{}'?\n\nPath: {}\nGit:  {}\n",
                modal.project.name,
                modal.project.path.display(),
                git_line
            );
            if git.is_dirty() {
                body.push_str(
                    "\nWARNING: uncommitted changes will be lost if the files are deleted.\n",
                );
            }
            if !modal.instances.is_empty() {
                body.push_str(&format!(
                    "\nInstances to remove: {}\n",
                    modal.instances.join(", ")
                ));
            }
            body.push_str(
                "\n[y] remove from the dashboard and keep every project file\n\
                 [f] delete the project directory as well\n\n\
                 Imported projects only lose their picker symlink; the original directory is never touched.",
            );
            let color = if git.is_dirty() { RED } else { YELLOW };
            let (footer, scroll) = render_message(
                frame,
                app,
                area,
                72,
                48,
                1,
                "delete project",
                &body,
                color,
                modal.scroll,
                &[
                    FooterDef {
                        key: "y",
                        label: "Remove, keep files",
                        action: ActionId::Confirm,
                    },
                    FooterDef {
                        key: "f",
                        label: "Delete files",
                        action: ActionId::DeleteProjectFiles,
                    },
                    FooterDef {
                        key: "Esc",
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
        Modal::MultiAgent(form) => {
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
        Modal::ProjectConfig(form) => {
            let (footer, scroll) = render_project_config(frame, app, form, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
    }
}

const CONFIG_LABEL_WIDTH: usize = 20;
/// ` ‹ ›` plus the value column: two chevrons with breathing room.
const CONFIG_CONTROLS_WIDTH: usize = 4;
/// Below this list width the settings form stacks label and value.
const CONFIG_NARROW_WIDTH: u16 = 48;

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
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let body = chunks[1];
    // Narrow terminals stack the label above the value instead of drawing a
    // label column beside a value column.
    let narrow = body.width < CONFIG_NARROW_WIDTH;
    let row_height = if narrow { 2 } else { 1 };
    let height = body.height.max(1) as usize;
    let rows = form.rows();
    let selected = form.selected_row();
    let visible_rows = (height / row_height).max(1);
    // Stateless scrolling: keep the cursor visible. The wheel moves the
    // selection, so the viewport follows automatically.
    let offset = selected.saturating_sub(visible_rows.saturating_sub(1));
    let pane = scroll_pane(body, rows.len() * row_height, offset * row_height);
    let list = pane.content;

    // Page tabs. Every page is a click target; `[`/`]` cycles them.
    let mut config_tabs = Vec::new();
    let mut tab_spans = Vec::new();
    let mut tab_x = chunks[0].x;
    for (index, page) in form.pages().iter().enumerate() {
        if index > 0 {
            tab_spans.push(Span::raw(" "));
            tab_x = tab_x.saturating_add(1);
        }
        let label = format!(" {} ", page.label());
        let width = (label.width() as u16).min(chunks[0].width.saturating_sub(tab_x - chunks[0].x));
        if width == 0 {
            break;
        }
        let style = if *page == form.page() {
            Style::default()
                .fg(ON_ACCENT)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        tab_spans.push(Span::styled(label.clone(), style));
        config_tabs.push(ConfigTabHit {
            rect: Rect {
                x: tab_x,
                y: chunks[0].y,
                width,
                height: 1,
            },
            page: *page,
        });
        tab_x = tab_x.saturating_add(width);
    }
    frame.render_widget(
        Paragraph::new(Line::from(tab_spans)).style(Style::default()),
        chunks[0],
    );

    let mut lines = Vec::with_capacity(visible_rows * row_height);
    let mut config_hits = Vec::new();
    let mut steppers = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(offset).take(visible_rows) {
        let y = list.y + ((index - offset) * row_height) as u16;
        let row_rect = Rect {
            x: list.x,
            y,
            width: list.width,
            height: row_height as u16,
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
                if narrow {
                    lines.push(Line::default());
                }
            }
            ConfigRow::Note(note) => {
                lines.push(Line::from(Span::styled(
                    truncate(&format!("   {note}"), list.width as usize),
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                )));
                if narrow {
                    lines.push(Line::default());
                }
            }
            ConfigRow::Field(field) => {
                config_hits.push(RowHit {
                    rect: row_rect,
                    index,
                });
                let (field_lines, mut row_steppers) =
                    config_field_lines(app, form, *field, index, row_rect, narrow);
                lines.extend(field_lines);
                steppers.append(&mut row_steppers);
            }
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default()),
        list,
    );
    render_scrollbar(frame, app, &pane);

    // File path on the left, dirty/saved marker on the right.
    let info = chunks[2];
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
        let pages_hint = if form.pages().len() > 1 {
            " · [ ] page"
        } else {
            ""
        };
        Span::styled(
            format!(" ↑↓ select · ←→ or click change · Enter edit/toggle{pages_hint} · Esc close"),
            Style::default().fg(MUTED),
        )
    };
    frame.render_widget(
        Paragraph::new(fit_line(Line::from(hint), chunks[3].width as usize)),
        chunks[3],
    );

    let footer = hint_buttons(
        frame,
        app,
        chunks[4],
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
        config_tabs,
        scroll: pane.hit,
    }
}

/// Build a settings row (or two stacked lines on narrow terminals) and its
/// chevron hit boxes.
fn config_field_lines(
    app: &App,
    form: &ConfigForm,
    field: ConfigField,
    index: usize,
    rect: Rect,
    narrow: bool,
) -> (Vec<Line<'static>>, Vec<ConfigStepHit>) {
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
    let value_width = if narrow {
        width.saturating_sub(3 + CONFIG_CONTROLS_WIDTH).max(2)
    } else {
        width
            .saturating_sub(3 + label_width + CONFIG_CONTROLS_WIDTH)
            .max(2)
    };
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

    let marker = if selected { "▸" } else { " " };
    let mut spans = if narrow {
        vec![
            Span::styled("   ", base),
            Span::styled(value.clone(), value_style),
        ]
    } else {
        vec![
            Span::styled(format!(" {marker} "), base),
            Span::styled(format!("{:<label_width$}", field.label()), base),
            Span::styled(value, value_style),
        ]
    };

    let mut steppers = Vec::new();
    let right = rect.x + rect.width.saturating_sub(1);
    let value_y = if narrow { rect.y + 1 } else { rect.y };
    match kind {
        FieldKind::Choice | FieldKind::Integer { .. } => {
            spans.push(Span::styled(" ", base));
            spans.push(Span::styled("‹", accent));
            spans.push(Span::styled(" ", base));
            spans.push(Span::styled("›", accent));
            steppers.push(ConfigStepHit {
                rect: Rect {
                    x: right.saturating_sub(2),
                    y: value_y,
                    width: 1,
                    height: 1,
                },
                field,
                step: -1,
            });
            steppers.push(ConfigStepHit {
                rect: Rect {
                    x: right,
                    y: value_y,
                    width: 1,
                    height: 1,
                },
                field,
                step: 1,
            });
        }
        FieldKind::Locked => spans.push(Span::styled("    ", muted)),
        FieldKind::Action => spans.push(Span::styled("  ↵ ", accent)),
        FieldKind::Bool | FieldKind::Text => spans.push(Span::styled("    ", base)),
    }

    let lines = if narrow {
        vec![
            Line::from(vec![
                Span::styled(format!(" {marker} "), base),
                Span::styled(field.label().to_string(), base),
            ]),
            Line::from(spans),
        ]
    } else {
        vec![Line::from(spans)]
    };
    (lines, steppers)
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

fn render_project_config(
    frame: &mut Frame,
    app: &App,
    form: &crate::app::ProjectConfigForm,
    area: Rect,
) -> (Vec<FooterHit>, Option<ScrollHit>) {
    let marker = |row: usize| if form.row == row { "›" } else { " " };
    let global_harness = app.config.harness;
    let harness_value = match form.harness {
        Some(harness) => harness.label().to_string(),
        None => format!("global default ({})", global_harness.label()),
    };
    let launch_value = match &form.launch {
        Some(name) if name == crate::launch::HARNESS => {
            format!(
                "harness ({})",
                form.harness.unwrap_or(global_harness).label()
            )
        }
        Some(name) => name.clone(),
        None => match &app.config.launch.default {
            Some(default) => format!("global default ({default})"),
            None => "global default (none)".to_string(),
        },
    };
    let integration_value = match form.integration {
        None => format!("global default ({})", app.config.launch.integration.label()),
        Some(integration) => match integration.resolve() {
            resolved if resolved != integration => {
                format!("{} → {}", integration.label(), resolved.label())
            }
            _ => integration.label().to_string(),
        },
    };
    let auto_start_value = match form.auto_start {
        Some(true) => "on".to_string(),
        Some(false) => "off".to_string(),
        None => format!(
            "global default ({})",
            if app.config.launch.auto_start {
                "on"
            } else {
                "off"
            }
        ),
    };
    let availability: Vec<String> = [
        crate::launch::Integration::Tmux,
        crate::launch::Integration::Herdr,
    ]
    .into_iter()
    .map(|integration| {
        format!(
            "{} {}",
            integration.label(),
            if integration.available() {
                "available"
            } else {
                "unavailable"
            }
        )
    })
    .collect();
    let mut body = format!(
        "Project  {}\nFile     {}\n\n\
         {} Launch command   {}\n\
         {} Integration      {}\n\
         {} Auto-launch      {}\n\
         {} Harness          {}\n\n\
         ↑/↓ row · ←/→ value · Enter save · r save + launch\n\
         {}\n\
         Inside tmux, Ctrl-B is the tmux prefix — Ctrl-] detaches hegn.",
        form.project.display(),
        form.project
            .join(crate::project::PROJECT_META_FILE)
            .display(),
        marker(0),
        launch_value,
        marker(1),
        integration_value,
        marker(2),
        auto_start_value,
        marker(3),
        harness_value,
        availability.join(" · "),
    );
    if let Some(error) = &form.error {
        body.push_str(&format!("\n\n{error}"));
    }
    let (footer, scroll) = render_message(
        frame,
        app,
        area,
        76,
        66,
        1,
        "project settings",
        &body,
        CYAN,
        0,
        &[
            FooterDef {
                key: "Enter",
                label: "Save",
                action: ActionId::ProjectConfigSave,
            },
            FooterDef {
                key: "r",
                label: "Save + launch",
                action: ActionId::ProjectConfigLaunch,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Cancel,
            },
        ],
    );
    (footer, scroll)
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
    offset: usize,
    buttons: &[FooterDef],
) -> (Vec<FooterHit>, Option<ScrollHit>) {
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
    let width = chunks[0].width as usize;
    let lines: Vec<Line<'static>> = body
        .lines()
        .flat_map(|line| wrap_text(line, width))
        .map(|line| Line::from(Span::styled(line, Style::default().fg(TEXT))))
        .collect();
    let pane = scroll_pane(chunks[0], lines.len(), offset);
    render_scrolled(frame, app, &pane, lines);
    let footer = hint_buttons(frame, app, chunks[1], buttons);
    (footer, pane.hit)
}

fn render_help(
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

fn render_create_form(
    frame: &mut Frame,
    app: &App,
    form: &crate::app::CreateProjectForm,
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
) -> (Vec<FooterHit>, Option<ScrollHit>) {
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
        // Wrap instead of ellipsizing: the failure usually contains a long path
        // plus the real cause, and the modal scrolls, so there is room for it.
        let prefix = " failure: ";
        // Leave the rightmost column for the scrollbar when the modal scrolls.
        let width = (inner.width.saturating_sub(1) as usize)
            .saturating_sub(prefix.width())
            .max(1);
        for (index, chunk) in wrap_text(error, width).into_iter().enumerate() {
            let lead = if index == 0 { prefix } else { "          " };
            lines.push(Line::from(Span::styled(
                format!("{lead}{chunk}"),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            )));
        }
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
    let pane = scroll_pane(inner_chunks[0], lines.len(), modal.scroll);
    render_scrolled(frame, app, &pane, lines);
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
    (
        hint_buttons(frame, app, inner_chunks[1], &buttons),
        pane.hit,
    )
}

fn render_security(
    frame: &mut Frame,
    app: &App,
    prompt: &crate::app::SecurityPrompt,
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

fn render_toast(frame: &mut Frame, app: &App, area: Rect) {
    let toast = render_toast_notification(frame, app, area);
    let mut bottom = toast.map_or(area.y + area.height, |rect| rect.y);
    if let Some(progress) = &app.provision
        && let Some(rect) = render_provision_progress(frame, progress, app.tick(), area, bottom)
    {
        bottom = rect.y;
    }
    if let Some(progress) = &app.download {
        render_download_progress(frame, progress, app.tick(), area, bottom);
    }
}

/// Progress notification for the first boot's package installation.
fn render_provision_progress(
    frame: &mut Frame,
    progress: &crate::provision::ProvisionProgress,
    tick: u64,
    area: Rect,
    bottom: u16,
) -> Option<Rect> {
    let height = 4;
    if bottom < area.y + height || area.width < 28 {
        return None;
    }
    let label = format!(
        " setting up {} · {} · {} ",
        progress.harness_label(),
        progress.stage_label(),
        progress.eta_label()
    );
    let width = (label.width() as u16 + 4)
        .min(area.width.saturating_sub(4))
        .max(28);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width + 2),
        y: bottom - height,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return Some(rect);
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&label, inner.width as usize),
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rows[0],
    );
    render_progress_bar(frame, rows[1], Some(progress.fraction()), tick);
    Some(rect)
}

/// Draw the transient toast, returning its rectangle so a download
/// notification can stack above it.
fn render_toast_notification(frame: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    let toast = app.toast.as_ref()?;
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
    Some(rect)
}

/// Bottom-right notification for an in-flight VM image download: the asset
/// name, its completion and a bar that fills as bytes arrive.
fn render_download_progress(
    frame: &mut Frame,
    progress: &DownloadProgress,
    tick: u64,
    area: Rect,
    bottom: u16,
) {
    let height = 4;
    if bottom < area.y + height || area.width < 24 {
        return;
    }
    let fraction = progress.fraction();
    let detail = match fraction {
        Some(fraction) => format!("{}%", (fraction * 100.0).floor() as u16),
        None => format!("{} MiB", progress.received / (1024 * 1024)),
    };
    let label = format!(" downloading {} · {detail} ", progress.asset);
    let width = (label.width() as u16 + 4)
        .min(area.width.saturating_sub(4))
        .max(24);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width + 2),
        y: bottom - height,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rows[0],
    );
    render_progress_bar(frame, rows[1], fraction, tick);
}

/// Fill `area` with a bar: exact when `fraction` is known, a sliding segment
/// while the server has not advertised a length.
fn render_progress_bar(frame: &mut Frame, area: Rect, fraction: Option<f64>, tick: u64) {
    let width = area.width as usize;
    if width == 0 {
        return;
    }
    let (start, end) = match fraction {
        Some(fraction) => (
            0,
            (width as f64 * fraction.clamp(0.0, 1.0)).round() as usize,
        ),
        None => {
            // Unknown length: slide a segment across the empty track.
            let segment = (width / 4).max(1);
            let offset = (tick as usize / 2) % (width + segment);
            (offset.saturating_sub(segment).min(width), offset.min(width))
        }
    };
    let end = end.min(width).max(start);
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled("░".repeat(start), Style::default().fg(BORDER)));
    }
    if end > start {
        spans.push(Span::styled(
            "█".repeat(end - start),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    if end < width {
        spans.push(Span::styled(
            "░".repeat(width - end),
            Style::default().fg(BORDER),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

/// Preferred minimum size for a modal. Narrower terminals use the full width.
const MODAL_MIN_WIDTH: u16 = 48;
const MODAL_MIN_HEIGHT: u16 = 10;
/// Below this body width the help table stacks key and description.
const HELP_NARROW_WIDTH: u16 = 56;

/// Responsive modal geometry: honour the preferred percentage, but grow to the
/// available space on small terminals and never exceed the screen.
fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let margin_x = if area.width >= 60 { 2 } else { 0 };
    let margin_y = if area.height >= 16 { 1 } else { 0 };
    let max_width = area.width.saturating_sub(margin_x * 2).max(1);
    let max_height = area.height.saturating_sub(margin_y * 2).max(1);
    let preferred_width = (area.width as u32 * width_pct as u32 / 100) as u16;
    let width = if max_width <= MODAL_MIN_WIDTH {
        max_width
    } else {
        preferred_width.max(MODAL_MIN_WIDTH).min(max_width)
    };
    let preferred_height = (area.height as u32 * height_pct as u32 / 100) as u16;
    let height = preferred_height
        .max(MODAL_MIN_HEIGHT.min(max_height))
        .min(max_height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// A scrollable modal body: the content area (shrunk by the scrollbar column)
/// and the track geometry returned to the application for hover and drag.
struct ScrollPane {
    content: Rect,
    hit: Option<ScrollHit>,
    total: usize,
}

fn scroll_pane(area: Rect, total: usize, offset: usize) -> ScrollPane {
    let page = area.height as usize;
    let max = total.saturating_sub(page);
    if area.width < 6 || area.height == 0 || max == 0 {
        return ScrollPane {
            content: area,
            hit: None,
            total,
        };
    }
    let content = Rect {
        width: area.width - 1,
        ..area
    };
    ScrollPane {
        content,
        hit: Some(ScrollHit {
            rect: Rect {
                x: area.x + content.width,
                y: area.y,
                width: 1,
                height: area.height,
            },
            max,
            page,
            offset: offset.min(max),
        }),
        total,
    }
}

/// Draw the track and thumb. The thumb inverts under the pointer or while it
/// is dragged, like every other mouse-interactive element in the TUI.
fn render_scrollbar(frame: &mut Frame, app: &App, pane: &ScrollPane) {
    let Some(hit) = pane.hit else { return };
    let track = hit.rect.height as usize;
    if track == 0 {
        return;
    }
    let thumb = ((hit.page * track) / pane.total.max(1)).clamp(1, track);
    let position = hit
        .offset
        .saturating_mul(track - thumb)
        .checked_div(hit.max)
        .unwrap_or(0);
    let hovered = app.scroll_hover || app.scroll_dragging;
    for row in 0..track {
        let in_thumb = row >= position && row < position + thumb;
        let (symbol, style) = if in_thumb {
            if hovered {
                // Invert the accent instead of using the button foreground: the
                // thumb is a full block, so a dark foreground paints the whole
                // column black and the accent background never shows. The
                // terminal's default text color keeps the bar visible on the
                // track and still reads as the accent inverted.
                (
                    "█",
                    Style::default()
                        .fg(TEXT)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("█", Style::default().fg(ACCENT))
            }
        } else {
            ("│", Style::default().fg(BORDER))
        };
        frame.render_widget(
            Paragraph::new(symbol).style(style),
            Rect {
                x: hit.rect.x,
                y: hit.rect.y + row as u16,
                width: 1,
                height: 1,
            },
        );
    }
}

/// Render a scrollable body without wrapping, so one logical line is one row
/// and the scrollbar offset stays exact. Overlong lines are ellipsized.
fn render_scrolled(frame: &mut Frame, app: &App, pane: &ScrollPane, lines: Vec<Line<'static>>) {
    let width = pane.content.width as usize;
    let lines: Vec<Line<'static>> = lines
        .into_iter()
        .map(|line| fit_line(line, width))
        .collect();
    let offset = pane.hit.map_or(0, |hit| hit.offset);
    frame.render_widget(
        Paragraph::new(lines).scroll((offset.min(u16::MAX as usize) as u16, 0)),
        pane.content,
    );
    render_scrollbar(frame, app, pane);
}

/// Word-wrap plain text to `max` display columns, hard-splitting words (for
/// long paths) when a single word cannot fit.
fn wrap_text(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = word.width();
        if current_width > 0 && current_width + 1 + word_width > max {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if current_width > 0 {
            current.push(' ');
            current_width += 1;
        }
        for ch in word.chars() {
            let char_width = ch.width().unwrap_or(0);
            if current_width + char_width > max {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += char_width;
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

/// Ellipsize a styled line to `max` display columns.
fn fit_line(line: Line<'static>, max: usize) -> Line<'static> {
    if line.width() <= max {
        return line;
    }
    if max == 0 {
        return Line::default();
    }
    let style = line.style;
    let mut spans = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        let remaining = max.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let width = span.content.width();
        if width <= remaining {
            used += width;
            spans.push(span);
        } else {
            spans.push(Span::styled(truncate(&span.content, remaining), span.style));
            break;
        }
    }
    Line::from(spans).style(style)
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
    use ratatui::buffer::Buffer;
    use std::path::PathBuf;

    fn app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.projects_root = dir.path().join("projects");
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.templates_dir = dir.path().join("templates");
        config.profiles_dir = dir.path().join("profiles");
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
        render_sized(app, 120, 42)
    }

    fn render_sized(app: &App, width: u16, height: u16) -> (String, UiHits) {
        let (text, hits, _) = render_buffer(app, width, height);
        (text, hits)
    }

    fn render_buffer(app: &App, width: u16, height: u16) -> (String, UiHits, Buffer) {
        let backend = TestBackend::new(width, height);
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
        (text, hits, buffer)
    }

    #[test]
    fn header_drops_the_tagline_on_the_dashboard() {
        let (_dir, app) = app();
        let (text, _) = render(&app);
        assert!(text.contains("hegn"), "{text}");
        assert!(text.contains("[dashboard]"), "{text}");
        assert!(
            !text.contains("Cloud Hypervisor instances for coding agents"),
            "the dashboard header no longer repeats the tagline:\n{text}"
        );
    }

    #[test]
    fn image_download_shows_a_progress_notification() {
        let (_dir, mut app) = app();
        app.set_download_progress(DownloadProgress {
            asset: "rootfs",
            received: 512 * 1024 * 1024,
            total: Some(1024 * 1024 * 1024),
        });
        let (text, _) = render(&app);
        let rows: Vec<&str> = text.lines().collect();
        let label = rows
            .iter()
            .position(|row| row.contains("downloading rootfs"))
            .expect("the notification names the asset");
        assert!(rows[label].contains("50%"), "{text}");
        assert!(
            label > rows.len() / 2,
            "the notification is anchored at the bottom:\n{text}"
        );
        assert!(
            rows[label + 1].contains('█'),
            "the bar fills proportionally:\n{text}"
        );
        assert!(
            rows[label + 1].contains('░'),
            "the unfilled track stays visible:\n{text}"
        );
    }

    #[test]
    fn provisioning_progress_names_the_harness_and_stage() {
        let (_dir, mut app) = app();
        let plan =
            crate::provision::ProvisionPlan::new(crate::config::Harness::Opencode, &[]).unwrap();
        let mut progress = crate::provision::ProvisionProgress::new(&plan);
        progress.advance(crate::provision::Stage::Packages);
        app.set_provision_progress(progress);
        let (text, _) = render(&app);
        assert!(text.contains("setting up OpenCode"), "{text}");
        assert!(text.contains("installing system packages"), "{text}");
        assert!(text.contains("left"), "{text}");
        assert!(text.contains('█') && text.contains('░'), "{text}");
    }

    #[test]
    fn image_download_progress_clears_when_the_worker_reports_back() {
        let (_dir, mut app) = app();
        app.set_download_progress(DownloadProgress {
            asset: "kernel",
            received: 1024,
            total: None,
        });
        app.clear_download_progress();
        let (text, _) = render(&app);
        assert!(!text.contains("downloading kernel"), "{text}");
    }

    #[test]
    fn dashboard_renders_projects_and_preview() {
        let (_dir, app) = app();
        let (text, hits) = render(&app);
        assert!(text.contains("hegn"));
        assert!(text.contains("projects (1)"));
        assert!(text.contains("preview"));
        assert!(text.contains("instances"));
        assert!(text.contains("backups"));
        assert!(text.contains("demo"));
        assert!(!hits.footer.is_empty());
        assert_eq!(hits.projects.len(), 1);
    }

    #[test]
    fn project_screen_shows_sections_health_git_and_strip() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.open_project = Some(PathBuf::from("/projects/demo"));
        app.refresh_project_snapshots();
        let (text, hits) = render(&app);
        for needle in [
            "instances",
            "snapshots",
            "health",
            "git",
            "hegn",
            "terminal",
        ] {
            assert!(text.contains(needle), "missing {needle} in:\n{text}");
        }
        assert_eq!(hits.sections.len(), 4);
        assert!(hits.terminal_strip.is_some());
        assert!(hits.terminal.is_none());
    }

    #[test]
    fn split_project_view_renders_terminal_and_section_headers() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.open_project = Some(PathBuf::from("/projects/demo"));
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
        let (text, hits) = render(&app);
        assert!(text.contains("terminal ·"), "{text}");
        assert!(text.contains("[1]"), "{text}");
        assert!(hits.terminal.is_some());
        assert_eq!(hits.sections.len(), 4);
    }

    #[test]
    fn section_view_keeps_every_section_visible_with_the_terminal_open() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.open_project = Some(PathBuf::from("/projects/demo"));
        app.project_view = ProjectView::Section;
        app.focus = Focus::Snapshots;
        let (text, hits) = render(&app);
        for needle in [
            "[1]",
            "[2]",
            "[3]",
            "[4]",
            "instances",
            "snapshots",
            "health",
            "git",
        ] {
            assert!(text.contains(needle), "missing {needle} in:\n{text}");
        }
        assert!(hits.terminal.is_some());
        assert_eq!(hits.sections.len(), 4);
    }

    #[test]
    fn delete_project_modal_shows_git_status_and_file_choices() {
        let (_dir, mut app) = app();
        let project = app.selected_project().cloned().unwrap();
        app.modal = Some(Modal::DeleteProject(crate::app::DeleteProjectModal {
            project,
            git: GitSummary {
                is_repo: true,
                branch: Some("main".into()),
                modified: 2,
                untracked: 1,
                ..GitSummary::default()
            },
            instances: vec!["demo".into()],
            scroll: 0,
        }));
        let (text, hits) = render(&app);
        assert!(text.contains("delete project"), "{text}");
        assert!(text.contains("main"), "{text}");
        assert!(text.contains("uncommitted changes"), "{text}");
        assert!(text.contains("Remove, keep files"), "{text}");
        assert!(text.contains("Delete files"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::Confirm)
        );
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::DeleteProjectFiles)
        );
    }

    #[test]
    fn project_settings_modal_lists_launch_integration_and_harness() {
        let (_dir, mut app) = app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.screen = Screen::Project;
        app.open_project = Some(project.clone());
        app.modal = Some(Modal::ProjectConfig(crate::app::ProjectConfigForm::new(
            project,
            &crate::project::ProjectMeta {
                launch: Some(crate::launch::HARNESS.into()),
                integration: Some(crate::launch::Integration::Herdr),
                ..crate::project::ProjectMeta::default()
            },
        )));
        let (text, hits) = render(&app);
        assert!(text.contains("project settings"), "{text}");
        assert!(text.contains("Launch command"), "{text}");
        assert!(text.contains("herdr"), "{text}");
        assert!(text.contains("Save + launch"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::ProjectConfigLaunch)
        );
    }

    #[test]
    fn modal_keybinding_hints_are_clickable() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Confirm(crate::app::ConfirmPrompt {
            title: "Delete snapshot".into(),
            message: "Delete 'demo'?".into(),
            confirm_label: "Delete".into(),
            action: crate::app::ConfirmAction::DeleteSnapshot("demo".into()),
            scroll: 0,
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
            scroll: 0,
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
    fn doctor_modal_wraps_the_full_failure_message() {
        let (_dir, mut app) = app();
        let report = crate::doctor::DoctorReport {
            ok: false,
            checks: Vec::new(),
            assets: Vec::new(),
            setup: crate::doctor::SetupActions::default(),
        };
        let error = format!(
            "failed to grow the instance rootfs /home/henrik/.local/share/hegn/instances/oc-test/rootfs.qcow2 ({}): END-OF-ERROR",
            "very-long-segment-".repeat(12)
        );
        app.modal = Some(Modal::Doctor(crate::app::DoctorModal::with_error(
            report, error,
        )));
        let (text, _) = render(&app);
        assert!(text.contains("failure:"), "{text}");
        assert!(
            text.contains("END-OF-ERROR"),
            "the modal must wrap instead of ellipsizing: {text}"
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
    fn hovered_scrollbar_thumb_inverts_without_going_black() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Help(HelpState::default()));

        let (_, hits, buffer) = render_buffer(&app, 60, 24);
        let scroll = hits.modal_scroll.expect("the help list overflows");
        let idle = &buffer[(scroll.rect.x, scroll.rect.y)];
        assert_eq!(idle.symbol(), "█");
        assert_eq!(idle.fg, ACCENT, "the idle thumb is drawn in the accent");

        app.scroll_hover = true;
        let (_, hits, buffer) = render_buffer(&app, 60, 24);
        let scroll = hits.modal_scroll.expect("the help list overflows");
        let hovered = &buffer[(scroll.rect.x, scroll.rect.y)];
        assert_eq!(hovered.symbol(), "█");
        assert_eq!(
            hovered.fg, TEXT,
            "the accent is inverted into the text color"
        );
        assert_eq!(hovered.bg, ACCENT);
        assert_ne!(
            hovered.fg, ON_ACCENT,
            "a black block glyph would paint the track solid black"
        );
        assert!(
            hovered.modifier.contains(Modifier::BOLD),
            "the hovered thumb stays emphasized"
        );

        app.scroll_dragging = true;
        let (_, hits, buffer) = render_buffer(&app, 60, 24);
        let scroll = hits.modal_scroll.expect("the help list overflows");
        let dragged = &buffer[(scroll.rect.x, scroll.rect.y)];
        assert_eq!(dragged.fg, TEXT, "dragging keeps the inverted style");
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

    #[test]
    fn tiny_terminals_render_modals_without_panicking() {
        let (_dir, mut app) = app();
        let path = app.config.state_dir.join("config.toml");
        for (width, height) in [(40, 10), (24, 8), (16, 6)] {
            app.modal = Some(Modal::Help(HelpState::default()));
            let _ = render_sized(&app, width, height);
            app.modal = Some(Modal::CreateProject(crate::app::CreateProjectForm::new(
                crate::config::Harness::Shell,
            )));
            let _ = render_sized(&app, width, height);
            app.modal = Some(Modal::Config(crate::config_form::ConfigForm::global(
                app.config.clone(),
                path.clone(),
            )));
            let _ = render_sized(&app, width, height);
        }
    }

    #[test]
    fn config_modal_renders_clickable_tabs() {
        let (_dir, mut app) = app();
        let path = app.config.state_dir.join("config.toml");
        app.modal = Some(Modal::Config(crate::config_form::ConfigForm::global(
            app.config.clone(),
            path,
        )));
        let (text, hits) = render_sized(&app, 100, 40);
        assert!(text.contains("General"), "{text}");
        assert!(text.contains("Profiles"), "{text}");
        assert!(text.contains("Local LLM"), "{text}");
        assert_eq!(hits.config_tabs.len(), 3);
        assert!(
            hits.config_tabs
                .iter()
                .any(|hit| hit.page == crate::config_form::ConfigPage::Profiles)
        );
        assert!(
            hits.config_tabs
                .iter()
                .any(|hit| hit.page == crate::config_form::ConfigPage::LocalLlm)
        );
    }

    #[test]
    fn config_modal_profiles_tab_renders_templates() {
        let (_dir, mut app) = app();
        let path = app.config.state_dir.join("config.toml");
        let mut form = crate::config_form::ConfigForm::global(app.config.clone(), path);
        assert!(form.set_page(crate::config_form::ConfigPage::Profiles));
        app.modal = Some(Modal::Config(form));
        let (text, hits) = render_sized(&app, 110, 40);
        assert!(text.contains("Harness profiles"), "{text}");
        assert!(text.contains("opencode.toml"), "{text}");
        assert!(text.contains("missing"), "{text}");
        assert!(!hits.config.is_empty(), "profile rows must be clickable");
    }

    #[test]
    fn config_modal_stacks_label_and_value_when_narrow() {
        let (_dir, mut app) = app();
        let path = app.config.state_dir.join("config.toml");
        app.modal = Some(Modal::Config(crate::config_form::ConfigForm::global(
            app.config.clone(),
            path,
        )));
        let (text, hits) = render_sized(&app, 50, 24);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.iter().any(|line| line.contains("▸ Harness")),
            "the selected label gets its own line:\n{text}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("▸ Harness") && !line.contains("shell")),
            "the value moves below the label:\n{text}"
        );
        assert!(
            hits.config.iter().any(|row| row.rect.height == 2),
            "stacked settings rows are two lines tall"
        );
        assert!(hits.modal_scroll.is_some());
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
    fn instance_config_modal_marks_read_only_context() {
        let (_dir, mut app) = app();
        let instance = crate::instance_config::InstanceConfig {
            name: "demo".into(),
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from("/projects/demo"),
            ..crate::instance_config::InstanceConfig::default()
        };
        app.modal = Some(Modal::Config(crate::config_form::ConfigForm::instance(
            instance,
            app.config.clone(),
        )));
        let (text, hits) = render(&app);
        assert!(text.contains("instance 'demo' settings"), "{text}");
        assert!(text.contains("read-only"), "{text}");
        assert!(text.contains("/projects/demo"), "{text}");
        assert!(
            hits.config.iter().any(|row| row.rect.height == 1),
            "instance settings must be clickable"
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
            scroll: 0,
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
        app.screen = Screen::Project;
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
        app.terminal
            .push_bytes(b"first output line\r\nsecond output line\r\n");
        let (text, _) = render(&app);
        assert!(text.contains("first output line"), "{text}");
        assert!(text.contains("second output line"), "{text}");
        assert!(text.contains("terminal"));
        assert!(text.contains("normal"));

        app.terminal_mode = TerminalMode::Insert;
        app.input = "cargo test".into();
        let (text, _) = render(&app);
        assert!(text.contains("cargo test"));
        assert!(text.contains("insert"));
    }

    #[test]
    fn terminal_renders_the_full_scrollback_window() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.project_view = ProjectView::Split;
        app.terminal_focus = true;
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
    fn wrap_text_wraps_words_and_hard_splits_long_paths() {
        assert_eq!(wrap_text("hello world", 6), vec!["hello", "world"]);
        assert_eq!(wrap_text("", 5), vec![String::new()]);
        let path = "/very/long/path";
        let lines = wrap_text(path, 5);
        assert!(lines.iter().all(|line| line.width() <= 5), "{lines:?}");
        assert_eq!(lines.concat(), path);
    }

    #[test]
    fn snapshots_render_when_present() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.open_project = Some(PathBuf::from("/projects/demo"));
        app.snapshots = vec![crate::snapshot::SnapshotMeta {
            name: "before-refactor".into(),
            created_at: chrono::Utc::now(),
            vm_state: "Paused".into(),
            dir: PathBuf::from("/snap/demo"),
            instance: None,
            project: Some(PathBuf::from("/projects/demo")),
            size_bytes: 1024 * 1024,
            note: None,
        }];
        app.refresh_project_snapshots();
        let (text, hits) = render(&app);
        assert!(text.contains("before-refactor"));
        assert!(text.contains("1.0 MiB"));
        assert_eq!(hits.snapshots.len(), 1);
    }
}
