//! Project lifecycle: entering and leaving the project screen, terminal input
//! submission and local list maintenance after background workers run.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::project::{self};
use crate::security::Decision;
use crate::session::state::StateStore;
use crate::vm::instance::InstanceSummary;

use super::*;

impl App {
    /// Open the selected project without booting a VM.
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
        self.open_project = Some(path);
        self.screen = Screen::Project;
        self.project_view = ProjectView::Compact;
        self.terminal_focus = false;
        self.focus = Focus::Instances;
        self.terminal_mode = TerminalMode::Normal;
        self.refresh_instances();
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
        self.project_view = ProjectView::Compact;
        self.terminal_focus = false;
        self.focus = Focus::Projects;
        self.refresh_filter();
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
        let inspection = self.security_engine.inspect(&command, cwd.as_deref());
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

    pub(super) fn workspace_is_inside_instance(&self, instance: &InstanceSummary) -> bool {
        let dir = self.config.instances_dir.join(&instance.name);
        let dir = dir.canonicalize().unwrap_or(dir);
        let workspace = instance
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| instance.workspace.clone());
        workspace.starts_with(&dir)
    }

    /// Reload global and instance configuration after an editor round-trip.
    pub fn reload_after_edit(&mut self) {
        match Config::load() {
            Ok(config) => self.apply_global_config(config),
            Err(err) => self.toast(format!("config reload failed: {err}"), ToastLevel::Error),
        }
    }

    /// Open the selected project or instance.
    ///
    /// On the dashboard this enters the project screen without booting a VM.
    /// On the project screen it attaches/boots the selected instance.
    pub fn open_selected(&mut self) -> Vec<Effect> {
        match self.focus {
            Focus::Snapshots => self.confirm_restore_snapshot(),
            Focus::Instances => self.attach_selected_instance(),
            Focus::Health | Focus::Git => Vec::new(),
            Focus::Projects => {
                if self.screen == Screen::Project {
                    self.open_terminal()
                } else {
                    self.open_project()
                }
            }
        }
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
    fn entering_a_project_does_not_start_a_vm() {
        let (_dir, mut app) = test_app();
        let mut project = fake_project("demo");
        project.path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project.path).unwrap();
        app.set_projects(vec![project.clone()]);
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(
            effects.is_empty(),
            "opening a project must not boot: {effects:?}"
        );
        assert_eq!(app.screen, Screen::Project);
        assert_eq!(app.project_view, ProjectView::Compact);
        assert_eq!(app.instance.state, InstanceState::Stopped);
        assert_eq!(app.open_project.as_deref(), Some(project.path.as_path()));
    }

    #[test]
    fn reopening_the_same_project_through_a_symlink_reuses_the_console() {
        let (dir, mut app) = test_app();
        let real = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("demo-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut project = fake_project("demo");
        project.path = link;
        app.set_projects(vec![project]);

        app.attach_console(Box::new(FakeConsole::new(&real)));
        let effects = app.open_selected();
        assert!(effects.is_empty(), "must not boot a second VM");
        assert_eq!(app.screen, Screen::Project);
    }
}
