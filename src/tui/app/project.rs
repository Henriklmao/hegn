//! Project lifecycle: entering and leaving the project screen, VM attachment,
//! terminal input submission and local list maintenance after background
//! workers run.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::project::{self, Project};
use crate::security::Decision;
use crate::session::state::StateStore;

use super::*;

impl App {
    /// Open the selected project screen without booting a VM.
    pub fn open_project(&mut self) -> Vec<Effect> {
        let Some(project) = self.selected_project().cloned() else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };
        if let Err(err) = self.store.touch(&project.path) {
            self.toast(
                format!("could not save recent projects: {err}"),
                ToastLevel::Warn,
            );
        }
        self.enter_project(project.path);
        Vec::new()
    }

    /// Open the selected project on a section, or do nothing when the picker
    /// has no selection. Dashboard section shortcuts must never change focus
    /// into a project view that was not actually opened.
    pub(super) fn open_project_section(&mut self, focus: Focus) -> Vec<Effect> {
        if self.selected_project().is_none() {
            return Vec::new();
        }
        let effects = self.open_project();
        self.focus = focus;
        effects
    }

    pub(super) fn enter_project(&mut self, path: PathBuf) {
        self.project_meta = project::read_meta(&path);
        self.vm_open = None;
        self.open_project = Some(path);
        self.screen = Screen::Project;
        self.project_view = ProjectView::Compact;
        self.terminal_focus = false;
        self.focus = Focus::Vm;
        self.terminal_mode = TerminalMode::Normal;
        self.refresh_project_entries();
        self.refresh_instance_filter();
        self.refresh_snapshots();
    }

    pub(super) fn show_session_project(&mut self) {
        let Some(project) = self.session.as_ref().map(|s| s.project().to_path_buf()) else {
            return;
        };
        self.enter_project(project);
        self.project_view = ProjectView::Split;
        self.terminal_focus = true;
        self.terminal_mode = TerminalMode::Normal;
    }

    pub fn leave_project(&mut self) {
        self.screen = Screen::Dashboard;
        self.open_project = None;
        self.vm_open = None;
        self.project_view = ProjectView::Compact;
        self.terminal_focus = false;
        self.focus = Focus::Projects;
        self.refresh_filter();
    }

    /// Leave the VM screen without touching the VM.
    pub fn leave_vm(&mut self) {
        self.screen = Screen::Dashboard;
        self.vm_open = None;
        self.open_project = None;
        self.focus = Focus::Projects;
        self.refresh_filter();
    }

    /// Attach the VM of the active project, or ask for an assignment when the
    /// project is not part of a VM yet.
    pub(super) fn attach_project_vm(&mut self) -> Vec<Effect> {
        let Some(project) = self
            .active_project()
            .map(|project| project.path.clone())
            .or_else(|| self.open_project.clone())
        else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };
        match self.project_vm_name(&project).map(str::to_string) {
            Some(vm) => {
                self.vm.state = VmUiState::Starting;
                self.vm.started_at = None;
                self.vm.note = Some(format!("starting VM {vm}"));
                vec![Effect::StartVm {
                    vm,
                    project: Some(project),
                }]
            }
            None => self.open_assign_form(project, None),
        }
    }

    /// Ask the user to pick a VM for a project (optionally pinning one).
    pub(super) fn open_assign_form(&mut self, project: PathBuf, vm: Option<String>) -> Vec<Effect> {
        let candidates: Vec<String> = match vm {
            Some(vm) => vec![vm],
            None => self
                .instances
                .iter()
                .map(|summary| summary.name.clone())
                .collect(),
        };
        if candidates.is_empty() {
            self.toast(
                "no VMs exist yet — create one, then assign this project",
                ToastLevel::Warn,
            );
            self.modal = Some(Modal::CreateVm(CreateVmForm::new(
                self.config.harness,
                self.config.vcpus,
                self.config.memory_mib,
            )));
            return Vec::new();
        }
        let name = self
            .project_entry(&project)
            .map(|entry| entry.name.clone())
            .or_else(|| {
                project
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| project.display().to_string());
        self.modal = Some(Modal::AssignProject(AssignProjectForm::for_project(
            name, candidates,
        )));
        Vec::new()
    }

    /// Confirm detaching the open project from its VM.
    pub(super) fn confirm_detach_project(&mut self) -> Vec<Effect> {
        let Some(project) = self
            .open_project
            .clone()
            .or_else(|| self.active_project().map(|project| project.path.clone()))
        else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };
        let Some(name) = self.project_entry(&project).map(|entry| entry.name.clone()) else {
            self.toast("project is not registered with hegn", ToastLevel::Warn);
            return Vec::new();
        };
        let vm = self.project_vm_name(&project).unwrap_or("").to_string();
        self.modal = Some(Modal::DetachProject(DetachProjectModal {
            project: name,
            vm,
            scroll: 0,
        }));
        Vec::new()
    }

    pub(super) fn confirm_fork_workspace(&mut self, parent: Option<String>) -> Vec<Effect> {
        let parent = parent.or_else(|| {
            self.project_entry(
                &self
                    .open_project
                    .clone()
                    .or_else(|| self.active_project().map(|project| project.path.clone()))
                    .unwrap_or_default(),
            )
            .map(|entry| entry.name.clone())
        });
        let Some(parent) = parent else {
            self.toast("select a registered project first", ToastLevel::Warn);
            return Vec::new();
        };
        let Some(vm) = self
            .project_entry(
                &self
                    .open_project
                    .clone()
                    .or_else(|| self.active_project().map(|project| project.path.clone()))
                    .unwrap_or_default(),
            )
            .and_then(|entry| entry.vm.clone())
        else {
            self.toast(
                "the project is unassigned — assign it to a VM before forking",
                ToastLevel::Warn,
            );
            return Vec::new();
        };
        self.modal = Some(Modal::ForkWorkspace(ForkWorkspaceForm::new(parent, vm)));
        Vec::new()
    }

    pub(super) fn confirm_delete_vm(&mut self) -> Vec<Effect> {
        let Some(vm) = self
            .active_vm_summary()
            .or_else(|| self.selected_instance())
            .or_else(|| self.instances.first())
            .cloned()
        else {
            self.toast("no VM selected", ToastLevel::Warn);
            return Vec::new();
        };
        let members = self
            .project_entries
            .iter()
            .filter(|entry| entry.vm.as_deref() == Some(vm.name.as_str()))
            .count();
        self.modal = Some(Modal::DeleteVm(DeleteVmModal {
            vm,
            members,
            stage: DeleteVmStage::Confirm,
            error: None,
            scroll: 0,
        }));
        Vec::new()
    }

    /// Inspect and submit the terminal input buffer.
    pub fn submit_input(&mut self) -> Vec<Effect> {
        let command = self.input.clone();
        if command.trim().is_empty() {
            self.input.clear();
            return Vec::new();
        }
        self.input.clear();

        if self.terminal_mode == TerminalMode::Raw {
            self.write_line(&command);
            return Vec::new();
        }

        let cwd = self
            .session
            .as_ref()
            .map(|s| s.project().to_path_buf())
            .or_else(|| self.selected_project().map(|p| p.path.clone()));
        let policy = match self.workspace_policy_for(cwd.as_deref()) {
            Ok(policy) => policy,
            Err(err) => {
                self.toast(
                    format!("workspace guard unavailable: {err:#}"),
                    ToastLevel::Error,
                );
                return Vec::new();
            }
        };
        let inspection =
            self.security_engine
                .inspect_with_policy(&command, cwd.as_deref(), &policy);
        match inspection.decision {
            Decision::Allow => {
                self.write_line(&command);
                if inspection.destructive {
                    self.toast("workspace guard: allowed", ToastLevel::Warn);
                }
            }
            Decision::Warn | Decision::Block => {
                self.modal = Some(Modal::Security(SecurityPrompt {
                    command,
                    project: cwd.unwrap_or_else(|| PathBuf::from(".")),
                    inspection,
                    error: None,
                    scroll: 0,
                }));
            }
        }
        Vec::new()
    }

    pub(super) fn write_line(&mut self, line: &str) -> bool {
        let mut error = None;
        let mut active = false;
        if let Some(session) = self.session.as_mut()
            && !session.is_exited()
        {
            active = true;
            if let Err(err) = session.send(format!("{line}\r").as_bytes()) {
                error = Some(err.to_string());
            }
        }
        if let Some(error) = error {
            self.toast(format!("write failed: {error}"), ToastLevel::Error);
            false
        } else if active {
            self.scroll = 0;
            true
        } else {
            self.toast("no active session", ToastLevel::Warn);
            false
        }
    }

    pub(super) fn write_bytes(&mut self, bytes: &[u8]) {
        let mut error = None;
        let mut active = false;
        if let Some(session) = self.session.as_mut()
            && !session.is_exited()
        {
            active = true;
            if let Err(err) = session.send(bytes) {
                error = Some(err.to_string());
            }
        }
        if let Some(error) = error {
            self.toast(format!("write failed: {error}"), ToastLevel::Error);
        } else if active {
            self.scroll = 0;
        } else {
            self.toast("no active session", ToastLevel::Warn);
        }
    }

    /// Drop a project from the in-memory dashboard right after its deletion so
    /// the user does not have to wait for (or trigger) a project rescan.
    pub fn forget_project_locally(&mut self, path: &Path) {
        self.projects.retain(|project| project.path != path);
        if self.open_project.as_deref() == Some(path) {
            self.leave_project();
        }
        self.refresh_filter();
    }

    /// Reload recent/hidden project state after a background worker changed it.
    pub fn reload_state(&mut self) {
        self.store = StateStore::load(self.config.state_dir.join("state.json"));
    }

    /// Reload global configuration after an editor round-trip.
    pub fn reload_after_edit(&mut self) {
        match Config::load() {
            Ok(config) => self.apply_global_config(config),
            Err(err) => self.toast(format!("config reload failed: {err}"), ToastLevel::Error),
        }
    }

    /// Enter on the dashboard opens the project; on the project screen it
    /// attaches the VM; on the VM screen it opens the selected member.
    pub fn open_selected(&mut self) -> Vec<Effect> {
        match self.screen {
            Screen::Vm => match self.focus {
                Focus::Projects => {
                    let Some(member) = self.vm_members.get(self.vm_member_selected).cloned() else {
                        return Vec::new();
                    };
                    self.open_project_path(member.path);
                    self.attach_project_vm()
                }
                Focus::Snapshots => self.confirm_restore_snapshot(),
                Focus::Tools => self.attach_active_vm(),
                Focus::Health | Focus::Git | Focus::Vm => Vec::new(),
            },
            Screen::Project => match self.focus {
                Focus::Snapshots => self.confirm_restore_snapshot(),
                Focus::Vm | Focus::Projects => self.attach_project_vm(),
                Focus::Health | Focus::Git | Focus::Tools => Vec::new(),
            },
            Screen::Dashboard => {
                let mut effects = self.open_project();
                if self.screen == Screen::Project {
                    effects.extend(self.attach_project_vm());
                }
                effects
            }
        }
    }

    fn open_project_path(&mut self, path: PathBuf) {
        let project = Project::load(path.clone());
        if !self.projects.iter().any(|candidate| candidate.path == path) {
            self.projects.push(project);
        }
        let _ = self.store.touch(&path);
        self.enter_project(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn normal_mode_forwards_nothing_to_the_guest() {
        let (_dir, mut app) = test_app();
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
        assert!(app.terminal_focus);

        // No `i`/`a` in this command: those keys intentionally switch modes.
        for c in "rm -rf /tmp".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Backspace));

        assert!(log.sent().is_empty(), "normal mode is not a passthrough");
        assert!(app.input.is_empty());
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
        assert!(app.modal.is_none());
    }

    #[test]
    fn insert_mode_buffers_input_and_only_enter_sends_it() {
        let (_dir, mut app) = test_app();
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));
        app.attached_vm = None;

        app.handle_key(key(KeyCode::Char('i')));
        for c in "echo hi".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.input, "echo hi");
        assert!(
            log.sent().is_empty(),
            "typing is not forwarded before Enter"
        );

        app.handle_key(key(KeyCode::Enter));
        assert!(app.input.is_empty());
        assert_eq!(log.sent_text(), "echo hi\r");
        assert!(app.modal.is_none(), "a benign command is not flagged");
    }

    #[test]
    fn raw_mode_forwards_keys_without_buffering_or_inspection() {
        let (_dir, mut app) = test_app();
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));
        app.terminal_mode = TerminalMode::Raw;

        for c in "ls".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(app.input.is_empty());
        assert_eq!(log.sent(), b"ls\r");
        assert!(app.modal.is_none());
    }

    #[test]
    fn detach_key_never_reaches_the_guest() {
        let (_dir, mut app) = test_app();
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));
        app.terminal_mode = TerminalMode::Insert;

        let effects = app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));

        assert!(effects.is_empty());
        assert_eq!(app.screen, Screen::Dashboard);
        assert!(
            log.sent().is_empty(),
            "Ctrl-B is a UI chord, not guest input"
        );
        assert!(
            app.session_active(),
            "detaching the view keeps the VM alive"
        );
    }

    #[test]
    fn insert_ctrl_c_clears_the_buffer_and_sends_an_interrupt() {
        let (_dir, mut app) = test_app();
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));
        app.terminal_mode = TerminalMode::Insert;
        app.input = "rm -rf /workspace".into();

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(
            app.input.is_empty(),
            "a secret half-typed command is dropped"
        );
        assert_eq!(log.sent(), vec![0x03]);
        assert!(
            app.modal.is_none(),
            "Ctrl-C never inspects the buffer as a command"
        );
        assert_eq!(app.screen, Screen::Project);
    }

    #[test]
    fn benign_submit_without_session_warns_instead_of_crashing() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.handle_key(key(KeyCode::Char('i')));
        for c in "ls".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(app.toast.is_some());
    }

    #[test]
    fn destructive_submit_opens_security_modal() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.handle_key(key(KeyCode::Char('i')));
        for c in "rm -rf /workspace".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::Security(prompt)) => {
                assert_eq!(prompt.command, "rm -rf /workspace");
                assert!(prompt.inspection.destructive);
            }
            other => panic!("expected security modal, got {other:?}"),
        }
    }

    #[test]
    fn entering_a_project_does_not_start_a_vm_by_itself() {
        let (_dir, mut app) = test_app();
        let mut project = fake_project("demo");
        project.path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project.path).unwrap();
        app.set_projects(vec![project.clone()]);
        let effects = app.open_project();
        assert!(
            effects.is_empty(),
            "opening a project must not boot: {effects:?}"
        );
        assert_eq!(app.screen, Screen::Project);
        assert_eq!(app.project_view, ProjectView::Compact);
        assert_eq!(app.vm.state, VmUiState::Stopped);
        assert_eq!(app.open_project.as_deref(), Some(project.path.as_path()));
    }

    #[test]
    fn enter_on_an_assigned_project_starts_its_vm() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.set_projects(vec![Project {
            name: "demo".into(),
            path: project.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }]);
        app.instances = vec![fake_instance(&app, "rust", project.clone())];
        assign_fake_project(&mut app, &project, "rust");

        let effects = app.open_selected();
        assert!(matches!(
            effects.as_slice(),
            [Effect::StartVm { vm, project: Some(path) }]
                if vm == "rust" && path == &project
        ));
        assert_eq!(app.screen, Screen::Project);
    }

    #[test]
    fn enter_on_an_unassigned_project_opens_the_assign_modal() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.set_projects(vec![Project {
            name: "demo".into(),
            path: project,
            git: GitSummary::default(),
            last_opened: None,
        }]);
        app.instances = vec![fake_instance(
            &app,
            "rust",
            app.config.projects_root.join("demo"),
        )];

        let effects = app.open_selected();
        assert!(effects.is_empty());
        assert!(matches!(app.modal, Some(Modal::AssignProject(_))));
    }
}
