//! Security engine access, the prompt that guards flagged terminal commands and
//! the per-command workspace policy derived from the active VM.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};

use crate::config::SecurityMode;
use crate::project::{self, ProjectRegistry};
use crate::security::{GuestWorkspacePolicy, SecurityEngine};
use crate::vm::registry::VmRegistry;

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

    /// Derive the security mode and workspace policy for one command.
    ///
    /// Nothing is cached: attaching, detaching, applying a VM or editing a
    /// `vm.toml` security mode must be visible on the next command. The
    /// attached VM wins over the project's assignment; an unassigned project
    /// falls back to the global mode and the configured guest base.
    pub fn command_security(
        &self,
        project: Option<&Path>,
    ) -> anyhow::Result<(SecurityMode, GuestWorkspacePolicy)> {
        if let Some(vm) = &self.attached_vm {
            let manager = VmRegistry::new(self.config.clone()).manager_by_name(vm)?;
            return Ok((manager.security_mode(), manager.workspace_policy()?));
        }
        if let Some(project) = project {
            let canonical = project
                .canonicalize()
                .unwrap_or_else(|_| project.to_path_buf());
            let registry = ProjectRegistry::load(&self.config.state_dir)?;
            if let Some(entry) = registry.find_by_path(&canonical)
                && let Some(vm) = &entry.vm
            {
                let manager = VmRegistry::new(self.config.clone()).manager_by_name(vm)?;
                return Ok((manager.security_mode(), manager.workspace_policy()?));
            }
        }
        Ok((
            self.config.security,
            GuestWorkspacePolicy::new(&self.config.guest_projects_base)?,
        ))
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
        app.attached_vm = None;

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
        app.attached_vm = None;

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

    fn vm_project_path(app: &App, name: &str) -> PathBuf {
        let path = app.config.projects_root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn save_vm(app: &App, name: &str) {
        crate::vm::config::VmStore::new(&app.config)
            .save(&crate::vm::config::VmConfig::from_config(
                &app.config,
                name,
                Some(crate::config::Harness::Shell),
            ))
            .unwrap();
    }

    fn save_vm_with_security(app: &App, name: &str, mode: SecurityMode) {
        let mut vm = crate::vm::config::VmConfig::from_config(
            &app.config,
            name,
            Some(crate::config::Harness::Shell),
        );
        vm.security = mode;
        crate::vm::config::VmStore::new(&app.config)
            .save(&vm)
            .unwrap();
    }

    fn register_project(app: &mut App, name: &str, path: &Path, vm: Option<&str>) {
        let mut registry = crate::project::ProjectRegistry::load(&app.config.state_dir).unwrap();
        registry
            .upsert(crate::project::ProjectEntry {
                name: name.into(),
                path: path.to_path_buf(),
                vm: None,
                read_only: false,
                kind: crate::project::ProjectKind::External,
                parent: None,
                managed: false,
                hidden: false,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        if let Some(vm) = vm {
            registry.assign(name, vm, false).unwrap();
        }
        app.refresh_project_entries();
    }

    #[test]
    fn assigned_project_policy_covers_member_and_workspace_and_blocks_recursive_delete() {
        let (_dir, mut app) = test_app();
        save_vm(&app, "rust");
        let path = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &path, Some("rust"));

        let (_, policy) = app.command_security(Some(&path)).unwrap();
        assert!(policy.contains("/projects/parser"));
        assert!(policy.contains("/workspace"));
        assert_eq!(policy.roots(), vec!["/projects/parser", "/workspace"]);

        let inspection =
            app.security_engine()
                .inspect_with_policy("rm -rf /projects/parser", None, &policy);
        assert_eq!(inspection.decision, Decision::Block);
        assert!(inspection.destructive);
    }

    #[test]
    fn unassigned_project_falls_back_to_workspace_only() {
        let (_dir, mut app) = test_app();
        let path = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &path, None);

        let (_, policy) = app.command_security(Some(&path)).unwrap();
        assert_eq!(policy.roots(), vec!["/workspace"]);
        assert!(!policy.contains("/projects/parser"));
    }

    #[test]
    fn attached_vm_takes_precedence_over_the_command_project() {
        let (_dir, mut app) = test_app();
        save_vm(&app, "rust");
        let assigned = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &assigned, Some("rust"));
        let unassigned = vm_project_path(&app, "other");
        register_project(&mut app, "other", &unassigned, None);

        app.attached_vm = Some("rust".into());
        let (_, policy) = app.command_security(Some(&unassigned)).unwrap();
        assert!(policy.contains("/projects/parser"));
        assert!(!policy.contains("/projects/other"));
    }

    #[test]
    fn unreadable_registry_fails_closed_without_writing() {
        let (_dir, mut app) = test_app();
        std::fs::create_dir_all(&app.config.state_dir).unwrap();
        std::fs::write(app.config.state_dir.join("projects.toml"), "not = [valid").unwrap();
        let log = ConsoleLog::default();
        app.session = Some(log.console("/projects/parser"));
        app.input = "rm -rf /projects/parser".into();

        let effects = app.submit_input();

        assert!(effects.is_empty());
        assert!(log.sent().is_empty(), "a failed policy must not write");
        let toast = app.toast.as_ref().expect("a refusal toast");
        assert_eq!(toast.level, ToastLevel::Error);
        assert!(
            toast.text.starts_with("command guard unavailable: "),
            "{}",
            toast.text
        );
    }

    #[test]
    fn security_mode_change_still_applies_after_policy_derivation() {
        let (_dir, mut app) = test_app();
        app.set_security_mode(SecurityMode::Off);
        assert_eq!(app.security_engine().mode(), SecurityMode::Off);
        let log = ConsoleLog::default();
        app.session = Some(log.console("/projects/demo"));
        app.input = "rm -rf /workspace".into();

        app.submit_input();

        assert_eq!(log.sent_text(), "rm -rf /workspace\r");
        assert!(app.modal.is_none());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("workspace guard"))
        );
    }

    #[test]
    fn vm_security_off_overrides_a_global_enforce_mode() {
        let (_dir, mut app) = test_app();
        app.config.security = SecurityMode::Enforce;
        save_vm_with_security(&app, "rust", SecurityMode::Off);
        let path = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &path, Some("rust"));
        let log = ConsoleLog::default();
        app.session = Some(log.console(path));
        app.input = "rm -rf /projects/parser".into();

        app.submit_input();

        assert_eq!(log.sent_text(), "rm -rf /projects/parser\r");
        assert!(app.modal.is_none());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("workspace guard"))
        );
    }

    #[test]
    fn vm_security_enforce_overrides_a_global_off_mode() {
        let (_dir, mut app) = test_app();
        app.config.security = SecurityMode::Off;
        save_vm_with_security(&app, "rust", SecurityMode::Enforce);
        let path = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &path, Some("rust"));
        let log = ConsoleLog::default();
        app.session = Some(log.console(path));
        app.input = "rm -rf /projects/parser".into();

        app.submit_input();

        assert!(log.sent().is_empty(), "the VM's enforce mode must block");
        match &app.modal {
            Some(Modal::Security(prompt)) => {
                assert_eq!(prompt.inspection.decision, Decision::Block);
            }
            other => panic!("expected the security prompt, got {other:?}"),
        }
    }

    #[test]
    fn attached_vm_security_mode_takes_precedence_over_the_command_project() {
        let (_dir, mut app) = test_app();
        save_vm_with_security(&app, "attached", SecurityMode::Off);
        save_vm_with_security(&app, "assigned", SecurityMode::Enforce);
        let attached_path = vm_project_path(&app, "other");
        register_project(&mut app, "other", &attached_path, Some("attached"));
        let assigned_path = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &assigned_path, Some("assigned"));
        app.attached_vm = Some("attached".into());

        let (mode, policy) = app.command_security(Some(&assigned_path)).unwrap();

        assert_eq!(mode, SecurityMode::Off);
        assert!(policy.contains("/projects/other"));
        assert!(!policy.contains("/projects/parser"));
    }

    #[test]
    fn unassigned_project_uses_the_global_security_mode() {
        let (_dir, mut app) = test_app();
        app.config.security = SecurityMode::Warn;
        let path = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &path, None);

        let (mode, policy) = app.command_security(Some(&path)).unwrap();

        assert_eq!(mode, SecurityMode::Warn);
        assert_eq!(policy.roots(), vec!["/workspace"]);
    }

    #[test]
    fn unreadable_vm_config_fails_closed_without_writing() {
        let (_dir, mut app) = test_app();
        let path = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &path, Some("rust"));
        std::fs::create_dir_all(app.config.vms_dir.join("rust")).unwrap();
        std::fs::write(app.config.vms_dir.join("rust/vm.toml"), "not = [valid").unwrap();
        let log = ConsoleLog::default();
        app.session = Some(log.console(path));
        app.input = "rm -rf /projects/parser".into();

        app.submit_input();

        assert!(log.sent().is_empty(), "a broken VM config must not write");
        let toast = app.toast.as_ref().expect("a refusal toast");
        assert_eq!(toast.level, ToastLevel::Error);
        assert!(
            toast.text.starts_with("command guard unavailable: "),
            "{}",
            toast.text
        );
    }

    #[test]
    fn editing_vm_security_takes_effect_on_the_next_command() {
        let (_dir, mut app) = test_app();
        save_vm_with_security(&app, "rust", SecurityMode::Off);
        let path = vm_project_path(&app, "parser");
        register_project(&mut app, "parser", &path, Some("rust"));

        assert_eq!(
            app.command_security(Some(&path)).unwrap().0,
            SecurityMode::Off
        );

        let store = crate::vm::config::VmStore::new(&app.config);
        let mut vm = store.load("rust").unwrap();
        vm.security = SecurityMode::Enforce;
        store.save(&vm).unwrap();
        app.refresh_project_entries();

        assert_eq!(
            app.command_security(Some(&path)).unwrap().0,
            SecurityMode::Enforce
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
