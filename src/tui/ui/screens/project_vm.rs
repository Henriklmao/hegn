//! Project screen `1 VM` section: state, tool profile, mount path and siblings.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, Focus};

use super::super::text::truncate;
use super::super::theme::{ACCENT, GREEN, MUTED, RED, TEXT, YELLOW, tab_block};

pub(crate) fn render_project_vm(frame: &mut Frame, app: &App, area: Rect, number: usize) {
    let active = app.focus == Focus::Vm;
    let block = tab_block(" vm ".to_string(), number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(project) = app.active_project() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no project selected",
                Style::default().fg(MUTED),
            ))),
            inner,
        );
        return;
    };
    let Some(vm_name) = app.project_vm_name(&project.path) else {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(vec![
                    Span::styled(" unassigned ", Style::default().fg(YELLOW)),
                    Span::styled("no VM owns this project", Style::default().fg(MUTED)),
                ]),
                Line::from(Span::styled(
                    " press a to pick a VM · n creates a new one",
                    Style::default().fg(MUTED),
                )),
            ])),
            inner,
        );
        return;
    };
    let summary = app.project_vm(&project.path);
    let state = summary
        .map(|summary| summary.state.as_str())
        .unwrap_or("stopped");
    let state_color = match state {
        "running" => GREEN,
        "paused" | "starting" => YELLOW,
        "failed" => RED,
        _ => MUTED,
    };
    let harness = summary
        .map(|summary| summary.harness.label().to_string())
        .unwrap_or_else(|| "—".to_string());
    let members = summary.map(|summary| summary.members).unwrap_or(0);
    let mount = app
        .project_guest_path(&project.path)
        .unwrap_or_else(|| "not exported".to_string());
    let peers: Vec<&str> = app
        .vm_members
        .iter()
        .filter(|entry| entry.vm.as_deref() == Some(vm_name))
        .map(|entry| entry.name.as_str())
        .filter(|name| {
            *name
                != app
                    .project_entry(&project.path)
                    .map(|e| e.name.as_str())
                    .unwrap_or("")
        })
        .take(4)
        .collect();

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" vm       ", Style::default().fg(MUTED)),
            Span::styled(
                format!("⬢ {vm_name}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(state, Style::default().fg(state_color)),
        ]),
        Line::from(vec![
            Span::styled(" tool     ", Style::default().fg(MUTED)),
            Span::styled(harness, Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" members  ", Style::default().fg(MUTED)),
            Span::styled(members.to_string(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" mount    ", Style::default().fg(MUTED)),
            Span::styled(mount, Style::default().fg(TEXT)),
        ]),
    ];
    if peers.is_empty() {
        lines.push(Line::from(Span::styled(
            " only member of this VM",
            Style::default().fg(MUTED),
        )));
    } else {
        lines.push(Line::from(vec![
            Span::styled(" siblings ", Style::default().fg(MUTED)),
            Span::styled(truncate(&peers.join(", "), 48), Style::default().fg(TEXT)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        " Enter attaches · v VM screen · F5 vm.toml · d detach",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
    use crate::tui::app::Screen;
    use crate::tui::ui::test_support::*;

    fn assign(app: &mut App) {
        let path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        let canonical = path.canonicalize().unwrap();
        let mut registry = ProjectRegistry::load(&app.config.state_dir).unwrap();
        registry
            .upsert(ProjectEntry {
                name: "demo".into(),
                path: canonical,
                vm: None,
                read_only: false,
                kind: ProjectKind::External,
                parent: None,
                managed: false,
                hidden: false,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        registry.assign("demo", "rust", false).unwrap();
        app.refresh_project_entries();
        app.vms = vec![crate::vm::registry::VmSummary {
            name: "rust".into(),
            harness: crate::config::Harness::Opencode,
            primary: Some("demo".into()),
            members: 2,
            project: path.clone(),
            workspace: path,
            created_at: chrono::Utc::now(),
            state: "running".into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.vms_dir.join("rust/vm.toml"),
            vcpus: 2,
            memory_mib: 2048,
            error: None,
        }];
        app.projects = vec![crate::project::Project {
            name: "demo".into(),
            path: app.config.projects_root.join("demo"),
            git: crate::project::GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.screen = Screen::Project;
        app.open_project = Some(app.config.projects_root.join("demo"));
        app.focus = Focus::Vm;
        app.refresh_project_snapshots();
    }

    #[test]
    fn project_vm_section_shows_tool_mount_and_siblings() {
        let (_dir, mut app) = app();
        let path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        app.projects = vec![crate::project::Project {
            name: "demo".into(),
            path: path.clone(),
            git: crate::project::GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.screen = Screen::Project;
        app.open_project = Some(path);
        let (text, _) = render(&app);
        assert!(text.contains("unassigned"), "{text}");
        assert!(text.contains("press a to pick a VM"), "{text}");

        assign(&mut app);
        let (text, hits) = render(&app);
        assert!(text.contains("rust"), "{text}");
        assert!(text.contains("OpenCode"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("mount"), "{text}");
        assert_eq!(hits.sections.len(), 4);
    }
}
