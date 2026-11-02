//! Global, instance and project settings modals.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::config::form::{ConfigField, ConfigForm, ConfigRow, FieldKind};
use crate::tui::app::{ActionId, App, ConfigStepHit, ConfigTabHit, FooterHit, RowHit, ScrollHit};

use super::super::ModalHits;
use super::super::screens::footer::{FooterDef, hint_buttons};
use super::super::text::{fit_line, tail, truncate};
use super::super::theme::{
    ACCENT, BORDER, CYAN, GREEN, MUTED, ON_ACCENT, RED, TEXT, YELLOW, centered,
};
use super::super::widgets::{render_scrollbar, scroll_pane};
use super::message::render_message;

pub(crate) const CONFIG_LABEL_WIDTH: usize = 20;
/// ` ‹ ›` plus the value column: two chevrons with breathing room.
pub(crate) const CONFIG_CONTROLS_WIDTH: usize = 4;
/// Below this list width the settings form stacks label and value.
pub(crate) const CONFIG_NARROW_WIDTH: u16 = 48;

pub(crate) fn render_config_form(
    frame: &mut Frame,
    app: &App,
    form: &ConfigForm,
    area: Rect,
) -> ModalHits {
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
pub(crate) fn config_field_lines(
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

pub(crate) fn render_project_config(
    frame: &mut Frame,
    app: &App,
    form: &crate::tui::app::ProjectConfigForm,
    area: Rect,
) -> (Vec<FooterHit>, Option<ScrollHit>) {
    let marker = |row: usize| if form.row == row { "›" } else { " " };
    let global_harness = app.config.harness;
    let harness_value = match form.harness {
        Some(harness) => harness.label().to_string(),
        None => format!("global default ({})", global_harness.label()),
    };
    let launch_value = match &form.launch {
        Some(name) if name == crate::agent::launch::HARNESS => {
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
        crate::agent::launch::Integration::Tmux,
        crate::agent::launch::Integration::Herdr,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Modal, Screen};
    use crate::tui::ui::test_support::*;
    use std::path::PathBuf;

    #[test]
    fn project_settings_modal_lists_launch_integration_and_harness() {
        let (_dir, mut app) = app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.screen = Screen::Project;
        app.open_project = Some(project.clone());
        app.modal = Some(Modal::ProjectConfig(
            crate::tui::app::ProjectConfigForm::new(
                project,
                &crate::project::ProjectMeta {
                    launch: Some(crate::agent::launch::HARNESS.into()),
                    integration: Some(crate::agent::launch::Integration::Herdr),
                    ..crate::project::ProjectMeta::default()
                },
            ),
        ));
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
    fn config_modal_renders_clickable_tabs() {
        let (_dir, mut app) = app();
        let path = app.config.state_dir.join("config.toml");
        app.modal = Some(Modal::Config(crate::config::form::ConfigForm::global(
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
                .any(|hit| hit.page == crate::config::form::ConfigPage::Profiles)
        );
        assert!(
            hits.config_tabs
                .iter()
                .any(|hit| hit.page == crate::config::form::ConfigPage::LocalLlm)
        );
    }

    #[test]
    fn config_modal_profiles_tab_renders_templates() {
        let (_dir, mut app) = app();
        let path = app.config.state_dir.join("config.toml");
        let mut form = crate::config::form::ConfigForm::global(app.config.clone(), path);
        assert!(form.set_page(crate::config::form::ConfigPage::Profiles));
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
        app.modal = Some(Modal::Config(crate::config::form::ConfigForm::global(
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
        app.modal = Some(Modal::Config(crate::config::form::ConfigForm::global(
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
        let instance = crate::vm::instance_config::InstanceConfig {
            name: "demo".into(),
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from("/projects/demo"),
            ..crate::vm::instance_config::InstanceConfig::default()
        };
        app.modal = Some(Modal::Config(crate::config::form::ConfigForm::instance(
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
}
