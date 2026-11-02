//! VM lifecycle from the UI side: starting and attaching consoles, failures and
//! the parts of instance deletion that need a confirmation.

use std::path::PathBuf;
use std::time::Instant;

use crate::project;
use crate::session::console::TerminalConsole;

use super::*;

impl App {
    pub(super) fn open_terminal(&mut self) -> Vec<Effect> {
        if self.open_project.is_none() && self.session.is_none() {
            self.project_view = ProjectView::Split;
            self.terminal_focus = true;
            self.terminal_mode = TerminalMode::Insert;
            return Vec::new();
        }
        let selected = self
            .selected_instance()
            .map(|instance| instance.name.clone());
        if let Some(name) = selected {
            if self
                .session
                .as_ref()
                .is_some_and(|session| session.instance_name() == name)
            {
                self.project_view = ProjectView::Split;
                self.terminal_focus = true;
                self.terminal_mode = TerminalMode::Insert;
                return Vec::new();
            }
            self.attach_selected_instance()
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
        let instance = self
            .selected_instance()
            .map(|instance| instance.name.clone())
            .or_else(|| {
                self.session
                    .as_ref()
                    .map(|session| session.instance_name().to_string())
            });
        let Some(instance) = instance else {
            self.toast("no instance selected", ToastLevel::Warn);
            return Vec::new();
        };
        vec![Effect::AttachConsole { instance }]
    }

    pub(super) fn attach_selected_instance(&mut self) -> Vec<Effect> {
        let Some(instance) = self.selected_instance().cloned() else {
            self.toast("no instance selected", ToastLevel::Warn);
            return Vec::new();
        };
        if self.instance.state == InstanceState::Starting {
            self.toast("a VM is already starting…", ToastLevel::Warn);
            return Vec::new();
        }
        self.session = None;
        self.pending_launch = None;
        self.provision_running = false;
        self.provision_ready_seen = false;
        self.terminal.clear();
        self.scroll = 0;
        self.input.clear();
        self.terminal_mode = TerminalMode::Normal;
        self.session_closed_reported = false;
        self.instance.state = InstanceState::Starting;
        self.instance.started_at = None;
        self.instance.note = Some(format!("attaching to instance {}", instance.name));
        self.enter_project(instance.project.clone());
        self.project_view = ProjectView::Split;
        self.terminal_focus = true;
        vec![Effect::AttachInstance {
            instance: instance.name,
        }]
    }

    pub(super) fn stop_selected_instance(&mut self) -> Vec<Effect> {
        let Some(instance) = self.selected_instance().cloned() else {
            self.toast("no instance selected", ToastLevel::Warn);
            return Vec::new();
        };
        vec![Effect::StopInstance {
            instance: instance.name,
        }]
    }

    pub(super) fn confirm_delete_instance(&mut self) -> Vec<Effect> {
        let Some(instance) = self.selected_instance().cloned() else {
            self.toast("no instance selected", ToastLevel::Warn);
            return Vec::new();
        };
        self.modal = Some(Modal::DeleteInstance(DeleteInstanceModal {
            instance,
            stage: DeleteInstanceStage::Confirm,
            error: None,
            scroll: 0,
        }));
        Vec::new()
    }

    /// Ask before removing a project from the dashboard.
    ///
    /// The modal shows git state (so uncommitted work is visible) and the
    /// instances that will be stopped and deleted. `y` unregisters the project
    /// and keeps its files; `f` also deletes the project directory.
    pub(super) fn confirm_delete_project(&mut self) -> Vec<Effect> {
        let Some(project) = self.selected_project().cloned() else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };
        let git = project::read_git_summary(&project.path);
        let instances = self
            .instances
            .iter()
            .filter(|instance| {
                crate::vm::instance::same_project(&instance.project, &project.path)
                    || crate::vm::instance::same_project(&instance.workspace, &project.path)
            })
            .map(|instance| instance.name.clone())
            .collect();
        self.modal = Some(Modal::DeleteProject(DeleteProjectModal {
            project,
            git,
            instances,
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

    /// Boot the project's default instance.
    ///
    /// This only *requests* the VM start; the terminal loop owns the runtime and
    /// performs the boot. A missing VM is a hard error, never a local shell.
    pub(super) fn start_project_instance(&mut self) -> Vec<Effect> {
        if self.instance.state == InstanceState::Starting {
            self.toast("a VM is already starting…", ToastLevel::Warn);
            return Vec::new();
        }
        let Some(project) = self.selected_project().cloned() else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };

        let attached_to_project = self.session.as_ref().is_some_and(|session| {
            crate::vm::instance::same_project(session.project(), &project.path)
        });
        if attached_to_project && self.session_active() {
            self.enter_project(project.path);
            self.project_view = ProjectView::Split;
            self.terminal_focus = true;
            return Vec::new();
        }

        let meta = project::read_meta(&project.path);
        let harness = meta.harness.unwrap_or(self.config.harness);

        if let Err(err) = self.store.touch(&project.path) {
            self.toast(
                format!("could not save recent projects: {err}"),
                ToastLevel::Warn,
            );
        }

        self.session = None;
        self.pending_launch = None;
        self.provision_running = false;
        self.provision_ready_seen = false;
        self.terminal.clear();
        self.scroll = 0;
        self.input.clear();
        self.terminal_mode = TerminalMode::Normal;
        self.session_closed_reported = false;
        self.instance.state = InstanceState::Starting;
        self.instance.started_at = None;
        self.instance.note = Some(format!(
            "booting Cloud Hypervisor VM for {}",
            project.display_path()
        ));
        self.enter_project(project.path.clone());
        self.project_view = ProjectView::Split;
        self.terminal_focus = true;
        vec![Effect::StartProjectInstance {
            project: project.path,
            harness,
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
        self.instance.state = InstanceState::Running;
        self.instance.backend = "cloud-hypervisor";
        self.instance.note = Some(format!(
            "{} in {}",
            console.label(),
            console.project().display()
        ));
        self.instance.started_at = Some(Instant::now());
        self.instance.socket = Some(
            self.config
                .instances_dir
                .join(console.instance_name())
                .join("run/cloud-hypervisor.sock"),
        );
        if let Some(index) = self
            .instance_visible
            .iter()
            .position(|&index| self.instances[index].name == console.instance_name())
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
        self.refresh_instances();
    }

    /// Report a hard VM start/attach failure. No host fallback is attempted.
    ///
    /// The doctor modal opens automatically so the missing prerequisite and the
    /// image download hint are visible without leaving the TUI.
    pub fn fail_start(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.session = None;
        self.pending_launch = None;
        self.provision_running = false;
        self.provision_ready_seen = false;
        self.instance.state = InstanceState::Failed;
        self.instance.started_at = None;
        self.instance.note = Some(error.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn f2_requests_the_real_attach_client() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.screen = Screen::Project;
        let effects = app.handle_key(key(KeyCode::F(2)));
        assert!(matches!(
            effects.as_slice(),
            [Effect::AttachConsole { instance }] if instance == "demo"
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
        assert_eq!(app.instance.state, InstanceState::Running);
    }

    #[test]
    fn missing_vm_never_falls_back_to_host_work() {
        let (_dir, mut app) = test_app();
        app.fail_start("missing VM assets");
        assert!(app.session.is_none());
        assert_eq!(app.instance.state, InstanceState::Failed);
        assert!(!app.session_active());
        assert!(app.toast.as_ref().unwrap().text.contains("VM start failed"));
    }

    #[test]
    fn starting_a_project_instance_resets_the_terminal_mode() {
        let (_dir, mut app) = test_app();
        let mut project = fake_project("demo");
        project.path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project.path).unwrap();
        app.set_projects(vec![project.clone()]);
        app.screen = Screen::Project;
        app.focus = Focus::Instances;
        app.terminal_mode = TerminalMode::Raw;
        let effects = app.start_project_instance();
        assert_eq!(
            app.terminal_mode,
            TerminalMode::Normal,
            "the terminal mode must reset for each VM"
        );
        assert_eq!(app.instance.state, InstanceState::Starting);
        assert!(app.session.is_none());
        match effects.as_slice() {
            [
                Effect::StartProjectInstance {
                    project: p,
                    harness,
                },
            ] => {
                assert_eq!(p, &project.path);
                assert_eq!(*harness, app.config.harness);
            }
            other => panic!("expected StartProjectInstance, got {other:?}"),
        }
    }

    #[test]
    fn a_second_start_is_rejected_while_the_first_is_booting() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.focus = Focus::Instances;
        app.instance.state = InstanceState::Starting;
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
        assert_eq!(app.instance.state, InstanceState::Running);
        assert!(app.session_active());
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
        // Persist both instances so the registry refresh in `enter_project`
        // keeps the selection instead of emptying the list.
        let store = crate::vm::instance_config::InstanceStore::new(app.config.clone());
        for (name, workspace) in [
            ("demo", demo.join("workspace")),
            ("demo-1", demo.join("clone-1")),
        ] {
            store
                .save(&crate::vm::instance_config::InstanceConfig {
                    name: name.into(),
                    project: demo.clone(),
                    workspace,
                    harness: Harness::Shell,
                    ..crate::vm::instance_config::InstanceConfig::default()
                })
                .unwrap();
        }
        let mut first = fake_instance(&app, "demo", demo.join("workspace"));
        first.project = demo.clone();
        let mut clone = fake_instance(&app, "demo-1", demo.join("clone-1"));
        clone.project = demo.clone();
        install_instances(&mut app, vec![first, clone]);
        app.terminal_mode = TerminalMode::Insert;

        let log = ConsoleLog::default();
        app.attach_console(log.console_named(demo, "demo-1"));

        assert_ne!(
            app.instance.state,
            InstanceState::Starting,
            "a successful attach is no longer booting"
        );
        assert_eq!(app.instance.backend, "cloud-hypervisor");
        assert!(app.instance.started_at.is_some());
        assert_eq!(app.selected_instance().unwrap().name, "demo-1");
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
        assert!(app.terminal_focus);
    }

    #[test]
    fn attaching_is_rejected_while_another_vm_is_booting() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.instance.state = InstanceState::Starting;

        let effects = app.attach_selected_instance();

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
        assert_eq!(app.instance.state, InstanceState::Failed);
        assert!(!app.session_active());
    }
}
