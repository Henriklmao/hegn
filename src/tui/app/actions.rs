//! Footer and mouse action dispatch, including synthetic keys for the
//! clickable hints in modals.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

impl App {
    /// Feed a synthetic key to the fork form (used by clickable hints).
    pub(super) fn fork_key(&mut self, code: KeyCode) -> Vec<Effect> {
        if matches!(self.modal, Some(Modal::ForkWorkspace(_))) {
            self.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        } else {
            Vec::new()
        }
    }

    /// Feed a synthetic key to the assignment form.
    pub(super) fn assign_key(&mut self, code: KeyCode) -> Vec<Effect> {
        if matches!(self.modal, Some(Modal::AssignProject(_))) {
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
            ActionId::Confirm => match &self.modal {
                Some(Modal::ForkWorkspace(_) | Modal::AssignProject(_)) => {
                    self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                }
                Some(
                    Modal::Confirm(_)
                    | Modal::DeleteVm(_)
                    | Modal::DeleteProject(_)
                    | Modal::DetachProject(_),
                ) => self.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
                _ => Vec::new(),
            },
            ActionId::Cancel => {
                if matches!(
                    self.modal,
                    Some(
                        Modal::Confirm(_)
                            | Modal::DeleteVm(_)
                            | Modal::DeleteProject(_)
                            | Modal::DetachProject(_)
                            | Modal::ForkWorkspace(_)
                            | Modal::AssignProject(_)
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
                    Some(
                        Modal::CreateProject(_)
                            | Modal::ImportProject(_)
                            | Modal::CreateVm(_)
                            | Modal::ForkWorkspace(_)
                            | Modal::AssignProject(_)
                    )
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
            ActionId::SubmitVm => match self.modal.take() {
                Some(Modal::CreateVm(form)) => self.submit_create_vm(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::SubmitAssign => self.assign_key(KeyCode::Enter),
            ActionId::SubmitFork => self.fork_key(KeyCode::Enter),
            ActionId::Edit => self.effect_for_selected(Effect::OpenEditor),
            ActionId::Lazygit => self.effect_for_selected(Effect::OpenLazygit),
            ActionId::Refresh => vec![Effect::RefreshProjects, Effect::RefreshVms],
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
                if self.screen == Screen::Vm {
                    self.leave_vm();
                } else {
                    self.leave_project();
                }
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
            ActionId::FocusVm => {
                if self.screen == Screen::Vm {
                    self.focus = Focus::Projects;
                    self.refresh_vms();
                    Vec::new()
                } else {
                    self.open_selected_projects_vm()
                }
            }
            ActionId::StartVm | ActionId::Shell => self.attach_active_vm(),
            ActionId::StopVm => self.stop_selected_instance(),
            ActionId::ApplyVm => self.apply_selected_vm(),
            ActionId::DeleteVm => self.confirm_delete_vm(),
            ActionId::AssignProject => match self.screen {
                Screen::Vm => self.assign_project_to_open_vm_action(),
                Screen::Project => {
                    let Some(project) = self
                        .active_project()
                        .map(|project| project.path.clone())
                        .or_else(|| self.selected_project().map(|p| p.path.clone()))
                    else {
                        return Vec::new();
                    };
                    self.open_assign_form(project, None)
                }
                Screen::Dashboard => {
                    let Some(project) = self.selected_project().map(|p| p.path.clone()) else {
                        return Vec::new();
                    };
                    self.open_assign_form(project, None)
                }
            },
            ActionId::DetachProject => {
                if self.screen == Screen::Vm {
                    self.detach_selected_member()
                } else {
                    self.confirm_detach_project()
                }
            }
            ActionId::ForkWorkspace => {
                if self.screen == Screen::Vm {
                    self.fork_selected_member()
                } else {
                    self.open_fork_workspace()
                }
            }
            ActionId::SetPrimary => {
                self.set_selected_primary();
                Vec::new()
            }
            ActionId::NewVm => {
                self.open_create_vm_form();
                Vec::new()
            }
            ActionId::DeleteProject => self.confirm_delete_project(),
            ActionId::DeleteProjectFiles => {
                if matches!(self.modal, Some(Modal::DeleteProject(_))) {
                    self.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::VmConfig => self.open_vm_config(),
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
            ActionId::ToggleForkMode => self.fork_key(KeyCode::Tab),
            ActionId::ToggleReadOnly => self.assign_key(KeyCode::Char('r')),
            ActionId::Close => {
                self.modal = None;
                self.scroll_hover = false;
                self.scroll_dragging = false;
                Vec::new()
            }
        }
    }
}

/// Helper exposed for the footer action; the modal form is owned by the state
/// machine, so this keeps the dispatch in one place.
impl App {
    pub(super) fn assign_project_to_open_vm_action(&mut self) -> Vec<Effect> {
        let Some(vm) = self.vm_open.clone() else {
            return Vec::new();
        };
        let candidates = self.unassigned_project_candidates();
        if candidates.is_empty() {
            self.toast("no unassigned projects", ToastLevel::Warn);
            return Vec::new();
        }
        self.modal = Some(Modal::AssignProject(AssignProjectForm::for_vm(
            vm, candidates,
        )));
        Vec::new()
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
                vm: None,
                project: None,
                size_bytes: 0,
                note: None,
                ..SnapshotMeta::default()
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
    fn fork_hint_actions_drive_the_form() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.instances = vec![fake_instance(&app, "rust", project.clone())];
        assign_fake_project(&mut app, &project, "rust");
        app.projects = vec![Project {
            name: "demo".into(),
            path: project.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.screen = Screen::Project;
        app.open_project = Some(project);
        app.handle_action(ActionId::ForkWorkspace);
        assert!(matches!(app.modal, Some(Modal::ForkWorkspace(_))));
        app.handle_action(ActionId::ToggleForkMode);
        match &app.modal {
            Some(Modal::ForkWorkspace(form)) => assert_eq!(form.mode_label(), "worktree"),
            other => panic!("expected fork modal, got {other:?}"),
        }
    }

    #[test]
    fn vm_footer_actions_attach_stop_and_apply() {
        let (_dir, mut app) = test_app();
        app.instances = vec![fake_instance(
            &app,
            "rust",
            app.config.projects_root.join("demo"),
        )];
        app.open_vm_screen("rust".into());

        assert!(matches!(
            app.handle_action(ActionId::StartVm).as_slice(),
            [Effect::AttachVm { vm }] if vm == "rust"
        ));
        assert!(matches!(
            app.handle_action(ActionId::StopVm).as_slice(),
            [Effect::StopVm { vm }] if vm == "rust"
        ));
        assert!(matches!(
            app.handle_action(ActionId::ApplyVm).as_slice(),
            [Effect::ApplyVm { vm }] if vm == "rust"
        ));
    }
}
