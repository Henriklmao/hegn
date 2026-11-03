//! Footer and mouse action dispatch, including synthetic keys for the
//! clickable hints in modals.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

impl App {
    /// Feed a synthetic key to the multi-agent modal (used by clickable hints).
    pub(super) fn multi_agent_key(&mut self, code: KeyCode) -> Vec<Effect> {
        if matches!(self.modal, Some(Modal::MultiAgent(_))) {
            self.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        } else {
            Vec::new()
        }
    }

    /// Dispatch a footer click or a direct action (mouse support).
    pub fn handle_action(&mut self, action: ActionId) -> Vec<Effect> {
        match action {
            ActionId::Open => self.open_selected(),
            ActionId::Create => {
                self.modal = Some(Modal::CreateProject(CreateProjectForm::new(
                    self.config.harness,
                )));
                Vec::new()
            }
            ActionId::Import => {
                self.modal = Some(Modal::ImportProject(ImportProjectForm::new()));
                Vec::new()
            }
            ActionId::Confirm => match self.modal {
                Some(Modal::MultiAgent(_)) => {
                    self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                }
                Some(Modal::Confirm(_) | Modal::DeleteInstance(_) | Modal::DeleteProject(_)) => {
                    self.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                }
                _ => Vec::new(),
            },
            ActionId::Cancel => {
                if matches!(
                    self.modal,
                    Some(
                        Modal::Confirm(_)
                            | Modal::DeleteInstance(_)
                            | Modal::DeleteProject(_)
                            | Modal::MultiAgent(_)
                            | Modal::ProjectConfig(_)
                    )
                ) {
                    self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::NextField => {
                if matches!(
                    self.modal,
                    Some(Modal::CreateProject(_) | Modal::ImportProject(_))
                ) {
                    self.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::SubmitCreate => match self.modal.take() {
                Some(Modal::CreateProject(form)) => self.submit_create_project(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::SubmitImport => match self.modal.take() {
                Some(Modal::ImportProject(form)) => self.submit_import_project(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::Edit => self.effect_for_selected(Effect::OpenEditor),
            ActionId::Lazygit => self.effect_for_selected(Effect::OpenLazygit),
            ActionId::Refresh => vec![Effect::RefreshProjects, Effect::RefreshInstances],
            ActionId::Doctor => {
                self.modal = Some(Modal::Doctor(DoctorModal::new(
                    crate::diagnostics::doctor::run(&self.config),
                )));
                Vec::new()
            }
            ActionId::DoctorSetup => {
                if matches!(self.modal, Some(Modal::Doctor(_))) {
                    self.open_setup_confirmation()
                } else {
                    Vec::new()
                }
            }
            ActionId::DownloadImages => match self.modal.take() {
                Some(Modal::Doctor(mut modal)) if modal.report.can_download() => {
                    modal.downloading = true;
                    self.modal = Some(Modal::Doctor(modal));
                    vec![Effect::DownloadImages]
                }
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::Help => {
                self.modal = Some(Modal::Help(HelpState::default()));
                Vec::new()
            }
            ActionId::HelpSearch => {
                if let Some(Modal::Help(help)) = self.modal.as_mut() {
                    help.searching = true;
                }
                Vec::new()
            }
            ActionId::HelpSearchStop => {
                if let Some(Modal::Help(help)) = self.modal.as_mut() {
                    help.searching = false;
                }
                Vec::new()
            }
            ActionId::Quit => {
                self.should_quit = true;
                vec![Effect::Quit]
            }
            ActionId::Back => {
                self.leave_project();
                Vec::new()
            }
            ActionId::ToggleRaw => {
                self.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE))
            }
            ActionId::InsertMode => {
                if self.screen == Screen::Project {
                    let effects = self.open_terminal();
                    self.project_view = ProjectView::Split;
                    self.terminal_focus = true;
                    self.terminal_mode = TerminalMode::Insert;
                    self.toast("insert mode — Esc returns to normal", ToastLevel::Info);
                    effects
                } else {
                    Vec::new()
                }
            }
            ActionId::NormalMode => {
                if self.screen == Screen::Project {
                    self.terminal_mode = TerminalMode::Normal;
                    self.toast("normal mode — press i or a to type", ToastLevel::Info);
                }
                Vec::new()
            }
            ActionId::Launch => self.request_launch(None),
            ActionId::ScrollUp => {
                if self.screen == Screen::Project {
                    self.scroll = self.scroll.saturating_add(5).min(self.terminal.len());
                }
                Vec::new()
            }
            ActionId::Restore => self.confirm_restore_snapshot(),
            ActionId::DeleteSnapshot => self.confirm_delete_snapshot(),
            ActionId::FocusInstances => {
                self.focus = Focus::Instances;
                self.refresh_instances();
                Vec::new()
            }
            ActionId::StartInstance => self.open_selected(),
            ActionId::StopInstance => self.stop_selected_instance(),
            ActionId::DeleteInstance => self.confirm_delete_instance(),
            ActionId::DeleteProject => self.confirm_delete_project(),
            ActionId::DeleteProjectFiles => {
                if matches!(self.modal, Some(Modal::DeleteProject(_))) {
                    self.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::InstanceConfig => self.open_instance_config(),
            ActionId::GlobalConfig => self.open_global_config(),
            ActionId::ProjectConfig => self.open_project_config(),
            ActionId::ProjectConfigSave => match self.modal.take() {
                Some(Modal::ProjectConfig(form)) => self.save_project_config(form),
                other => {
                    self.modal = other;
                    Vec::new()
                }
            },
            ActionId::ProjectConfigLaunch => match self.modal.take() {
                Some(Modal::ProjectConfig(form)) => self.launch_project_config(form),
                other => {
                    self.modal = other;
                    Vec::new()
                }
            },
            ActionId::CloneInstance | ActionId::MultiAgent => self.open_multi_agent(),
            ActionId::KeepFiles => {
                self.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            }
            ActionId::EditConfig => match self.modal.take() {
                Some(Modal::Config(mut form)) => {
                    if form.is_editing() {
                        form.error = Some("apply or cancel the current edit first".into());
                        self.modal = Some(Modal::Config(form));
                        Vec::new()
                    } else if form.dirty() {
                        form.error = Some("save or revert before editing the raw file".into());
                        self.modal = Some(Modal::Config(form));
                        Vec::new()
                    } else {
                        vec![Effect::OpenEditor(form.path)]
                    }
                }
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::SaveConfig => match self.modal.take() {
                Some(Modal::Config(form)) => self.save_config_form(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::ReloadConfig => match self.modal.take() {
                Some(Modal::Config(form)) => self.revert_config_form(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::NewSnapshot => {
                self.toast(
                    "create snapshots with: hegn snapshot create",
                    ToastLevel::Info,
                );
                Vec::new()
            }
            ActionId::Allow => {
                if self.modal.is_some() {
                    self.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::Checkpoint => {
                if self.modal.is_some() {
                    self.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::Deny => {
                if self.modal.is_some() {
                    self.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::MultiAgentMore => self.multi_agent_key(KeyCode::Up),
            ActionId::MultiAgentFewer => self.multi_agent_key(KeyCode::Down),
            ActionId::HarnessPrev => self.multi_agent_key(KeyCode::Left),
            ActionId::HarnessNext => self.multi_agent_key(KeyCode::Right),
            ActionId::ToggleCloneStart => self.multi_agent_key(KeyCode::Char(' ')),
            ActionId::Close => {
                self.modal = None;
                self.scroll_hover = false;
                self.scroll_dragging = false;
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn footer_confirm_action_deletes_instead_of_opening_project() {
        let (_dir, mut app) = test_app();
        let paths = SnapshotStore::new(&app.config.snapshots_dir)
            .prepare("old")
            .unwrap();
        SnapshotStore::new(&app.config.snapshots_dir)
            .save_meta(&SnapshotMeta {
                name: "old".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: paths.dir,
                instance: None,
                project: None,
                size_bytes: 0,
                note: None,
            })
            .unwrap();
        app.refresh_snapshots();
        app.focus = Focus::Snapshots;
        app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));
        app.handle_action(ActionId::Confirm);
        assert!(app.modal.is_none());
        assert!(app.snapshots.is_empty());
        assert!(!app.config.snapshots_dir.join("old").exists());
    }

    #[test]
    fn footer_create_action_submits_without_discarding_input() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('c')));
        for c in "from-footer".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_action(ActionId::SubmitCreate);
        assert!(app.modal.is_none(), "{:?}", app.modal);
        assert!(app.config.projects_root.join("from-footer").is_dir());
    }

    #[test]
    fn terminal_scroll_footer_action_scrolls_instead_of_opening() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.terminal.push_bytes(b"one\ntwo\n");
        app.handle_action(ActionId::ScrollUp);
        assert!(app.scroll > 0);
        assert_eq!(app.screen, Screen::Project);
    }

    #[test]
    fn footer_help_action_opens_modal() {
        let (_dir, mut app) = test_app();
        app.handle_action(ActionId::Help);
        assert!(matches!(app.modal, Some(Modal::Help(_))));
        app.handle_action(ActionId::HelpSearch);
        assert!(matches!(&app.modal, Some(Modal::Help(help)) if help.searching));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(&app.modal, Some(Modal::Help(help)) if !help.searching));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn multi_agent_hint_actions_drive_the_form() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        app.handle_key(key(KeyCode::Char('M')));
        app.handle_action(ActionId::MultiAgentMore);
        match &app.modal {
            Some(Modal::MultiAgent(form)) => assert_eq!(form.count, 3),
            other => panic!("expected multi-agent modal, got {other:?}"),
        }
        app.handle_action(ActionId::ToggleCloneStart);
        match &app.modal {
            Some(Modal::MultiAgent(form)) => assert!(!form.start),
            other => panic!("expected multi-agent modal, got {other:?}"),
        }
    }
}
