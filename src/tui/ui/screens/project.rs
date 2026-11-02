//! Project screen layouts: compact composition, split view and section stack.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, ProjectSection, ProjectView, SectionHit};
use crate::tui::ui::RenderHits;

use super::super::theme::{ACCENT, GREEN, MUTED, TEXT, YELLOW, panel_block};
use super::git::render_git;
use super::health::render_health;
use super::instances::render_instances;
use super::projects::branch_color;
use super::snapshots::render_snapshots;
use super::terminal::{render_terminal_pane, render_terminal_strip};

pub(crate) fn render_project_panel(frame: &mut Frame, app: &App, area: Rect) {
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

pub(crate) fn render_project(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
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

pub(crate) fn render_composed_project(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
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

pub(crate) fn render_project_split(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
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

pub(crate) fn render_section_headers(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
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
pub(crate) fn render_section_stack(frame: &mut Frame, app: &App, area: Rect) -> RenderHits {
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

pub(crate) fn render_section_expanded(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Focus, Screen};
    use crate::tui::ui::test_support::*;
    use std::path::PathBuf;

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
}
