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
    use crate::tui::app::test_support::*;

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
