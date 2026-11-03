//! Modal renderer dispatch. Each modal lives in its own module; this file only
//! routes to it and assembles the shared [`ModalHits`].

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::tui::app::{ActionId, App, Modal};

use super::ModalHits;
use super::screens::footer::FooterDef;
use super::theme::{RED, YELLOW};

mod config;
mod create;
mod delete;
mod doctor;
mod help;
mod import;
mod message;
mod multi_agent;
mod security;

pub(crate) fn render_modal(frame: &mut Frame, app: &App, modal: &Modal, area: Rect) -> ModalHits {
    match modal {
        Modal::Config(form) => config::render_config_form(frame, app, form, area),
        Modal::Help(help) => {
            let (footer, scroll) = help::render_help(frame, app, help, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::Doctor(modal) => {
            let (footer, scroll) = doctor::render_doctor(frame, app, modal, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::Security(prompt) => {
            let (footer, scroll) = security::render_security(frame, app, prompt, area);
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
            let (footer, scroll) = message::render_message(
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
            let (footer, scroll) = message::render_message(
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
            let (footer, scroll) = create::render_create_form(frame, app, form, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
        Modal::ImportProject(form) => {
            ModalHits::buttons(import::render_import_form(frame, app, form, area))
        }
        Modal::DeleteInstance(modal) => delete::render_delete_instance(frame, app, modal, area),
        Modal::DeleteProject(modal) => delete::render_delete_project(frame, app, modal, area),
        Modal::MultiAgent(form) => multi_agent::render_multi_agent(frame, app, form, area),
        Modal::ProjectConfig(form) => {
            let (footer, scroll) = config::render_project_config(frame, app, form, area);
            ModalHits {
                footer,
                scroll,
                ..ModalHits::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::security::SecurityEngine;
    use crate::tui::app::{ActionId, Modal, SecurityPrompt};
    use crate::tui::ui::test_support::*;
    use std::path::PathBuf;

    #[test]
    fn modal_keybinding_hints_are_clickable() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::Confirm(crate::tui::app::ConfirmPrompt {
            title: "Delete snapshot".into(),
            message: "Delete 'demo'?".into(),
            confirm_label: "Delete".into(),
            action: crate::tui::app::ConfirmAction::DeleteSnapshot("demo".into()),
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
}
