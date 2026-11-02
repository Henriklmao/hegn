//! Doctor modal: host checks, VM assets and setup/download actions.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{ActionId, App, FooterHit, ScrollHit};

use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::text::wrap_text;
use super::super::theme::{CYAN, GREEN, MUTED, RED, TEXT, YELLOW, centered};
use super::super::widgets::{render_scrolled, scroll_pane};

pub(crate) fn render_doctor(
    frame: &mut Frame,
    app: &App,
    modal: &crate::tui::app::DoctorModal,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Modal;
    use crate::tui::ui::test_support::*;
    use std::path::PathBuf;

    #[test]
    fn doctor_modal_wraps_the_full_failure_message() {
        let (_dir, mut app) = app();
        let report = crate::diagnostics::doctor::DoctorReport {
            ok: false,
            checks: Vec::new(),
            assets: Vec::new(),
            setup: crate::diagnostics::doctor::SetupActions::default(),
        };
        let error = format!(
            "failed to grow the instance rootfs /home/henrik/.local/share/hegn/instances/oc-test/rootfs.qcow2 ({}): END-OF-ERROR",
            "very-long-segment-".repeat(12)
        );
        app.modal = Some(Modal::Doctor(crate::tui::app::DoctorModal::with_error(
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
        let report = crate::diagnostics::doctor::DoctorReport {
            ok: false,
            checks: vec![
                crate::diagnostics::doctor::Check::new("kernel image", false, true, "missing"),
                crate::diagnostics::doctor::Check::new("lazygit", false, false, "optional"),
            ],
            assets: vec![crate::vm::images::AssetStatus {
                name: "kernel",
                path: PathBuf::from("/images/vmlinux-x86_64"),
                url: "https://example.com/vmlinux-x86_64".into(),
                sha256: "0".repeat(64),
                exists: false,
                size_bytes: None,
            }],
            setup: crate::diagnostics::doctor::SetupActions {
                download_images: true,
                ..crate::diagnostics::doctor::SetupActions::default()
            },
        };
        app.modal = Some(Modal::Doctor(crate::tui::app::DoctorModal::new(report)));
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
}
