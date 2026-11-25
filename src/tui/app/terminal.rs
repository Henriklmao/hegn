//! VM lifecycle from the UI side: starting and attaching consoles, failures and
//! the confirmations that precede destructive VM operations.

use std::path::PathBuf;
use std::time::Instant;

use crate::project;
use crate::session::console::TerminalConsole;

use super::*;

impl App {
    pub(super) fn open_terminal(&mut self) -> Vec<Effect> {
        if self.screen == Screen::Vm {
            return self.attach_active_vm();
        }
        if self.open_project.is_none() && self.session.is_none() {
            self.project_view = ProjectView::Split;
            self.terminal_focus = true;
            self.terminal_mode = TerminalMode::Insert;
            return Vec::new();
        }
        let vm = self
            .active_vm_name()
            .or_else(|| self.session.as_ref().map(|s| s.vm_name().to_string()));
        if let Some(name) = vm.clone() {
            if self
                .session
                .as_ref()
                .is_some_and(|session| session.vm_name() == name)
            {
                self.project_view = ProjectView::Split;
                self.terminal_focus = true;
                self.terminal_mode = TerminalMode::Insert;
                return Vec::new();
            }
            self.attach_vm(&name)
        } else {
            self.start_project_instance()
        }
    }

    pub(super) fn effect_for_open_project(
        &self,
        effect: impl FnOnce(PathBuf) -> Effect,
    ) -> Vec<Effect> {
        self.open_project
            .clone()
            .map(|path| vec![effect(path)])
            .unwrap_or_default()
    }

    pub(super) fn attach_console_external(&mut self) -> Vec<Effect> {
        let vm = self
            .active_vm_name()
            .or_else(|| self.session.as_ref().map(|s| s.vm_name().to_string()));
        let Some(vm) = vm else {
            self.toast("no VM selected", ToastLevel::Warn);
            return Vec::new();
        };
        vec![Effect::AttachConsole { vm }]
    }

    /// Attach the VM of the open project or the VM screen.
    pub(super) fn attach_active_vm(&mut self) -> Vec<Effect> {
        if self.screen == Screen::Vm {
            let Some(vm) = self.vm_open.clone() else {
                self.toast("no VM selected", ToastLevel::Warn);
                return Vec::new();
            };
            if self
                .session
                .as_ref()
                .is_some_and(|session| session.vm_name() == vm)
            {
                self.project_view = ProjectView::Split;
                self.terminal_focus = true;
                self.terminal_mode = TerminalMode::Insert;
                return Vec::new();
            }
            return self.attach_vm(&vm);
        }
        self.attach_project_vm()
    }

    /// Start/attach one VM by name and prepare the embedded terminal.
    pub(super) fn attach_vm(&mut self, name: &str) -> Vec<Effect> {
        if self.vm.state == VmUiState::Starting {
            self.toast("a VM is already starting…", ToastLevel::Warn);
            return Vec::new();
        }
        self.session = None;
        self.attached_vm = None;
        self.pending_launch = None;
        self.provision_running = false;
        self.provision_ready_seen = false;
        self.terminal.clear();
        self.scroll = 0;
        self.input.clear();
        self.terminal_mode = TerminalMode::Normal;
        self.session_closed_reported = false;
        self.vm.state = VmUiState::Starting;
        self.vm.started_at = None;
        self.vm.note = Some(format!("attaching to VM {name}"));
        vec![Effect::AttachVm {
            vm: name.to_string(),
        }]
    }

    pub(super) fn stop_selected_instance(&mut self) -> Vec<Effect> {
        let Some(vm) = self.active_vm_name().or_else(|| {
            self.selected_instance()
                .map(|instance| instance.name.clone())
        }) else {
            self.toast("no VM selected", ToastLevel::Warn);
            return Vec::new();
        };
        vec![Effect::StopVm { vm }]
    }

    pub(super) fn apply_selected_vm(&mut self) -> Vec<Effect> {
        let Some(vm) = self.active_vm_name().or_else(|| {
            self.selected_instance()
                .map(|instance| instance.name.clone())
        }) else {
            self.toast("no VM selected", ToastLevel::Warn);
            return Vec::new();
        };
        vec![Effect::ApplyVm { vm }]
    }

