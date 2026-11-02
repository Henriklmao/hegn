//! Keyboard handling for the dashboard, the project screen, the embedded
//! terminal and every modal.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Harness;
use crate::config::form::FormOutcome;
use crate::project::{self, Project, ProjectMeta};

use super::*;

impl App {
    /// Handle a key press and return side effects for the terminal loop.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        self.hovered = None;
        self.config_hover = None;
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        self.scroll_hover = false;
        self.scroll_dragging = false;
        match self.screen {
            Screen::Dashboard => self.handle_dashboard_key(key),
            Screen::Project => self.handle_project_key(key),
        }
    }

    pub(super) fn handle_dashboard_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return vec![Effect::Quit];
        }

        if self.searching {
            match key.code {
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.query.clear();
                    self.refresh_filter();
                }
                KeyCode::Char(c) => {
                    self.query.push(c);
                    self.refresh_filter();
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.refresh_filter();
                }
                KeyCode::Enter => {
                    self.searching = false;
                    return self.open_selected();
                }
                KeyCode::Esc => {
                    self.searching = false;
                }
                KeyCode::Up => self.select_next(-1),
                KeyCode::Down => self.select_next(1),
                KeyCode::Tab => self.focus_next(),
                _ => {}
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                vec![Effect::Quit]
            }
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.refresh_filter();
                }
                Vec::new()
            }
            KeyCode::Char('/') => {
                self.searching = true;
                Vec::new()
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.modal = Some(Modal::Help(HelpState::default()));
                Vec::new()
            }
            KeyCode::F(3) => {
                self.modal = Some(Modal::Doctor(DoctorModal::new(
                    crate::diagnostics::doctor::run(&self.config),
                )));
                Vec::new()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next(1);
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_next(-1);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.select_next(5);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.select_next(-5);
                Vec::new()
            }
            KeyCode::Enter | KeyCode::Char('o') => self.open_project(),
            KeyCode::Char('c') => {
                self.modal = Some(Modal::CreateProject(CreateProjectForm::new(
                    self.config.harness,
                )));
                Vec::new()
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.modal = Some(Modal::ImportProject(ImportProjectForm::new()));
                Vec::new()
            }
            KeyCode::Char('e') => self.effect_for_selected(Effect::OpenEditor),
            KeyCode::Char('g') => self.effect_for_selected(Effect::OpenLazygit),
            KeyCode::Char('r') => {
                self.refresh_snapshots();
                vec![Effect::RefreshProjects, Effect::RefreshInstances]
            }
            KeyCode::Char('d') => match self.focus {
                Focus::Instances => self.confirm_delete_instance(),
                Focus::Snapshots => self.confirm_delete_snapshot(),
                Focus::Projects | Focus::Health | Focus::Git => self.confirm_delete_project(),
            },
            KeyCode::Char('n') if self.focus == Focus::Snapshots => {
                self.handle_action(ActionId::NewSnapshot)
            }
            KeyCode::Char('b') | KeyCode::Char('2') => self.open_project_section(Focus::Instances),
            KeyCode::Char('s') | KeyCode::Char('3') => self.open_project_section(Focus::Snapshots),
            KeyCode::Char('p') | KeyCode::Char('1') => {
                self.focus = Focus::Projects;
                Vec::new()
            }
            KeyCode::Char('S') if self.focus == Focus::Instances => self.stop_selected_instance(),
            KeyCode::F(4) => self.open_global_config(),
            KeyCode::F(5) if self.focus == Focus::Instances => self.open_instance_config(),
            KeyCode::Char('t') => {
                if self.session.is_some() {
                    self.show_session_project();
                    Vec::new()
                } else {
                    self.toast("no active session", ToastLevel::Warn);
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn effect_for_selected(
        &self,
        effect: impl FnOnce(PathBuf) -> Effect,
    ) -> Vec<Effect> {
        match self.selected_project() {
            Some(project) => vec![effect(project.path.clone())],
            None => Vec::new(),
        }
    }

    pub(super) fn handle_project_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if crate::session::attach_bar::detach_requested(key) {
            self.leave_project();
            return Vec::new();
        }
        if key.code == KeyCode::F(2) {
            return self.attach_console_external();
        }
        if key.code == KeyCode::F(6) {
            return self.open_project_config();
        }
        // Left leaves the project unless the guest is actively receiving text.
        if key.code == KeyCode::Left && self.terminal_mode == TerminalMode::Normal {
            self.leave_project();
            return Vec::new();
        }
        if self.terminal_mode != TerminalMode::Insert
            && let KeyCode::Char(number @ '1'..='4') = key.code
        {
            let section = match number {
                '1' => ProjectSection::Instances,
                '2' => ProjectSection::Snapshots,
                '3' => ProjectSection::Health,
                _ => ProjectSection::Git,
            };
            self.focus = section.focus();
            if self.project_view != ProjectView::Compact {
                self.project_view = ProjectView::Section;
                self.terminal_focus = false;
            }
            return Vec::new();
        }
        if self.terminal_mode != TerminalMode::Insert
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
        {
            self.focus_next();
            return Vec::new();
        }
        if self.terminal_focus {
            return self.handle_project_terminal_key(key);
        }

        if key.code == KeyCode::Esc {
            if self.project_view == ProjectView::Section {
                self.project_view = ProjectView::Split;
                self.terminal_focus = false;
            } else {
                self.leave_project();
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus_next();
                if self.project_view == ProjectView::Section {
                    self.project_section();
                }
                Vec::new()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next(1);
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_next(-1);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.select_next(5);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.select_next(-5);
                Vec::new()
            }
            KeyCode::Enter if self.focus == Focus::Snapshots => self.confirm_restore_snapshot(),
            KeyCode::Enter if self.focus == Focus::Instances => {
                if self.selected_instance().is_some() {
                    self.attach_selected_instance()
                } else {
                    self.start_project_instance()
                }
            }
            KeyCode::Char('i') | KeyCode::Char('a') | KeyCode::Char('t') => self.open_terminal(),
            KeyCode::Char('L') => self.request_launch(None),
            KeyCode::Char('n') if self.focus == Focus::Snapshots => {
                self.handle_action(ActionId::NewSnapshot)
            }
            KeyCode::Char('n') if self.focus == Focus::Instances => self.start_project_instance(),
            KeyCode::Char('M') => self.open_multi_agent(),
            KeyCode::Char('S') => self.stop_selected_instance(),
            KeyCode::Char('d') if self.focus == Focus::Instances => self.confirm_delete_instance(),
            KeyCode::Char('d') if self.focus == Focus::Snapshots => self.confirm_delete_snapshot(),
            KeyCode::Char('e') => self.effect_for_open_project(Effect::OpenEditor),
            KeyCode::Char('g') => self.effect_for_open_project(Effect::OpenLazygit),
            KeyCode::Char('r') => {
                self.refresh_snapshots();
                vec![Effect::RefreshProjects, Effect::RefreshInstances]
            }
            KeyCode::F(4) => self.open_global_config(),
            KeyCode::F(5) if self.focus == Focus::Instances => self.open_instance_config(),
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.modal = Some(Modal::Help(HelpState::default()));
                Vec::new()
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
                vec![Effect::Quit]
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn handle_project_terminal_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.code == KeyCode::Esc {
            match self.terminal_mode {
                TerminalMode::Insert => {
                    self.terminal_mode = TerminalMode::Normal;
                    self.toast("normal mode — press i or a to type", ToastLevel::Info);
                }
                _ => self.terminal_focus = false,
            }
            return Vec::new();
        }

        match self.terminal_mode {
            TerminalMode::Raw => {
                if let Some(bytes) = encode_key(key) {
                    self.write_bytes(&bytes);
                }
            }
            TerminalMode::Normal => match key.code {
                KeyCode::Char('i') | KeyCode::Char('a') => {
                    self.terminal_mode = TerminalMode::Insert;
                    self.toast("insert mode — Esc returns to normal", ToastLevel::Info);
                }
                KeyCode::Char('M') => return self.open_multi_agent(),
                KeyCode::Char('L') => return self.request_launch(None),
                KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.terminal.clear();
                    self.scroll = 0;
                }
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_add(5).min(self.terminal.len());
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_sub(5);
                }
                KeyCode::Home => {
                    self.scroll = self.terminal.len().saturating_sub(1);
                }
                KeyCode::End => {
                    self.scroll = 0;
                }
                _ => {}
            },
            TerminalMode::Insert => match key.code {
                KeyCode::Enter => {
                    return self.submit_input();
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.clear();
                    self.write_bytes(b"\x03");
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.write_bytes(b"\x04");
                }
                KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.terminal.clear();
                    self.scroll = 0;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.clear();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    if self.input.len() < 8192 {
                        self.input.push(c);
                    }
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Tab => self.write_bytes(b"\t"),
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_add(5).min(self.terminal.len());
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_sub(5);
                }
                KeyCode::Home => {
                    self.scroll = self.terminal.len().saturating_sub(1);
                }
                KeyCode::End => {
                    self.scroll = 0;
                }
                _ => {}
            },
        }
        Vec::new()
    }

    pub(super) fn handle_modal_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(modal) = self.modal.take() else {
            return Vec::new();
        };
        match modal {
            Modal::Help(mut help) => {
                if help.searching {
                    let scroll = match key.code {
                        KeyCode::Up => Some(-1),
                        KeyCode::Down => Some(1),
                        _ => None,
                    };
                    let page = match key.code {
                        KeyCode::PageUp => Some(-1),
                        KeyCode::PageDown => Some(1),
                        _ => None,
                    };
                    match key.code {
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            help.query.clear();
                            help.scroll = 0;
                        }
                        KeyCode::Char(c) => {
                            help.query.push(c);
                            help.scroll = 0;
                        }
                        KeyCode::Backspace => {
                            help.query.pop();
                            help.scroll = 0;
                        }
                        KeyCode::Esc | KeyCode::Enter => help.searching = false,
                        _ => {}
                    }
                    self.modal = Some(Modal::Help(help));
                    if let Some(delta) = scroll {
                        self.scroll_modal(delta);
                    }
                    if let Some(pages) = page {
                        self.scroll_modal_page(pages);
                    }
                    return Vec::new();
                }
                match key.code {
                    KeyCode::Char('/') => {
                        help.searching = true;
                        self.modal = Some(Modal::Help(help));
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.modal = Some(Modal::Help(help));
                        self.scroll_modal(1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.modal = Some(Modal::Help(help));
                        self.scroll_modal(-1);
                    }
                    KeyCode::PageDown => {
                        self.modal = Some(Modal::Help(help));
                        self.scroll_modal_page(1);
                    }
                    KeyCode::PageUp => {
                        self.modal = Some(Modal::Help(help));
                        self.scroll_modal_page(-1);
                    }
                    KeyCode::Esc if !help.query.is_empty() => {
                        help.query.clear();
                        help.scroll = 0;
                        self.modal = Some(Modal::Help(help));
                    }
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
                        // closed
                    }
                    _ => self.modal = Some(Modal::Help(help)),
                }
                Vec::new()
            }
            Modal::Error { .. } => Vec::new(),
            Modal::Confirm(prompt) => self.handle_confirm_key(key, prompt),
            Modal::Config(mut form) => match form.handle_key(key) {
                FormOutcome::Stay => {
                    self.modal = Some(Modal::Config(form));
                    Vec::new()
                }
                FormOutcome::Close => Vec::new(),
                FormOutcome::Save => self.save_config_form(form),
                FormOutcome::Revert => self.revert_config_form(form),
                FormOutcome::OpenFile => vec![Effect::OpenEditor(form.path)],
                FormOutcome::OpenProfile(harness) => vec![Effect::OpenProfileEditor(harness)],
            },
            Modal::DeleteInstance(mut modal) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Vec::new(),
                KeyCode::Char('y') => match modal.stage {
                    DeleteInstanceStage::Confirm => {
                        if self.workspace_is_inside_instance(&modal.instance) {
                            modal.stage = DeleteInstanceStage::Files;
                            self.modal = Some(Modal::DeleteInstance(modal));
                            Vec::new()
                        } else {
                            vec![Effect::DeleteInstance {
                                instance: modal.instance.name,
                                delete_files: false,
                            }]
                        }
                    }
                    DeleteInstanceStage::Files => vec![Effect::DeleteInstance {
                        instance: modal.instance.name,
                        delete_files: true,
                    }],
                },
                KeyCode::Char('n') | KeyCode::Char('k')
                    if modal.stage == DeleteInstanceStage::Files =>
                {
                    vec![Effect::DeleteInstance {
                        instance: modal.instance.name,
                        delete_files: false,
                    }]
                }
                KeyCode::Char('n') => Vec::new(),
                KeyCode::Char('j') | KeyCode::Down => {
                    self.modal = Some(Modal::DeleteInstance(modal));
                    self.scroll_modal(1);
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up if modal.stage == DeleteInstanceStage::Confirm => {
                    self.modal = Some(Modal::DeleteInstance(modal));
                    self.scroll_modal(-1);
                    Vec::new()
                }
                KeyCode::PageDown => {
                    self.modal = Some(Modal::DeleteInstance(modal));
                    self.scroll_modal_page(1);
                    Vec::new()
                }
                KeyCode::PageUp => {
                    self.modal = Some(Modal::DeleteInstance(modal));
                    self.scroll_modal_page(-1);
                    Vec::new()
                }
                _ => {
                    self.modal = Some(Modal::DeleteInstance(modal));
                    Vec::new()
                }
            },
            Modal::DeleteProject(modal) => match key.code {
                KeyCode::Char('y') => self.delete_project_effect(modal, false),
                KeyCode::Char('f') => self.delete_project_effect(modal, true),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => Vec::new(),
                KeyCode::Char('j') | KeyCode::Down => {
                    self.modal = Some(Modal::DeleteProject(modal));
                    self.scroll_modal(1);
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.modal = Some(Modal::DeleteProject(modal));
                    self.scroll_modal(-1);
                    Vec::new()
                }
                KeyCode::PageDown => {
                    self.modal = Some(Modal::DeleteProject(modal));
                    self.scroll_modal_page(1);
                    Vec::new()
                }
                KeyCode::PageUp => {
                    self.modal = Some(Modal::DeleteProject(modal));
                    self.scroll_modal_page(-1);
                    Vec::new()
                }
                _ => {
                    self.modal = Some(Modal::DeleteProject(modal));
                    Vec::new()
                }
            },
            Modal::MultiAgent(mut form) => match key.code {
                KeyCode::Esc => Vec::new(),
                KeyCode::Up | KeyCode::Char('k') => {
                    form.count = form.count.saturating_add(1).min(8);
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    form.count = form.count.saturating_sub(1).max(1);
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    let index = Harness::ALL
                        .iter()
                        .position(|harness| *harness == form.harness)
                        .unwrap_or(0);
                    let next = if key.code == KeyCode::Left {
                        (index + Harness::ALL.len() - 1) % Harness::ALL.len()
                    } else {
                        (index + 1) % Harness::ALL.len()
                    };
                    form.harness = Harness::ALL[next];
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
                KeyCode::Char(' ') => {
                    form.start = !form.start;
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
                KeyCode::PageDown => {
                    self.modal = Some(Modal::MultiAgent(form));
                    self.scroll_modal_page(1);
                    Vec::new()
                }
                KeyCode::PageUp => {
                    self.modal = Some(Modal::MultiAgent(form));
                    self.scroll_modal_page(-1);
                    Vec::new()
                }
                KeyCode::Enter => vec![Effect::CloneInstance {
                    source: form.source,
                    count: form.count,
                    harness: form.harness,
                    start: form.start,
                }],
                _ => {
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
            },
            Modal::ProjectConfig(mut form) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Vec::new(),
                KeyCode::Up | KeyCode::Char('k') => {
                    form.select(-1);
                    self.modal = Some(Modal::ProjectConfig(form));
                    Vec::new()
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    form.select(1);
                    self.modal = Some(Modal::ProjectConfig(form));
                    Vec::new()
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    self.cycle_project_config(&mut form, -1);
                    self.modal = Some(Modal::ProjectConfig(form));
                    Vec::new()
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.cycle_project_config(&mut form, 1);
                    self.modal = Some(Modal::ProjectConfig(form));
                    Vec::new()
                }
                KeyCode::Enter => self.save_project_config(form),
                KeyCode::Char('r') | KeyCode::Char('L') => self.launch_project_config(form),
                _ => {
                    self.modal = Some(Modal::ProjectConfig(form));
                    Vec::new()
                }
            },
            Modal::Doctor(mut modal) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Vec::new(),
                KeyCode::Char('r') => {
                    modal.report = crate::diagnostics::doctor::run(&self.config);
                    modal.downloading = false;
                    modal.scroll = 0;
                    self.modal = Some(Modal::Doctor(modal));
                    Vec::new()
                }
                KeyCode::Char('d') if modal.report.can_download() && !modal.downloading => {
                    modal.downloading = true;
                    self.modal = Some(Modal::Doctor(modal));
                    vec![Effect::DownloadImages]
                }
                KeyCode::Char('s')
                    if modal.report.can_setup() && !modal.downloading && !modal.setting_up =>
                {
                    self.modal = Some(Modal::Doctor(modal));
                    self.open_setup_confirmation()
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.modal = Some(Modal::Doctor(modal));
                    self.scroll_modal(1);
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.modal = Some(Modal::Doctor(modal));
                    self.scroll_modal(-1);
                    Vec::new()
                }
                KeyCode::PageDown => {
                    self.modal = Some(Modal::Doctor(modal));
                    self.scroll_modal_page(1);
                    Vec::new()
                }
                KeyCode::PageUp => {
                    self.modal = Some(Modal::Doctor(modal));
                    self.scroll_modal_page(-1);
                    Vec::new()
                }
                _ => {
                    self.modal = Some(Modal::Doctor(modal));
                    Vec::new()
                }
            },
            Modal::Security(prompt) => self.handle_security_key(key, prompt),
            Modal::ImportProject(mut form) => {
                if key.code == KeyCode::Esc {
                    return Vec::new();
                }
                match key.code {
                    KeyCode::Tab | KeyCode::BackTab => {
                        form.cycle_focus();
                        self.modal = Some(Modal::ImportProject(form));
                    }
                    KeyCode::Enter => return self.submit_import_project(form),
                    _ => {
                        self.edit_import_form(&mut form, key);
                        self.modal = Some(Modal::ImportProject(form));
                    }
                }
                Vec::new()
            }
            Modal::CreateProject(mut form) => {
                if key.code == KeyCode::Esc {
                    return Vec::new();
                }
                match key.code {
                    KeyCode::Tab => {
                        form.cycle_focus(false);
                        self.modal = Some(Modal::CreateProject(form));
                    }
                    KeyCode::BackTab => {
                        form.cycle_focus(true);
                        self.modal = Some(Modal::CreateProject(form));
                    }
                    KeyCode::Enter if form.focus == CreateFocus::Create => {
                        return self.submit_create_project(form);
                    }
                    KeyCode::Enter => {
                        form.cycle_focus(false);
                        self.modal = Some(Modal::CreateProject(form));
                    }
                    KeyCode::Char(' ') if form.focus == CreateFocus::Packages => {
                        if let Some(pkg) = form.packages.get_mut(form.package_cursor) {
                            pkg.selected = !pkg.selected;
                        }
                        self.modal = Some(Modal::CreateProject(form));
                    }
                    _ => {
                        self.edit_create_form(&mut form, key);
                        self.modal = Some(Modal::CreateProject(form));
                    }
                }
                Vec::new()
            }
        }
    }

    pub(super) fn edit_create_form(&self, form: &mut CreateProjectForm, key: KeyEvent) {
        match form.focus {
            CreateFocus::Name => match key.code {
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    form.name.push(c)
                }
                KeyCode::Backspace => {
                    form.name.pop();
                }
                _ => {}
            },
            CreateFocus::Harness => {
                let harnesses = Harness::ALL;
                let index = harnesses
                    .iter()
                    .position(|h| *h == form.harness)
                    .unwrap_or(0);
                match key.code {
                    KeyCode::Left => {
                        form.harness = harnesses[(index + harnesses.len() - 1) % harnesses.len()];
                    }
                    KeyCode::Right | KeyCode::Char(' ') => {
                        form.harness = harnesses[(index + 1) % harnesses.len()];
                    }
                    _ => {}
                }
            }
            CreateFocus::Packages => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    form.package_cursor = form
                        .package_cursor
                        .checked_sub(1)
                        .unwrap_or(form.packages.len().saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    form.package_cursor = (form.package_cursor + 1) % form.packages.len().max(1);
                }
                KeyCode::Char(' ') => {
                    if let Some(pkg) = form.packages.get_mut(form.package_cursor) {
                        pkg.selected = !pkg.selected;
                    }
                }
                _ => {}
            },
            CreateFocus::Create => {
                if key.code == KeyCode::Char(' ') {
                    // Submit is handled on Enter; space is harmless here.
                }
            }
        }
    }

    pub(super) fn submit_create_project(&mut self, form: CreateProjectForm) -> Vec<Effect> {
        let templates = self.config.templates_dir.join("AGENTS.md");
        let template = templates.is_file().then_some(templates.as_path());
        match project::create_project(
            &self.config.projects_root,
            &form.name,
            template,
            form.harness,
        ) {
            Ok(path) => {
                let meta = ProjectMeta {
                    harness: Some(form.harness),
                    packages: form.selected_packages(),
                    template: template.map(|path| path.display().to_string()),
                    ..ProjectMeta::default()
                };
                if let Err(err) = project::write_meta(&path, &meta) {
                    self.toast(
                        format!("project created, metadata failed: {err}"),
                        ToastLevel::Warn,
                    );
                }
                let loaded = Project::load(path.clone());
                self.add_project(loaded);
                self.toast(format!("created {}", path.display()), ToastLevel::Info);
            }
            Err(err) => {
                let mut form = form;
                form.error = Some(err.to_string());
                self.modal = Some(Modal::CreateProject(form));
            }
        }
        Vec::new()
    }

    pub(super) fn edit_import_form(&self, form: &mut ImportProjectForm, key: KeyEvent) {
        let field = match form.focus {
            ImportFocus::Path => &mut form.path,
            ImportFocus::Name => &mut form.name,
        };
        match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if field.len() < 4096 {
                    field.push(c);
                }
            }
            KeyCode::Backspace => {
                field.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                field.clear();
            }
            _ => {}
        }
    }

    pub(super) fn submit_import_project(&mut self, form: ImportProjectForm) -> Vec<Effect> {
        let path = form.path.trim();
        if path.is_empty() {
            let mut form = form;
            form.error = Some("enter the path of an existing directory".into());
            self.modal = Some(Modal::ImportProject(form));
            return Vec::new();
        }
        let name = {
            let name = form.name.trim();
            (!name.is_empty()).then_some(name)
        };
        match self.config.import_project(Path::new(path), name) {
            Ok(link) => {
                let loaded = Project::load(link.clone());
                let target = link
                    .canonicalize()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| link.display().to_string());
                self.add_project(loaded);
                self.toast(
                    format!("imported {} → {target}", link.display()),
                    ToastLevel::Info,
                );
            }
            Err(err) => {
                let mut form = form;
                form.error = Some(err.to_string());
                self.modal = Some(Modal::ImportProject(form));
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn search_mode_edits_filter_and_escape_leaves_it() {
        let (_dir, mut app) = test_app();
        app.set_projects(vec![fake_project("alpha"), fake_project("beta")]);
        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.searching);
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.query, "b");
        assert_eq!(app.selected_project().unwrap().name, "beta");
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.searching);
    }

    #[test]
    fn quit_key_sets_flag() {
        let (_dir, mut app) = test_app();
        let effects = app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
        assert!(matches!(effects.as_slice(), [Effect::Quit]));
    }

    #[test]
    fn tab_toggles_project_sections() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.focus = Focus::Instances;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Snapshots);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Health);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Git);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Instances);
    }

    #[test]
    fn number_keys_jump_to_project_sections() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.focus = Focus::Instances;
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.focus, Focus::Snapshots);
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.focus, Focus::Health);
        app.handle_key(key(KeyCode::Char('4')));
        assert_eq!(app.focus, Focus::Git);
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.focus, Focus::Instances);
    }

    #[test]
    fn enter_never_confirms_a_deletion() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        app.focus = Focus::Instances;
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            app.modal,
            Some(Modal::DeleteInstance(
                crate::tui::app::DeleteInstanceModal {
                    stage: crate::tui::app::DeleteInstanceStage::Confirm,
                    ..
                }
            ))
        ));

        app.handle_key(key(KeyCode::Char('y')));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(effects.is_empty(), "Enter must not delete files");
        assert!(matches!(
            app.modal,
            Some(Modal::DeleteInstance(
                crate::tui::app::DeleteInstanceModal {
                    stage: crate::tui::app::DeleteInstanceStage::Files,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn enter_never_confirms_destructive_prompts() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Confirm(ConfirmPrompt {
            title: "Delete snapshot".into(),
            message: "Delete 'demo'?".into(),
            confirm_label: "Delete".into(),
            action: ConfirmAction::DeleteSnapshot("demo".into()),
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));

        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(app.modal, Some(Modal::Security(_))),
            "Enter must not allow a flagged command"
        );
    }

    #[test]
    fn create_project_modal_creates_directory() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('c')));
        assert!(matches!(app.modal, Some(Modal::CreateProject(_))));
        for c in "demo-app".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // Tab to the create button and submit.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.modal.is_none(), "{:?}", app.modal);
        let path = app.config.projects_root.join("demo-app");
        assert!(path.join("AGENTS.md").is_file());
        assert!(app.projects.iter().any(|p| p.path == path));
        let meta = project::read_meta(&path);
        assert_eq!(meta.harness, Some(Harness::Shell));
        assert!(meta.template.is_none());
    }

    #[test]
    fn create_project_error_keeps_modal_open() {
        let (_dir, mut app) = test_app();
        app.config.projects_root = PathBuf::from("/definitely/missing/root");
        app.handle_key(key(KeyCode::Char('c')));
        app.handle_key(key(KeyCode::Char('x')));
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::CreateProject(form)) => assert!(form.error.is_some()),
            other => panic!("expected form, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_focus_supports_documented_new_shortcut() {
        let (_dir, mut app) = test_app();
        app.focus = Focus::Snapshots;
        app.handle_key(key(KeyCode::Char('n')));
        let toast = app.toast.as_ref().unwrap();
        assert!(
            toast.text.contains("hegn snapshot create"),
            "{}",
            toast.text
        );
    }

    #[test]
    fn terminal_input_collects_and_clears_on_empty_submit() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.input, "ls");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "l");
        app.input.clear();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.input.is_empty());
    }

    #[test]
    fn terminal_starts_in_normal_mode_and_i_enters_insert() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        // Normal mode swallows printable keys instead of forwarding them.
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.input.is_empty());
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.terminal_mode, TerminalMode::Insert);
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "x");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
        assert_eq!(app.screen, Screen::Project);
        // Esc in normal mode returns focus to the sections; the pane stays.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Project);
        // Left (or the breadcrumb) leaves the project.
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn insert_mode_esc_does_not_leave_the_terminal() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.terminal_mode, TerminalMode::Insert);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Project);
    }

    #[test]
    fn left_arrow_leaves_the_project_terminal_in_normal_mode() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        assert_eq!(app.screen, Screen::Project);

        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn left_arrow_in_insert_or_raw_mode_does_not_leave_the_terminal() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));

        app.terminal_mode = TerminalMode::Insert;
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Project);

        app.terminal_mode = TerminalMode::Raw;
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Project);
    }

    #[test]
    fn multi_agent_is_reachable_from_the_project_terminal() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        assert_eq!(app.screen, Screen::Project);
        let effects = app.handle_key(key(KeyCode::Char('M')));
        assert!(effects.is_empty());
        match &app.modal {
            Some(Modal::MultiAgent(form)) => assert_eq!(form.source, "fake"),
            other => panic!("expected multi-agent form, got {other:?}"),
        }
    }

    #[test]
    fn help_search_mode_captures_typing_and_clears_in_steps() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Help(HelpState::default()));
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(&app.modal, Some(Modal::Help(help)) if help.searching));

        for c in "sq".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let Some(Modal::Help(help)) = &app.modal else {
            panic!("typing must not close the help modal");
        };
        assert_eq!(help.query, "sq");

        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Esc));
        let Some(Modal::Help(help)) = &app.modal else {
            panic!("the first Esc stops searching");
        };
        assert!(!help.searching);
        assert_eq!(help.query, "s");

        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(&app.modal, Some(Modal::Help(help)) if help.query.is_empty()));

        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn instance_focus_and_delete_double_confirmation() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        app.focus = Focus::Instances;
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);

        app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(
            app.modal,
            Some(Modal::DeleteInstance(
                crate::tui::app::DeleteInstanceModal {
                    stage: crate::tui::app::DeleteInstanceStage::Confirm,
                    ..
                }
            ))
        ));
        // First confirmation moves to the file question.
        app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            app.modal,
            Some(Modal::DeleteInstance(
                crate::tui::app::DeleteInstanceModal {
                    stage: crate::tui::app::DeleteInstanceStage::Files,
                    ..
                }
            ))
        ));
        let effects = app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::DeleteInstance {
                instance,
                delete_files: true
            }] if instance == "demo"
        ));
    }

    #[test]
    fn projects_focus_delete_opens_the_project_delete_modal() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.projects = vec![Project {
            name: "demo".into(),
            path: app.config.projects_root.join("demo"),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.focus = Focus::Projects;

        app.handle_key(key(KeyCode::Char('d')));
        let Some(Modal::DeleteProject(modal)) = &app.modal else {
            panic!("expected a project delete confirmation");
        };
        assert_eq!(modal.project.name, "demo");
        assert_eq!(modal.instances, vec!["demo".to_string()]);
    }

    #[test]
    fn project_delete_without_a_vm_does_not_need_an_instance() {
        let (_dir, mut app) = test_app();
        app.projects = vec![Project {
            name: "empty".into(),
            path: app.config.projects_root.join("empty"),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.instances.clear();
        app.focus = Focus::Projects;

        app.handle_key(key(KeyCode::Char('d')));
        let Some(Modal::DeleteProject(modal)) = &app.modal else {
            panic!("expected a project delete confirmation");
        };
        assert!(modal.instances.is_empty());
    }

    #[test]
    fn delete_project_asks_about_files_and_shows_git_state() {
        let (_dir, mut app) = test_app();
        let path = app.config.projects_root.join("dirty");
        std::fs::create_dir_all(&path).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(path.join("untracked.txt"), "work in progress").unwrap();
        app.projects = vec![Project {
            name: "dirty".into(),
            path: path.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;

        app.handle_key(key(KeyCode::Char('d')));
        let Some(Modal::DeleteProject(modal)) = &app.modal else {
            panic!("expected a project delete confirmation");
        };
        assert!(modal.git.is_repo);
        assert_eq!(modal.git.untracked, 1);

        // Enter never confirms a destructive prompt.
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(effects.is_empty());
        assert!(matches!(app.modal, Some(Modal::DeleteProject(_))));

        let effects = app.handle_key(key(KeyCode::Char('f')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::DeleteProject { project, delete_files: true }] if project == &path
        ));
        assert!(app.modal.is_none());
    }

    #[test]
    fn keeping_project_files_emits_a_keep_effect() {
        let (_dir, mut app) = test_app();
        let path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        app.projects = vec![Project {
            name: "demo".into(),
            path: path.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;

        app.handle_key(key(KeyCode::Char('d')));
        let effects = app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::DeleteProject { project, delete_files: false }] if project == &path
        ));
    }

    #[test]
    fn number_keys_do_nothing_without_a_selected_project() {
        let (_dir, mut app) = test_app();
        app.set_projects(vec![]);
        app.screen = Screen::Dashboard;
        app.focus = Focus::Projects;

        assert!(app.handle_key(key(KeyCode::Char('2'))).is_empty());
        assert_eq!(app.screen, Screen::Dashboard);
        assert_eq!(app.focus, Focus::Projects);
        assert!(app.toast.is_none());

        assert!(app.handle_key(key(KeyCode::Char('3'))).is_empty());
        assert_eq!(app.screen, Screen::Dashboard);
        assert_eq!(app.focus, Focus::Projects);
    }

    #[test]
    fn number_keys_open_a_selected_project_on_that_section() {
        let (_dir, mut app) = test_app();
        let mut project = fake_project("demo");
        project.path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project.path).unwrap();
        app.set_projects(vec![project]);

        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.screen, Screen::Project);
        assert_eq!(app.focus, Focus::Instances);
    }

    #[test]
    fn keeping_files_emits_a_keep_effect() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        app.focus = Focus::Instances;
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('y')));
        let effects = app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::DeleteInstance {
                instance,
                delete_files: false
            }] if instance == "demo"
        ));
    }

    #[test]
    fn stop_shortcut_only_works_with_instance_focus() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.focus = Focus::Projects;
        let effects = app.handle_key(key(KeyCode::Char('S')));
        assert!(
            effects.is_empty(),
            "S must not stop an instance from projects"
        );
        app.focus = Focus::Instances;
        let effects = app.handle_key(key(KeyCode::Char('S')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::StopInstance { instance }] if instance == "demo"
        ));
    }

    #[test]
    fn a_section_number_shrinks_the_terminal_to_section_view() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        app.project_view = ProjectView::Split;
        app.focus = Focus::Instances;
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.project_view, ProjectView::Section);
        assert_eq!(app.focus, Focus::Snapshots);
        assert!(!app.terminal_focus);
    }

    #[test]
    fn esc_keeps_the_terminal_expanded_until_left_or_breadcrumb() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        assert_eq!(app.project_view, ProjectView::Split);
        app.terminal_mode = TerminalMode::Insert;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
        assert!(app.terminal_focus);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.project_view, ProjectView::Split);
        assert!(!app.terminal_focus);
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn h_and_left_open_the_import_modal_from_the_project_list() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('h')));
        assert!(matches!(app.modal, Some(Modal::ImportProject(_))));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());

        app.handle_key(key(KeyCode::Left));
        assert!(matches!(app.modal, Some(Modal::ImportProject(_))));
    }

    #[test]
    fn import_form_creates_a_symlink_and_adds_the_project() {
        let (dir, mut app) = test_app();
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();

        app.handle_key(key(KeyCode::Char('h')));
        for ch in source.to_string_lossy().chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(app.modal.is_none(), "successful import closes the modal");
        let link = app.config.projects_root.join("demo");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(app.projects.iter().any(|project| project.name == "demo"));
        let toast = app.toast.as_ref().expect("import toast");
        assert!(toast.text.contains("imported"), "{}", toast.text);
    }

    #[test]
    fn delete_instance_cancel_keys_emit_nothing() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        app.focus = Focus::Instances;
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);

        app.handle_key(key(KeyCode::Char('d')));
        let effects = app.handle_key(key(KeyCode::Esc));
        assert!(effects.is_empty());
        assert!(app.modal.is_none());

        app.handle_key(key(KeyCode::Char('d')));
        let effects = app.handle_key(key(KeyCode::Char('n')));
        assert!(effects.is_empty());
        assert!(app.modal.is_none());
    }

    #[test]
    fn delete_instance_escape_at_the_files_question_keeps_the_instance() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        app.focus = Focus::Instances;
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('y')));

        let effects = app.handle_key(key(KeyCode::Esc));

        assert!(
            effects.is_empty(),
            "the last chance must not delete: {effects:?}"
        );
        assert!(app.modal.is_none());
    }

    #[test]
    fn deleting_an_instance_outside_the_instance_dir_skips_the_file_question() {
        let (dir, mut app) = test_app();
        let outside = dir.path().join("host-workspace");
        std::fs::create_dir_all(&outside).unwrap();
        app.focus = Focus::Instances;
        let instance = fake_instance(&app, "demo", outside);
        install_instances(&mut app, vec![instance]);

        app.handle_key(key(KeyCode::Char('d')));
        let effects = app.handle_key(key(KeyCode::Char('y')));

        assert!(matches!(
            effects.as_slice(),
            [Effect::DeleteInstance {
                instance,
                delete_files: false
            }] if instance == "demo"
        ));
        assert!(app.modal.is_none());
    }

    #[test]
    fn delete_instance_files_stage_k_and_n_keep_the_files() {
        for cancel in ['k', 'n'] {
            let (_dir, mut app) = test_app();
            let inside = app.config.instances_dir.join("demo").join("workspace");
            app.focus = Focus::Instances;
            let instance = fake_instance(&app, "demo", inside);
            install_instances(&mut app, vec![instance]);
            app.handle_key(key(KeyCode::Char('d')));
            app.handle_key(key(KeyCode::Char('y')));

            let effects = app.handle_key(key(KeyCode::Char(cancel)));

            assert!(matches!(
                effects.as_slice(),
                [Effect::DeleteInstance {
                    instance,
                    delete_files: false
                }] if instance == "demo"
            ));
        }
    }

    #[test]
    fn project_delete_cancel_keys_emit_nothing() {
        let (_dir, mut app) = test_app();
        let path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        app.projects = vec![Project {
            name: "demo".into(),
            path,
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;

        for cancel in [KeyCode::Char('n'), KeyCode::Char('q'), KeyCode::Esc] {
            app.handle_key(key(KeyCode::Char('d')));
            assert!(matches!(app.modal, Some(Modal::DeleteProject(_))));
            let effects = app.handle_key(key(cancel));
            assert!(
                effects.is_empty(),
                "{cancel:?} must not delete: {effects:?}"
            );
            assert!(app.modal.is_none());
        }
    }

    #[test]
    fn create_form_cycles_focus_both_ways_and_toggles_packages() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('c')));

        app.handle_key(key(KeyCode::Tab)); // Name -> Harness
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Tab)); // Harness -> Packages
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Tab)); // Packages -> Create

        let Some(Modal::CreateProject(form)) = &app.modal else {
            panic!("expected the create form");
        };
        assert_eq!(form.focus, CreateFocus::Create);
        assert_ne!(form.harness, Harness::Shell, "Right picks the next harness");
        assert_eq!(form.package_cursor, 1);
        assert_eq!(form.selected_packages(), vec!["git", "nodejs"]);

        app.handle_key(key(KeyCode::BackTab)); // Create -> Packages
        app.handle_key(key(KeyCode::BackTab)); // Packages -> Harness
        app.handle_key(key(KeyCode::BackTab)); // Harness -> Name
        app.handle_key(key(KeyCode::BackTab)); // Name wraps to Create
        assert!(matches!(
            &app.modal,
            Some(Modal::CreateProject(form)) if form.focus == CreateFocus::Create
        ));
    }

    #[test]
    fn enter_in_the_create_form_advances_focus_and_empty_names_do_not_submit() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('c')));

        app.handle_key(key(KeyCode::Enter)); // Name -> Harness
        assert!(matches!(
            &app.modal,
            Some(Modal::CreateProject(form)) if form.focus == CreateFocus::Harness
        ));
        app.handle_key(key(KeyCode::Enter)); // Harness -> Packages
        app.handle_key(key(KeyCode::Enter)); // Packages -> Create
        assert!(matches!(
            &app.modal,
            Some(Modal::CreateProject(form)) if form.focus == CreateFocus::Create
        ));

        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::CreateProject(form)) => {
                let error = form.error.as_deref().unwrap_or_default();
                assert!(error.contains("empty"), "{error}");
            }
            other => panic!("empty names must keep the form open, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_dir(&app.config.projects_root)
                .unwrap()
                .count(),
            0,
            "an invalid submit must not create a directory"
        );
    }

    #[test]
    fn import_form_edits_the_path_and_optional_name_separately() {
        let (dir, mut app) = test_app();
        let source = dir.path().join("external/repo");
        std::fs::create_dir_all(&source).unwrap();

        app.handle_key(key(KeyCode::Char('h')));
        for c in source.to_string_lossy().chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Tab));
        assert!(matches!(
            &app.modal,
            Some(Modal::ImportProject(form)) if form.focus == ImportFocus::Name
        ));
        for c in "linked".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(app.modal.is_none(), "{:?}", app.modal);
        let link = app.config.projects_root.join("linked");
        assert_eq!(
            std::fs::canonicalize(&link).unwrap(),
            std::fs::canonicalize(&source).unwrap()
        );
        assert!(app.projects.iter().any(|project| project.name == "linked"));
    }

    #[test]
    fn import_form_rejects_empty_and_missing_paths_in_place() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::ImportProject(form)) => {
                assert!(
                    form.error
                        .as_deref()
                        .unwrap_or("")
                        .contains("enter the path")
                );
            }
            other => panic!("expected import form, got {other:?}"),
        }
        assert!(app.projects.is_empty());

        app.modal = None;
        app.handle_key(key(KeyCode::Char('h')));
        for c in "/definitely/missing/hegn-import".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::ImportProject(form)) => {
                assert!(
                    form.error
                        .as_deref()
                        .unwrap_or("")
                        .contains("does not exist"),
                    "{:?}",
                    form.error
                );
            }
            other => panic!("expected import form, got {other:?}"),
        }
        assert!(app.projects.is_empty());
    }

    #[test]
    fn importing_hegn_state_is_rejected_in_the_form() {
        let (_dir, mut app) = test_app();
        std::fs::create_dir_all(&app.config.state_dir).unwrap();
        let state_path = app.config.state_dir.to_string_lossy().into_owned();
        app.handle_key(key(KeyCode::Char('h')));
        for ch in state_path.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::ImportProject(form)) => {
                let error = form.error.as_deref().unwrap_or_default();
                assert!(error.contains("overlaps hegn state"), "{error}");
            }
            other => panic!("expected import modal with an error, got {other:?}"),
        }
    }
}
