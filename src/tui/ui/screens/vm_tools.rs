//! VM screen `3 Tools`: harness, packages, resources and provisioning state.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, Focus};

use super::super::text::truncate;
use super::super::theme::{GREEN, MUTED, TEXT, YELLOW, tab_block};

pub(crate) fn render_vm_tools(frame: &mut Frame, app: &App, area: Rect, number: usize) {
    let active = app.focus == Focus::Tools;
    let block = tab_block(" tools ".to_string(), number, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(name) = app.vm_open.as_deref() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no VM open",
                Style::default().fg(MUTED),
            ))),
            inner,
        );
        return;
    };
    let summary = app.instances.iter().find(|summary| summary.name == name);
    let harness = summary
        .map(|summary| summary.harness)
        .unwrap_or(app.config.harness);
    let state = summary
        .map(|summary| summary.state.as_str())
        .unwrap_or("stopped");
    let vcpus = summary
        .map(|summary| summary.vcpus)
        .unwrap_or(app.config.vcpus);
    let memory = summary
        .map(|summary| summary.memory_mib)
        .unwrap_or(app.config.memory_mib);
    let packages = crate::vm::config::VmStore::new(&app.config)
        .load(name)
        .map(|vm| vm.extra_packages)
        .unwrap_or_default();
    let provisioning = app
        .provision
        .as_ref()
        .map(|progress| progress.stage_label().to_string())
        .unwrap_or_else(|| "idle".to_string());

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" harness  ", Style::default().fg(MUTED)),
            Span::styled(
                harness.label().to_string(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" state    ", Style::default().fg(MUTED)),
            Span::styled(
                state,
                Style::default().fg(if state == "running" { GREEN } else { YELLOW }),
            ),
        ]),
        Line::from(vec![
            Span::styled(" resources", Style::default().fg(MUTED)),
            Span::styled(
                format!(" {vcpus} vCPU · {memory} MiB"),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" packages ", Style::default().fg(MUTED)),
            Span::styled(
                if packages.is_empty() {
                    "harness preset only".to_string()
                } else {
                    truncate(&packages.join(", "), 42)
                },
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" preset   ", Style::default().fg(MUTED)),
            Span::styled(provisioning, Style::default().fg(TEXT)),
        ]),
        Line::from(Span::styled(
            " Enter shell · s start · S stop · A apply · e vm.toml",
            Style::default().fg(MUTED),
        )),
    ];
    if let Some(summary) = summary
        && let Some(log) = &summary.serial_log
    {
        lines.push(Line::from(vec![
            Span::styled(" serial   ", Style::default().fg(MUTED)),
            Span::styled(
                truncate(&log.display().to_string(), 42),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}
