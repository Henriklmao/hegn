//! Health panel: versions, pinned assets, VM age, newest snapshot and apt state.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, Focus};

use super::super::theme::{GREEN, MUTED, TEXT, YELLOW, tab_block};

pub(crate) fn render_health(frame: &mut Frame, app: &App, area: Rect, number: usize) {
    let active = app.focus == Focus::Health;
    let block = tab_block(" health ".to_string(), number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let project = app.active_project();
    let vm = app.active_vm_summary();
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
    let age = vm
        .map(|vm| {
            let created = vm.created_at.format("%Y-%m-%d").to_string();
            if vm.state == "running" {
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
    let backup = vm
        .and_then(|vm| app.newest_snapshot_for_vm(&vm.name))
        .map(|snapshot| {
            format!(
                "{} · {}",
                snapshot.created_at.format("%Y-%m-%d %H:%M"),
                snapshot.size_label()
            )
        })
        .unwrap_or_else(|| "none".into());
    lines.push(Line::from(vec![
        Span::styled(" snapshot  ", Style::default().fg(MUTED)),
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
    if let Some(vm) = vm
        && app.apply_pending(&vm.name)
    {
        lines.push(Line::from(vec![
            Span::styled(" apply     ", Style::default().fg(MUTED)),
            Span::styled(
                format!("required · hegn vm apply {}", vm.name),
                Style::default().fg(YELLOW),
            ),
        ]));
    }
    if app.screen == crate::tui::app::Screen::Project {
        let template = app
            .project_meta
            .template
            .clone()
            .unwrap_or_else(|| "—".into());
        lines.push(Line::from(vec![
            Span::styled(" template  ", Style::default().fg(MUTED)),
            Span::styled(template, Style::default().fg(TEXT)),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

#[cfg(test)]
mod tests {
    use crate::tui::app::{App, VmSection};
    use crate::tui::ui::test_support::*;

    fn app_with_vm(name: &str) -> (tempfile::TempDir, App) {
        let (dir, mut app) = app();
        let path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        app.vms = vec![crate::vm::registry::VmSummary {
            name: name.into(),
            harness: crate::config::Harness::Shell,
            primary: Some("demo".into()),
            members: 1,
            project: path.clone(),
            workspace: path,
            created_at: chrono::Utc::now(),
            state: "running".into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.vms_dir.join(name).join("vm.toml"),
            vcpus: 2,
            memory_mib: 2048,
            error: None,
        }];
        app.open_vm_screen(name.into());
        assert_eq!(app.vm_section(), VmSection::Projects);
        (dir, app)
    }

    #[test]
    fn health_shows_a_pending_apply_only_until_the_vm_is_applied() {
        let (_dir, mut app) = app_with_vm("rust");
        app.pending_apply.insert("rust".into());

        let (text, _) = render(&app);
        assert!(text.contains("apply"), "{text}");
        assert!(text.contains("required · hegn vm apply rust"), "{text}");

        app.pending_apply.clear();
        let (text, _) = render(&app);
        assert!(!text.contains("hegn vm apply rust"), "{text}");
    }
}
