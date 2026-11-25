//! Agent launch requests: queueing after boot, releasing when provisioning is
//! done, and resolving commands for the active VM.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::launch::{self, LaunchSpec};
use crate::config::Harness;
use crate::project;

use super::*;

impl App {
    /// Queue the configured command after the TUI started a VM.
    pub fn queue_auto_launch(&mut self, vm: &str, harness: Harness, project: &Path, started: bool) {
        if !started {
            return;
        }
        // Clones export their private workspace; project-level launch settings
        // live next to the original project.
        let meta_project = self
            .instances
            .iter()
            .find(|candidate| candidate.name == vm)
            .map(|candidate| candidate.project.as_path())
            .unwrap_or(project);
        let meta = project::read_meta(meta_project);
        // The project can override auto-start, so a chatty agent does not take
        // over every VM the user boots just to run one command.
        if !meta.auto_start.unwrap_or(self.config.launch.auto_start) {
            return;
        }
        if meta.launch.is_none() && self.config.launch.default.is_none() {
            return;
        }
        let spec = match launch::resolve(
            &self.config,
            harness,
            meta.launch.as_deref(),
            meta.integration,
        ) {
            Ok(spec) => spec,
            Err(err) => {
                self.toast(format!("launch skipped: {err}"), ToastLevel::Warn);
                return;
            }
        };
        // A shell project already lands in the embedded maintenance terminal;
        // do not immediately hand it to an external integration.
        if spec.is_shell() {
            return;
        }
        self.pending_launch = Some(PendingLaunch {
            spec,
            vm: vm.to_string(),
            project: project.to_path_buf(),
            since: Instant::now(),
            provision_seen: self.provision_running,
            ready: self.provision_ready_seen,
        });
    }

    /// Take a queued launch once provisioning is done (or clearly not running).
    pub fn take_due_launch(&mut self) -> Option<(LaunchSpec, String, PathBuf)> {
        let pending = self.pending_launch.as_ref()?;
        // A cloud image may take a while to reach cloud-init. Wait for the
        // explicit ready marker instead of racing the installer; images without
        // cloud-init only need enough time for the shell prompt.
        let grace = if self.config.cloud_init {
            Duration::from_secs(5 * 60)
        } else {
            Duration::from_secs(5)
        };
        let due = pending.ready || (!pending.provision_seen && pending.since.elapsed() >= grace);
        if !due {
            return None;
        }
        let pending = self.pending_launch.take()?;
        Some((pending.spec, pending.vm, pending.project))
    }

    /// Launch a command for the active VM right now.
    pub fn request_launch(&mut self, selector: Option<&str>) -> Vec<Effect> {
        let Some(vm) = self.active_vm_summary().cloned() else {
            self.toast("no VM selected", ToastLevel::Warn);
            return Vec::new();
        };
        let project = self
            .open_project
            .clone()
            .or_else(|| self.selected_project().map(|project| project.path.clone()))
            .unwrap_or_else(|| vm.project.clone());
        let meta = if self.open_project.as_deref() == Some(project.as_path()) {
            self.project_meta.clone()
        } else {
            project::read_meta(&project)
        };
        let spec = match launch::resolve(
            &self.config,
            vm.harness,
            selector.or(meta.launch.as_deref()),
            meta.integration,
        ) {
            Ok(spec) => spec,
            Err(err) => {
                self.toast(format!("launch failed: {err}"), ToastLevel::Error);
                return Vec::new();
            }
        };
        self.pending_launch = None;
        vec![Effect::LaunchAgent {
            spec,
            vm: vm.name,
            project,
        }]
    }