    /// Ask before removing a project from the dashboard.
    ///
    /// The modal shows git state (so uncommitted work is visible) and the VM
    /// that will lose the project (unassigned, never deleted). `y` unregisters
    /// the project and keeps its files; `f` also deletes the project directory.
    pub(super) fn confirm_delete_project(&mut self) -> Vec<Effect> {
        let Some(project) = self.selected_project().cloned() else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };
        let git = project::read_git_summary(&project.path);
        let vms = self
            .project_vm_name(&project.path)
            .map(|vm| vec![vm.to_string()])
            .unwrap_or_default();
        self.modal = Some(Modal::DeleteProject(DeleteProjectModal {
            project,
            git,
            vms,
            scroll: 0,
        }));
        Vec::new()
    }

    pub(super) fn delete_project_effect(
        &mut self,
        modal: DeleteProjectModal,
        delete_files: bool,
    ) -> Vec<Effect> {
        let path = modal.project.path.clone();
        self.toast(
            format!("deleting project {}…", modal.project.name),
            ToastLevel::Info,
        );
        vec![Effect::DeleteProject {
            project: path,
            delete_files,
        }]
    }

    /// Boot the selected project's VM.
    ///
    /// This only *requests* the VM start; the terminal loop owns the runtime and
    /// performs the boot. A missing VM assignment is a hard prompt, never a
    /// local shell.
    pub(super) fn start_project_instance(&mut self) -> Vec<Effect> {
        if self.vm.state == VmUiState::Starting {
            self.toast("a VM is already starting…", ToastLevel::Warn);
            return Vec::new();
        }
        if self.screen == Screen::Vm {
            return self.attach_active_vm();
        }
        let Some(project) = self
            .active_project()
            .cloned()
            .or_else(|| self.selected_project().cloned())
        else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };
        let Some(vm) = self.project_vm_name(&project.path).map(str::to_string) else {
            return self.open_assign_form(project.path, None);
        };

        let attached_to_vm = self
            .session
            .as_ref()
            .is_some_and(|session| session.vm_name() == vm);
        if attached_to_vm && self.session_active() {
            self.enter_project(project.path);
            self.project_view = ProjectView::Split;
            self.terminal_focus = true;
            return Vec::new();
        }

        if let Err(err) = self.store.touch(&project.path) {
            self.toast(
                format!("could not save recent projects: {err}"),
                ToastLevel::Warn,
            );
        }

        self.session = None;
        self.attached_vm = None;
        self.pending_launch = None;
        self.provision_running = false;
        self.provision_ready_seen = false;
        self.terminal.clear();
        self.scroll = 0;
        self.input.clear();
        self.terminal_mode = TerminalMode::Normal;
        self.session_closed_reported = false;
        self.vm.state = VmUiState::Starting;
        self.vm.started_at = None;
        self.vm.note = Some(format!(
            "booting Cloud Hypervisor VM {vm} for {}",
            project.display_path()
        ));
        self.enter_project(project.path.clone());
        self.project_view = ProjectView::Split;
        self.terminal_focus = true;
        vec![Effect::StartVm {
            vm,
            project: Some(project.path),
        }]
    }

    /// Drop the embedded console before another client attaches.
    ///
    /// The relay accepts one frontend at a time: a second `hegn attach` kicks
    /// the TUI's socket, which used to leave the terminal in a half-dead state.
    pub fn detach_console(&mut self) {
        self.session = None;
        self.terminal_mode = TerminalMode::Normal;
    }

    /// Install a console for a successfully started/attached VM.
    pub fn attach_console(&mut self, console: Box<dyn TerminalConsole>) {
        let vm_name = console.vm_name().to_string();
        self.vm.state = VmUiState::Running;
        self.vm.backend = "cloud-hypervisor";
        self.vm.note = Some(format!(
            "{} in {}",
            console.label(),
            console.project().display()
        ));
        self.vm.started_at = Some(Instant::now());
        self.vm.socket = Some(
            self.config
                .vms_dir
                .join(&vm_name)
                .join("run/cloud-hypervisor.sock"),
        );
        self.attached_vm = Some(vm_name.clone());
        if let Some(index) = self
            .instances
            .iter()
            .position(|instance| instance.name == vm_name)
        {
            self.instance_selected = index;
        }
        let project = console.project().to_path_buf();
        self.session = Some(console);
        self.enter_project(project);
        self.project_view = ProjectView::Split;
        self.terminal_focus = true;
        self.terminal_mode = TerminalMode::Normal;
        self.input.clear();
        self.scroll = 0;
        self.terminal.clear();
        self.session_closed_reported = false;
        self.toast(
            "attached — normal mode, press i or a to type",
            ToastLevel::Info,
        );
        self.refresh_vms();
    }

    /// Report a hard VM start/attach failure. No host fallback is attempted.
    ///
    /// The doctor modal opens automatically so the missing prerequisite and the
    /// image download hint are visible without leaving the TUI.
    pub fn fail_start(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.session = None;
        self.attached_vm = None;
        self.pending_launch = None;
        self.provision_running = false;
        self.provision_ready_seen = false;
        self.vm.state = VmUiState::Failed;
        self.vm.started_at = None;
        self.vm.note = Some(error.clone());
        self.toast(format!("VM start failed: {error}"), ToastLevel::Error);
        self.modal = Some(Modal::Doctor(DoctorModal::with_error(
            crate::diagnostics::doctor::run(&self.config),
            error,
        )));
    }

    pub fn session_active(&mut self) -> bool {
        self.session
            .as_mut()
            .map(|session| !session.is_exited())
            .unwrap_or(false)
    }
}

