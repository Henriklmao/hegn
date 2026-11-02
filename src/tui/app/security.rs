//! Security engine access and the prompt that guards flagged terminal commands.

use crossterm::event::{KeyCode, KeyEvent};

use crate::config::SecurityMode;
use crate::project;
use crate::security::SecurityEngine;

use super::*;

impl App {
    pub(super) fn handle_security_key(
        &mut self,
        key: KeyEvent,
        prompt: SecurityPrompt,
    ) -> Vec<Effect> {
        match key.code {
            // Allowing a flagged command is destructive; Enter deliberately
            // falls through to the catch-all and leaves the prompt open.
            KeyCode::Char('a') => {
                if self.write_line(&prompt.command) {
                    self.toast("command approved once", ToastLevel::Warn);
                }
                Vec::new()
            }
            KeyCode::Char('c') => {
                match project::create_checkpoint(&prompt.project, "before flagged command") {
                    Ok(branch) => {
                        if self.write_line(&prompt.command) {
                            self.toast(
                                format!("checkpoint {branch}; command approved"),
                                ToastLevel::Info,
                            );
                        } else {
                            self.toast(
                                format!(
                                    "checkpoint {branch} created, but the command was not sent"
                                ),
                                ToastLevel::Warn,
                            );
                        }
                    }
                    Err(err) => {
                        let mut prompt = prompt;
                        prompt.error = Some(err.to_string());
                        self.modal = Some(Modal::Security(prompt));
                    }
                }
                Vec::new()
            }
            KeyCode::Char('d') | KeyCode::Esc => {
                self.toast("command blocked", ToastLevel::Warn);
                Vec::new()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.modal = Some(Modal::Security(prompt));
                self.scroll_modal(1);
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.modal = Some(Modal::Security(prompt));
                self.scroll_modal(-1);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.modal = Some(Modal::Security(prompt));
                self.scroll_modal_page(1);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.modal = Some(Modal::Security(prompt));
                self.scroll_modal_page(-1);
                Vec::new()
            }
            _ => {
                self.modal = Some(Modal::Security(prompt));
                Vec::new()
            }
        }
    }

    /// Test/uploads access to the security engine.
    pub fn security_engine(&self) -> &SecurityEngine {
        &self.security_engine
    }

    pub fn set_security_mode(&mut self, mode: SecurityMode) {
        self.config.security = mode;
        self.security_engine = SecurityEngine::new(mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecurityMode;
    use crate::security::Decision;
    use crate::tui::app::test_support::*;

    fn prompt(app: &App, command: &str, project: PathBuf) -> SecurityPrompt {
        SecurityPrompt {
            command: command.into(),
            project,
            inspection: app.security_engine().inspect(command, None),
            error: None,
            scroll: 0,
        }
    }

    fn git_repo(path: &std::path::Path) {
        std::fs::create_dir_all(path).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(path.join("AGENTS.md"), "# demo\n").unwrap();
    }

    #[test]
    fn security_prompt_only_explicit_allow_writes_the_command() {
        let (_dir, mut app) = test_app();
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));
        app.modal = Some(Modal::Security(prompt(
            &app,
            "rm -rf /workspace",
            PathBuf::from("/tmp"),
        )));

        // Enter, space and `y` are not consent while the prompt is open.
        for code in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Char('y')] {
            app.handle_key(key(code));
            assert!(
                matches!(app.modal, Some(Modal::Security(_))),
                "{code:?} must not dismiss the security prompt"
            );
            assert!(log.sent().is_empty(), "{code:?} must not reach the guest");
        }

        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.modal.is_none());
        assert_eq!(log.sent_text(), "rm -rf /workspace\r");
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("approved once"))
        );
    }

    #[test]
    fn security_deny_and_esc_block_without_writing() {
        let (_dir, mut app) = test_app();
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));

        app.modal = Some(Modal::Security(prompt(
            &app,
            "rm -rf /workspace",
            PathBuf::from("/tmp"),
        )));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert!(log.sent().is_empty());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("blocked"))
        );

        app.modal = Some(Modal::Security(prompt(
            &app,
            "git reset --hard HEAD~3",
            PathBuf::from("/tmp"),
        )));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.modal.is_none());
        assert!(log.sent().is_empty(), "deny must never reach the guest");
    }

    #[test]
    fn warn_mode_still_prompts_and_needs_an_explicit_allow() {
        let (_dir, mut app) = test_app();
        app.set_security_mode(SecurityMode::Warn);
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));

        app.handle_key(key(KeyCode::Char('i')));
        for c in "rm -rf /workspace".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));

        match &app.modal {
            Some(Modal::Security(prompt)) => {
                assert_eq!(prompt.inspection.decision, Decision::Warn);
                assert!(prompt.inspection.destructive);
            }
            other => panic!("warn mode must still ask, got {other:?}"),
        }
        assert!(log.sent().is_empty());
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Security(_))));
        assert!(log.sent().is_empty());
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(log.sent_text(), "rm -rf /workspace\r");
    }

    #[test]
    fn off_mode_allows_but_keeps_the_destructive_workspace_guard() {
        let (_dir, mut app) = test_app();
        app.set_security_mode(SecurityMode::Off);
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));

        app.handle_key(key(KeyCode::Char('i')));
        for c in "rm -rf /workspace".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(app.modal.is_none(), "off mode does not prompt");
        assert_eq!(log.sent_text(), "rm -rf /workspace\r");
        let toast = app.toast.as_ref().expect("workspace guard toast");
        assert_eq!(toast.level, ToastLevel::Warn);
        assert!(toast.text.contains("workspace guard"), "{}", toast.text);
    }

    #[test]
    fn checkpoint_writes_a_branch_before_allowing_the_command() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        git_repo(&project);
        let log = ConsoleLog::default();
        app.attach_console(log.console(project.clone()));
        app.modal = Some(Modal::Security(prompt(
            &app,
            "rm -rf /workspace",
            project.clone(),
        )));

        app.handle_key(key(KeyCode::Char('c')));

        assert!(app.modal.is_none());
        assert_eq!(log.sent_text(), "rm -rf /workspace\r");
        let toast = app.toast.as_ref().expect("checkpoint toast");
        assert!(toast.text.contains("checkpoint"), "{}", toast.text);
        assert!(toast.text.contains("approved"), "{}", toast.text);
        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["branch", "--list", "hegn/checkpoint-*"])
            .output()
            .unwrap();
        assert!(branches.status.success());
        assert!(
            !String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "the safety branch must exist"
        );
    }

    #[test]
    fn security_prompt_deny_closes_modal() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.modal.is_none());
    }

    #[test]
    fn security_allow_without_session_warns() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.modal.is_none());
        let toast = app.toast.as_ref().expect("a warning toast");
        assert!(
            toast.text.contains("no active session"),
            "approval must not hide the write failure: {}",
            toast.text
        );
    }

    #[test]
    fn checkpoint_for_non_git_project_keeps_modal_with_error() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Char('c')));
        match &app.modal {
            Some(Modal::Security(prompt)) => assert!(prompt.error.is_some()),
            other => panic!("expected security modal, got {other:?}"),
        }
    }
}