    /// Fork the open project into a clone or worktree (the VM screen's fork
    /// action). The form picks the mode.
    pub(super) fn open_fork_workspace(&mut self) -> Vec<Effect> {
        self.confirm_fork_workspace(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectMeta;
    use crate::tui::app::test_support::*;

    fn assigned_app() -> (tempfile::TempDir, App, PathBuf) {
        let (dir, mut app) = test_app();
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
        app.open_project = Some(project.clone());
        app.screen = Screen::Project;
        (dir, app, project)
    }

    #[test]
    fn auto_launch_waits_for_provisioning_then_releases_the_command() {
        let (_dir, mut app) = test_app();
        app.config.launch.auto_start = true;
        app.queue_auto_launch(
            "demo",
            Harness::Opencode,
            Path::new("/missing/project"),
            true,
        );
        let pending = app.pending_launch.as_ref().expect("queued");
        assert_eq!(pending.spec.command, "opencode");
        assert!(app.take_due_launch().is_none());
        app.provision_started();
        assert!(app.take_due_launch().is_none());
        app.provision_ready();
        let (spec, vm, _project) = app.take_due_launch().expect("ready");
        assert_eq!(spec.command, "opencode");
        assert_eq!(vm, "demo");
        assert!(app.take_due_launch().is_none());
    }

    #[test]
    fn a_ready_marker_that_arrives_before_started_still_launches() {
        let (_dir, mut app) = test_app();
        app.config.launch.auto_start = true;
        // The provision watcher can win the race against the console attach.
        app.provision_ready();
        app.queue_auto_launch(
            "demo",
            Harness::Opencode,
            Path::new("/missing/project"),
            true,
        );
        assert!(
            app.pending_launch
                .as_ref()
                .is_some_and(|pending| pending.ready)
        );
        assert!(app.take_due_launch().is_some());
    }

    #[test]
    fn shell_projects_keep_the_embedded_maintenance_terminal() {
        let (_dir, mut app) = test_app();
        app.config.launch.auto_start = true;
        app.queue_auto_launch("demo", Harness::Shell, Path::new("/missing/project"), true);
        assert!(app.pending_launch.is_none());
    }

    #[test]
    fn project_auto_start_override_wins_over_the_global_setting() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        project::write_meta(
            &project,
            &ProjectMeta {
                auto_start: Some(false),
                ..ProjectMeta::default()
            },
        )
        .unwrap();
        app.config.launch.auto_start = true;

        app.queue_auto_launch("demo", Harness::Opencode, &project, true);
        assert!(app.pending_launch.is_none());
    }

    #[test]
    fn attaching_to_a_running_vm_does_not_auto_launch() {
        let (_dir, mut app) = test_app();
        app.config.launch.auto_start = true;
        app.queue_auto_launch(
            "demo",
            Harness::Opencode,
            Path::new("/missing/project"),
            false,
        );
        assert!(app.pending_launch.is_none());
    }

    #[test]
    fn auto_launch_without_a_configured_command_is_a_no_op() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.config.launch.auto_start = true;
        app.config.launch.default = None;

        app.queue_auto_launch("demo", Harness::Opencode, &project, true);

        assert!(app.pending_launch.is_none());
        assert!(app.toast.is_none());
    }

    #[test]
    fn an_unknown_project_launch_command_warns_instead_of_queueing() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        project::write_meta(
            &project,
            &ProjectMeta {
                launch: Some("missing-command".into()),
                ..ProjectMeta::default()
            },
        )
        .unwrap();
        app.config.launch.auto_start = true;

        app.queue_auto_launch("demo", Harness::Opencode, &project, true);

        assert!(app.pending_launch.is_none());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("launch skipped"))
        );
    }

    #[test]
    fn request_launch_requires_an_assigned_vm_and_resolves_its_harness() {
        let (_dir, mut app) = test_app();
        let effects = app.request_launch(None);
        assert!(effects.is_empty());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("no VM selected"))
        );

        let (_dir, mut app, project) = assigned_app();
        app.config.launch.default = Some(crate::agent::launch::HARNESS.to_string());
        app.pending_launch = None;

        let effects = app.request_launch(None);

        let [
            Effect::LaunchAgent {
                spec,
                vm,
                project: launched,
            },
        ] = effects.as_slice()
        else {
            panic!("expected one launch effect, got {effects:?}");
        };
        assert_eq!(vm, "rust");
        assert_eq!(launched, &project);
        assert_eq!(spec.name, Harness::Shell.label());
        assert!(
            app.pending_launch.is_none(),
            "a manual launch clears the queue"
        );
    }

    #[test]
    fn fork_action_opens_the_workspace_form() {
        let (_dir, mut app, _project) = assigned_app();
        app.handle_key(key(KeyCode::Char('f')));
        match &app.modal {
            Some(Modal::ForkWorkspace(form)) => {
                assert_eq!(form.parent, "demo");
                assert_eq!(form.vm, "rust");
            }
            other => panic!("expected fork form, got {other:?}"),
        }
    }
}
