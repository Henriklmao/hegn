//! Bottom keybinding footer and the clickable hint rows used by modals.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{
    ActionId, App, DeleteInstanceStage, Focus, FooterHit, Modal, ProjectView, Screen, TerminalMode,
};

use super::super::theme::{ACCENT, BORDER, MUTED, ON_ACCENT};

#[derive(Clone, Copy)]
pub(crate) struct FooterDef {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) action: ActionId,
}

pub(crate) fn render_footer(frame: &mut Frame, app: &App, area: Rect) -> Vec<FooterHit> {
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

pub(crate) fn footer_buttons(app: &App) -> Vec<FooterDef> {
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
pub(crate) fn hint_buttons(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    buttons: &[FooterDef],
) -> Vec<FooterHit> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;
    use crate::tui::app::{
        App, ConfirmPrompt, DeleteInstanceModal, DeleteInstanceStage::Confirm,
        DeleteInstanceStage::Files, DeleteProjectModal, MultiAgentForm,
    };
    use crate::tui::ui::test_support::*;
    use crate::vm::instance::InstanceSummary;
    use std::path::PathBuf;

    fn actions(app: &App) -> Vec<ActionId> {
        footer_buttons(app)
            .into_iter()
            .map(|button| button.action)
            .collect()
    }

    fn labels(app: &App) -> Vec<&'static str> {
        footer_buttons(app)
            .into_iter()
            .map(|button| button.label)
            .collect()
    }

    fn instance(app: &App, name: &str, state: &str) -> InstanceSummary {
        InstanceSummary {
            name: name.into(),
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from(format!("/workspaces/{name}")),
            harness: Harness::Shell,
            ephemeral: false,
            source_instance: None,
            created_at: chrono::Utc::now(),
            state: state.into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.instances_dir.join(name).join("hegn.toml"),
            vcpus: 2,
            memory_mib: 2048,
            error: None,
        }
    }

    #[test]
    fn dashboard_footer_tracks_the_focused_panel() {
        let (_dir, mut app) = app();
        let buttons = actions(&app);
        for action in [
            ActionId::Open,
            ActionId::Create,
            ActionId::Import,
            ActionId::DeleteProject,
            ActionId::Edit,
            ActionId::GlobalConfig,
            ActionId::Doctor,
            ActionId::Refresh,
            ActionId::Help,
            ActionId::Quit,
        ] {
            assert!(buttons.contains(&action), "missing {action:?}");
        }
        assert!(!buttons.contains(&ActionId::StartInstance));

        app.focus = Focus::Instances;
        let buttons = actions(&app);
        for action in [
            ActionId::StartInstance,
            ActionId::StopInstance,
            ActionId::DeleteInstance,
            ActionId::InstanceConfig,
        ] {
            assert!(buttons.contains(&action), "missing {action:?}");
        }
        assert!(labels(&app).contains(&"Attach"));

        app.focus = Focus::Snapshots;
        let buttons = actions(&app);
        assert!(buttons.contains(&ActionId::NewSnapshot));
        assert!(buttons.contains(&ActionId::DeleteSnapshot));
        assert!(!buttons.contains(&ActionId::Open));

        app.focus = Focus::Health;
        let buttons = actions(&app);
        assert!(!buttons.contains(&ActionId::NewSnapshot));
        assert!(buttons.contains(&ActionId::GlobalConfig));
    }

    #[test]
    fn project_footer_switches_with_view_focus_and_terminal_mode() {
        let (_dir, mut app) = app();
        app.screen = Screen::Project;
        app.open_project = Some(PathBuf::from("/projects/demo"));
        app.project_view = ProjectView::Compact;
        app.focus = Focus::Instances;

        assert!(labels(&app).contains(&"Start"));

        app.instances = vec![instance(&app, "build", "running")];
        app.refresh_instance_filter();
        assert!(
            labels(&app).contains(&"Open"),
            "an existing instance is opened instead of started"
        );

        app.focus = Focus::Snapshots;
        let buttons = actions(&app);
        for action in [
            ActionId::Restore,
            ActionId::NewSnapshot,
            ActionId::DeleteSnapshot,
            ActionId::InsertMode,
        ] {
            assert!(buttons.contains(&action), "missing {action:?}");
        }

        app.project_view = ProjectView::Split;
        app.terminal_mode = TerminalMode::Normal;
        let buttons = footer_buttons(&app);
        assert!(
            buttons
                .iter()
                .any(|button| button.action == ActionId::InsertMode && button.label == "Type")
        );

        app.terminal_mode = TerminalMode::Insert;
        let buttons = footer_buttons(&app);
        assert!(
            buttons
                .iter()
                .any(|button| button.action == ActionId::NormalMode && button.label == "Normal")
        );

        app.project_view = ProjectView::Section;
        let buttons = footer_buttons(&app);
        assert!(
            buttons
                .iter()
                .any(|button| button.action == ActionId::InsertMode && button.label == "Terminal")
        );
        for action in [
            ActionId::Launch,
            ActionId::ToggleRaw,
            ActionId::ProjectConfig,
            ActionId::Back,
            ActionId::Help,
        ] {
            assert!(
                buttons.iter().any(|button| button.action == action),
                "missing {action:?}"
            );
        }
    }

    #[test]
    fn modal_footer_offers_stage_specific_destructive_choices() {
        let (_dir, mut app) = app();
        let build = instance(&app, "build", "running");

        app.modal = Some(Modal::DeleteInstance(DeleteInstanceModal {
            instance: build.clone(),
            stage: Confirm,
            error: None,
            scroll: 0,
        }));
        let mut buttons = actions(&app);
        assert_eq!(buttons, vec![ActionId::Confirm, ActionId::Cancel]);
        assert!(labels(&app).contains(&"Continue"));

        app.modal = Some(Modal::DeleteInstance(DeleteInstanceModal {
            instance: build,
            stage: Files,
            error: None,
            scroll: 0,
        }));
        buttons = actions(&app);
        assert_eq!(
            buttons,
            vec![ActionId::Confirm, ActionId::KeepFiles, ActionId::Cancel]
        );
        let labels = labels(&app);
        assert!(labels.contains(&"Delete files"));
        assert!(labels.contains(&"Keep files"));

        let project = app.selected_project().cloned().unwrap();
        app.modal = Some(Modal::DeleteProject(DeleteProjectModal {
            project,
            git: crate::project::GitSummary::default(),
            instances: vec!["build".into()],
            scroll: 0,
        }));
        assert_eq!(
            actions(&app),
            vec![
                ActionId::Confirm,
                ActionId::DeleteProjectFiles,
                ActionId::Cancel
            ]
        );

        app.modal = Some(Modal::MultiAgent(MultiAgentForm::new(
            "build".into(),
            Harness::Shell,
        )));
        assert_eq!(actions(&app), vec![ActionId::Confirm, ActionId::Cancel]);

        app.modal = Some(Modal::Confirm(ConfirmPrompt {
            title: "Delete snapshot".into(),
            message: "Delete 'demo'?".into(),
            confirm_label: "Delete".into(),
            action: crate::tui::app::ConfirmAction::DeleteSnapshot("demo".into()),
            scroll: 0,
        }));
        assert_eq!(actions(&app), vec![ActionId::Confirm, ActionId::Cancel]);
    }

    #[test]
    fn footer_buttons_stay_inside_a_narrow_terminal() {
        let (_dir, mut app) = app();
        app.focus = Focus::Instances;
        let (text, hits) = render_sized(&app, 40, 15);
        assert!(!hits.footer.is_empty(), "{text}");
        for hit in &hits.footer {
            assert!(hit.rect.x < 40, "{hit:?}");
            assert!(
                hit.rect.x + hit.rect.width <= 40,
                "footer button overflows: {hit:?}"
            );
            assert!(hit.rect.y < 15, "{hit:?}");
        }
    }

    fn footer_text(app: &App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_footer(frame, app, Rect::new(0, 0, width, height));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn footer_hint_row_reflects_search_and_screen() {
        let (_dir, mut app) = app();

        let text = footer_text(&app, 120, 4);
        assert!(text.contains("Tab cycle"), "{text}");
        assert!(text.contains("<Enter> Open"), "{text}");

        app.searching = true;
        let text = footer_text(&app, 120, 4);
        assert!(text.contains("typing filter"), "{text}");

        app.searching = false;
        app.screen = Screen::Project;
        let text = footer_text(&app, 120, 4);
        assert!(text.contains("Esc/← dashboard"), "{text}");
    }
}
