//! Agent launch requests: queueing after boot, releasing when provisioning is
//! done, and resolving commands for the selected instance.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::launch::{self, LaunchSpec};
use crate::config::Harness;
use crate::project;

use super::*;

impl App {
    /// Queue the configured command after the TUI started a VM with Enter.
    pub fn queue_auto_launch(
        &mut self,
        instance: &str,
        harness: Harness,
        project: &Path,
        started: bool,
    ) {
        if !started {
            return;
        }
        // Clones export their private workspace; project-level launch settings
        // live next to the original project.
        let meta_project = self
            .instances
            .iter()
            .find(|candidate| candidate.name == instance)
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
            instance: instance.to_string(),
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
        Some((pending.spec, pending.instance, pending.project))
    }

    /// Launch a command for the selected instance right now.
    pub fn request_launch(&mut self, selector: Option<&str>) -> Vec<Effect> {
        let Some(instance) = self.selected_instance().cloned() else {
            self.toast("no instance selected", ToastLevel::Warn);
            return Vec::new();
        };
        let meta = if self.open_project.as_deref() == Some(instance.project.as_path()) {
            self.project_meta.clone()
        } else {
            project::read_meta(&instance.project)
        };
        let spec = match launch::resolve(
            &self.config,
            instance.harness,
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
            instance: instance.name,
            project: instance.workspace,
        }]
    }

    pub(super) fn open_multi_agent(&mut self) -> Vec<Effect> {
        let Some(session) = self.session.as_ref() else {
            self.toast(
                "open a project first — multi-agent clones live inside its terminal",
                ToastLevel::Warn,
            );
            return Vec::new();
        };
        let instance = session.instance_name().to_string();
        let harness = self
            .instances
            .iter()
            .find(|entry| entry.name == instance)
            .map(|entry| entry.harness)
            .unwrap_or(self.config.harness);
        self.modal = Some(Modal::MultiAgent(MultiAgentForm::new(instance, harness)));
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectMeta;
    use crate::tui::app::test_support::*;

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
        let (spec, instance, _project) = app.take_due_launch().expect("ready");
        assert_eq!(spec.command, "opencode");
        assert_eq!(instance, "demo");
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
    fn multi_agent_form_submits_clone_effect() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        app.handle_key(key(KeyCode::Char('M')));
        assert!(matches!(app.modal, Some(Modal::MultiAgent(_))));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::CloneInstance {
                count: 2,
                start: true,
                ..
            }]
        ));
    }
}