impl App {
    /// Delete the selected VM after its two-stage confirmation.
    pub(super) fn delete_vm_effect(
        &mut self,
        modal: DeleteVmModal,
        delete_files: bool,
    ) -> Vec<Effect> {
        let name = modal.vm.name.clone();
        self.toast(format!("deleting VM {name}…"), ToastLevel::Info);
        vec![Effect::DeleteVm {
            vm: name,
            delete_files,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn f2_requests_the_real_attach_client() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.instances = vec![fake_instance(&app, "demo", project.clone())];
        assign_fake_project(&mut app, &project, "demo");
        app.projects = vec![Project {
            name: "demo".into(),
            path: project.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.open_project = Some(project);
        app.screen = Screen::Project;
        let effects = app.handle_key(key(KeyCode::F(2)));
        assert!(matches!(
            effects.as_slice(),
            [Effect::AttachConsole { vm }] if vm == "demo"
        ));
    }

    #[test]
    fn detaching_drops_the_embedded_console_before_an_external_attach() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        app.terminal_mode = TerminalMode::Insert;
        app.detach_console();
        assert!(app.session.is_none());
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
        assert_eq!(app.vm.state, VmUiState::Running);
    }

    #[test]
    fn missing_vm_never_falls_back_to_host_work() {
        let (_dir, mut app) = test_app();
        app.fail_start("missing VM assets");
        assert!(app.session.is_none());
        assert_eq!(app.vm.state, VmUiState::Failed);
        assert!(!app.session_active());
        assert!(app.toast.as_ref().unwrap().text.contains("VM start failed"));
    }

    #[test]
    fn starting_a_project_instance_resets_the_terminal_mode() {
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
        app.focus = Focus::Vm;
        app.terminal_mode = TerminalMode::Raw;
        let effects = app.start_project_instance();
        assert_eq!(
            app.terminal_mode,
            TerminalMode::Normal,
            "the terminal mode must reset for each VM"
        );
        assert_eq!(app.vm.state, VmUiState::Starting);
        assert!(app.session.is_none());
        match effects.as_slice() {
            [Effect::StartVm { vm, project: p }] => {
                assert_eq!(p.as_deref(), Some(project.as_path()));
                assert_eq!(vm, "rust");
            }
            other => panic!("expected StartVm, got {other:?}"),
        }
    }

    #[test]
    fn a_second_start_is_rejected_while_the_first_is_booting() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.focus = Focus::Vm;
        app.vm.state = VmUiState::Starting;
        let effects = app.start_project_instance();
        assert!(effects.is_empty());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("already starting"))
        );
    }

    #[test]
    fn attaching_a_console_enters_the_terminal_view() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        assert_eq!(app.screen, Screen::Project);
        assert_eq!(app.vm.state, VmUiState::Running);
        assert!(app.session_active());
        assert_eq!(app.attached_vm.as_deref(), Some("fake"));
    }

    #[test]
    fn attaching_a_console_clears_stale_input_and_scrollback() {
        let (_dir, mut app) = test_app();
        app.input = "stale command".into();
        app.scroll = 7;
        app.terminal.push_bytes(b"old output\r\n");
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        assert!(app.input.is_empty());
        assert_eq!(app.scroll, 0);
        assert!(app.terminal.is_empty());
    }

    #[test]
    fn attaching_selects_the_matching_instance_and_starts_in_normal_mode() {
        let (_dir, mut app) = test_app();
        let demo = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&demo).unwrap();
        // Persist the VM and its registry membership so the registry refresh
        // in `enter_project` keeps the selection instead of emptying the list.
        crate::vm::registry::VmRegistry::new(app.config.clone())
            .ensure_for_project(&demo, Harness::Shell)
            .unwrap();
        let instance = fake_instance(&app, "demo", demo.clone());
        install_instances(&mut app, vec![instance]);
        app.terminal_mode = TerminalMode::Insert;

        let log = ConsoleLog::default();
        app.attach_console(log.console_named(demo, "demo"));

        assert_ne!(
            app.vm.state,
            VmUiState::Starting,
            "a successful attach is no longer booting"
        );
        assert_eq!(app.vm.backend, "cloud-hypervisor");
        assert!(app.vm.started_at.is_some());
        assert_eq!(app.selected_instance().unwrap().name, "demo");
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
        assert!(app.terminal_focus);
    }

    #[test]
    fn attaching_is_rejected_while_another_vm_is_booting() {
        let (_dir, mut app) = test_app();
        let inside = app.config.vms_dir.join("demo").join("workspace");
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.vm.state = VmUiState::Starting;

        let effects = app.attach_vm("demo");

        assert!(effects.is_empty());
        assert!(app.session.is_none());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("already starting"))
        );
    }

    #[test]
    fn fail_start_never_falls_back_to_a_host_shell() {
        let (_dir, mut app) = test_app();
        app.fail_start("boom");
        assert!(app.session.is_none());
        assert_eq!(app.vm.state, VmUiState::Failed);
        assert!(!app.session_active());
    }
}
