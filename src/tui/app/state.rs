//! Dashboard and instance state: loading project lists, filters, selection and
//! the periodic poll.

use std::time::Instant;

use crate::config::Config;
use crate::project::{self, GitSummary, Project, ProjectMeta};
use crate::security::SecurityEngine;
use crate::session::snapshot::SnapshotStore;
use crate::session::state::StateStore;
use crate::session::terminal::TerminalBuffer;
use crate::vm::instance::InstanceSummary;

use super::util::clamp_index;
use super::*;

impl App {
    pub fn new(config: Config) -> Self {
        let store = StateStore::load(config.state_dir.join("state.json"));
        let snapshots = SnapshotStore::new(&config.snapshots_dir)
            .list()
            .unwrap_or_default();
        let instances = crate::vm::instance::InstanceRegistry::new(config.clone())
            .summaries()
            .unwrap_or_default();
        let instance = InstanceStatus::from_config(&config);
        let security_engine = SecurityEngine::new(config.security);
        Self {
            config,
            store,
            projects: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            query: String::new(),
            searching: false,
            focus: Focus::Projects,
            screen: Screen::Dashboard,
            open_project: None,
            project_view: ProjectView::Compact,
            terminal_focus: false,
            section_hits: Vec::new(),
            terminal_hit: None,
            terminal_strip_hit: None,
            modal: None,
            instance,
            health: None,
            snapshots,
            selected_snapshot: 0,
            project_snapshots: Vec::new(),
            project_snapshot_selected: 0,
            snapshot_rows: Vec::new(),
            instances,
            instance_visible: Vec::new(),
            instance_selected: 0,
            instance_project: None,
            instance_rows: Vec::new(),
            project_rows: Vec::new(),
            project_meta: ProjectMeta::default(),
            session: None,
            terminal: TerminalBuffer::default(),
            input: String::new(),
            terminal_mode: TerminalMode::Normal,
            scroll: 0,
            toast: None,
            download: None,
            provision: None,
            pending_launch: None,
            provision_running: false,
            provision_ready_seen: false,
            footer_hits: Vec::new(),
            config_rows: Vec::new(),
            config_steppers: Vec::new(),
            config_tabs: Vec::new(),
            config_hover: None,
            hovered: None,
            modal_scroll: None,
            scroll_hover: false,
            scroll_dragging: false,
            should_quit: false,
            loading: true,
            session_closed_reported: false,
            viewport: (120, 40),
            tick: 0,
            security_engine,
            last_click: None,
        }
    }

    /// Install a fresh project list (usually from the background scanner).
    pub fn set_projects(&mut self, mut projects: Vec<Project>) {
        projects.retain(|project| !self.store.is_hidden(&project.path));
        let opened = self.store.state().opened_at();
        for project in &mut projects {
            project.last_opened = opened.get(&project.path).copied();
        }
        let previous = self.selected_project().map(|p| p.path.clone());
        self.projects = projects;
        self.loading = false;
        self.refresh_filter();
        let preferred = previous.or_else(|| self.store.state().last_project.clone());
        if let Some(path) = preferred
            && let Some(index) = self
                .visible
                .iter()
                .position(|&i| self.projects[i].path == path)
        {
            self.selected = index;
        }
        self.refresh_instance_filter();
    }

    pub fn add_project(&mut self, project: Project) {
        let path = project.path.clone();
        // Creating or importing a project that was previously removed with
        // "keep files" makes it visible again.
        if let Err(err) = self.store.set_project_hidden(&path, false) {
            self.toast(
                format!("could not update the project list: {err}"),
                ToastLevel::Warn,
            );
        }
        self.projects.retain(|p| p.path != project.path);
        self.projects.push(project);
        self.projects
            .sort_by_key(|project| project.name.to_lowercase());
        self.loading = false;
        self.query.clear();
        self.refresh_filter();
        if let Some(index) = self
            .visible
            .iter()
            .position(|&i| self.projects[i].path == path)
        {
            self.selected = index;
        }
        self.refresh_instance_filter();
    }

    /// Read the last `hegn-health` line from the selected instance's log.
    ///
    /// The guest writes it at boot; the host never queries the guest from the
    /// renderer. A missing or stale line is shown as `not checked`.
    pub fn refresh_health(&mut self) {
        let Some(instance) = self.selected_instance() else {
            self.health = None;
            return;
        };
        let Some(path) = instance.serial_log.as_deref() else {
            self.health = None;
            return;
        };
        let bytes = crate::session::logs::tail_bytes(path, 200).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        self.health = InstanceHealth::parse(&text);
    }

    pub fn refresh_filter(&mut self) {
        self.visible = project::filter_projects(&self.projects, &self.query);
        if self.visible.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.visible.len() {
            self.selected = self.visible.len() - 1;
        }
        self.refresh_instance_filter();
    }

    pub fn selected_project(&self) -> Option<&Project> {
        self.visible
            .get(self.selected)
            .and_then(|&i| self.projects.get(i))
    }

    /// Project shown on the project screen, frozen when it was entered.
    pub fn active_project(&self) -> Option<&Project> {
        match &self.open_project {
            Some(path) => {
                if let Some(project) = self.projects.iter().find(|project| &project.path == path) {
                    return Some(project);
                }
                // Clone consoles record their private workspace as the session
                // project. Map it back to the original project for the chrome.
                let project_path = self
                    .instances
                    .iter()
                    .find(|instance| {
                        crate::vm::instance::same_project(&instance.workspace, path)
                            || crate::vm::instance::same_project(&instance.project, path)
                    })
                    .map(|instance| instance.project.clone())?;
                self.projects
                    .iter()
                    .find(|project| project.path == project_path)
            }
            None => self.selected_project(),
        }
    }

    pub fn selected_instance(&self) -> Option<&InstanceSummary> {
        self.instance_visible
            .get(self.instance_selected)
            .and_then(|&index| self.instances.get(index))
    }

    /// Rebuild the instance list so it only contains instances for the project
    /// selected in the project list. Multi-agent clones share their source
    /// project, so they stay visible and are one `Enter` away.
    ///
    /// The previous selection is preserved while the project stays the same;
    /// switching projects selects a running instance when one exists.
    pub fn refresh_instance_filter(&mut self) {
        let selected = self
            .selected_instance()
            .map(|instance| instance.name.clone());
        self.refresh_instance_filter_named(selected);
    }

    /// Rebuild the filter while preserving `selected` by name. The name must be
    /// captured before [`App::instances`] is replaced: the visible indices point
    /// into the old vector.
    pub(super) fn refresh_instance_filter_named(&mut self, selected: Option<String>) {
        let project = self
            .active_project()
            .map(|project| project.path.clone())
            .or_else(|| self.open_project.clone());
        let project_changed = self.instance_project != project;
        self.instance_project = project.clone();

        self.instance_visible = match project {
            Some(project) => self
                .instances
                .iter()
                .enumerate()
                .filter(|(_, instance)| {
                    crate::vm::instance::same_project(&instance.project, &project)
                })
                .map(|(index, _)| index)
                .collect(),
            None => Vec::new(),
        };

        if !project_changed
            && let Some(name) = selected
            && let Some(index) = self
                .instance_visible
                .iter()
                .position(|&index| self.instances[index].name == name)
        {
            self.instance_selected = index;
            return;
        }
        if project_changed {
            self.instance_selected = self
                .instance_visible
                .iter()
                .position(|&index| self.instances[index].state == "running")
                .unwrap_or(0);
        } else {
            self.instance_selected = self
                .instance_selected
                .min(self.instance_visible.len().saturating_sub(1));
        }
    }

    /// Refresh the instance list from disk/process state. Cheap enough for the
    /// UI tick; it only reads files and `/proc`.
    pub fn refresh_instances(&mut self) {
        let selected = self
            .selected_instance()
            .map(|instance| instance.name.clone());
        let registry = crate::vm::instance::InstanceRegistry::new(self.config.clone());
        if let Ok(instances) = registry.summaries() {
            self.instances = instances;
        }
        self.refresh_instance_filter_named(selected);
        let selected = self.selected_instance().cloned();
        if let Some(summary) = selected {
            self.instance.vcpus = summary.vcpus;
            self.instance.memory_mib = summary.memory_mib;
            // Do not overwrite the optimistic Starting state while the attach
            // thread is still booting; it would re-enable double starts.
            if self.instance.state != InstanceState::Starting {
                self.instance.state = match summary.state.as_str() {
                    "running" => InstanceState::Running,
                    "paused" => InstanceState::Paused,
                    _ => InstanceState::Stopped,
                };
            }
        } else if self.session.is_none() && self.instance.state != InstanceState::Starting {
            self.instance.state = InstanceState::Stopped;
            self.instance.started_at = None;
            self.instance.note = None;
            self.instance.socket = None;
        }
        self.refresh_health();
    }

    pub fn focus_next(&mut self) {
        if self.screen == Screen::Dashboard {
            self.focus = Focus::Projects;
            return;
        }
        self.focus = match self.focus {
            Focus::Projects => Focus::Instances,
            Focus::Instances => Focus::Snapshots,
            Focus::Snapshots => Focus::Health,
            Focus::Health => Focus::Git,
            Focus::Git => Focus::Instances,
        };
        if self.project_view == ProjectView::Section {
            self.project_section();
        }
    }

    pub fn select_next(&mut self, delta: isize) {
        match self.focus {
            Focus::Projects => {
                if self.visible.is_empty() {
                    return;
                }
                let max = self.visible.len() - 1;
                self.selected = clamp_index(self.selected, delta, max);
                self.refresh_instance_filter();
            }
            Focus::Instances => {
                if self.instance_visible.is_empty() {
                    return;
                }
                let max = self.instance_visible.len() - 1;
                self.instance_selected = clamp_index(self.instance_selected, delta, max);
            }
            Focus::Snapshots => {
                let len = if self.screen == Screen::Project {
                    self.project_snapshots.len()
                } else {
                    self.snapshots.len()
                };
                if len == 0 {
                    return;
                }
                if self.screen == Screen::Project {
                    self.project_snapshot_selected =
                        clamp_index(self.project_snapshot_selected, delta, len - 1);
                } else {
                    self.selected_snapshot = clamp_index(self.selected_snapshot, delta, len - 1);
                }
            }
            Focus::Health | Focus::Git => {}
        }
    }

    pub fn toast(&mut self, text: impl Into<String>, level: ToastLevel) {
        self.toast = Some(Toast::new(text, level));
    }

    /// Record the latest image download progress shown in the notification.
    pub fn set_download_progress(&mut self, progress: crate::vm::images::DownloadProgress) {
        self.download = Some(progress);
    }

    /// Drop the download notification once the worker has reported back.
    pub fn clear_download_progress(&mut self) {
        self.download = None;
    }

    pub fn set_viewport(&mut self, cols: u16, rows: u16) {
        if self.viewport == (cols, rows) {
            return;
        }
        self.viewport = (cols, rows);
    }

    pub fn poll(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if self.tick.is_multiple_of(40) {
            self.refresh_instances();
        }
        let mut closed = false;
        if let Some(session) = &mut self.session {
            for bytes in session.drain() {
                self.terminal.push_bytes(&bytes);
            }
            if session.is_exited() {
                closed = true;
            }
        }
        if closed && !self.session_closed_reported {
            self.session_closed_reported = true;
            self.instance.state = InstanceState::Stopped;
            self.instance.started_at = None;
            self.toast("session exited", ToastLevel::Info);
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.expired(Instant::now()))
        {
            self.toast = None;
        }
    }

    pub fn tick(&self) -> u64 {
        self.tick
    }

    pub fn project_section(&self) -> ProjectSection {
        ProjectSection::from_focus(self.focus)
    }

    pub fn project_git(&self) -> Option<&GitSummary> {
        self.selected_project().map(|p| &p.git)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn starts_loading_with_no_projects() {
        let (_dir, app) = test_app();
        assert!(app.loading);
        assert!(app.visible.is_empty());
    }

    #[test]
    fn set_projects_applies_recency_and_filters() {
        let (_dir, mut app) = test_app();
        let mut a = fake_project("alpha");
        a.path = app.config.projects_root.join("alpha");
        let mut b = fake_project("hegn-tui");
        b.path = app.config.projects_root.join("hegn-tui");
        app.store.touch(&b.path).unwrap();
        app.set_projects(vec![a, b]);
        assert!(!app.loading);
        assert_eq!(app.visible.len(), 2);
        assert_eq!(app.selected_project().unwrap().name, "hegn-tui");

        app.query = "alpha".into();
        app.refresh_filter();
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.selected_project().unwrap().name, "alpha");
    }

    #[test]
    fn hidden_projects_stay_out_of_the_dashboard() {
        let (_dir, mut app) = test_app();
        let mut a = fake_project("alpha");
        a.path = app.config.projects_root.join("alpha");
        let mut b = fake_project("beta");
        b.path = app.config.projects_root.join("beta");
        app.store.set_project_hidden(&b.path, true).unwrap();
        app.set_projects(vec![a.clone(), b]);
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.selected_project().unwrap().name, "alpha");

        app.forget_project_locally(&a.path);
        assert!(app.projects.is_empty());
        assert!(app.visible.is_empty());
    }

    #[test]
    fn instance_list_is_scoped_to_the_selected_project_and_includes_clones() {
        let (_dir, mut app) = test_app();
        let demo = app.config.projects_root.join("demo");
        let other = app.config.projects_root.join("other");

        let mut base = fake_instance(&app, "demo", demo.join("workspace"));
        base.project = demo.clone();
        let mut clone = fake_instance(&app, "demo-1", demo.join("clone-1"));
        clone.project = demo.clone();
        clone.ephemeral = true;
        clone.source_instance = Some("demo".into());
        let mut other_instance = fake_instance(&app, "other", other.join("workspace"));
        other_instance.project = other.clone();

        let demo_project = Project {
            name: "demo".into(),
            path: demo.clone(),
            git: GitSummary::default(),
            last_opened: None,
        };
        let other_project = Project {
            name: "other".into(),
            path: other.clone(),
            ..demo_project.clone()
        };
        app.set_projects(vec![demo_project, other_project]);
        app.instances = vec![base, clone, other_instance];

        app.selected = app
            .visible
            .iter()
            .position(|&index| app.projects[index].path == demo)
            .unwrap();
        app.refresh_instance_filter();
        assert_eq!(app.instance_visible.len(), 2);
        let names: Vec<&str> = app
            .instance_visible
            .iter()
            .map(|&index| app.instances[index].name.as_str())
            .collect();
        assert_eq!(names, ["demo", "demo-1"]);
        assert!(
            app.instance_visible
                .iter()
                .any(|&index| app.instances[index].ephemeral),
            "multi-agent clones must be attachable from the list"
        );

        app.selected = app
            .visible
            .iter()
            .position(|&index| app.projects[index].path == other)
            .unwrap();
        app.refresh_instance_filter();
        assert_eq!(app.instance_visible.len(), 1);
        assert_eq!(app.selected_instance().unwrap().name, "other");
    }

    #[test]
    fn refresh_does_not_clear_the_starting_state() {
        let (_dir, mut app) = test_app();
        app.instance.state = InstanceState::Starting;
        app.refresh_instances();
        assert_eq!(app.instance.state, InstanceState::Starting);
    }

    #[test]
    fn instance_status_reflects_config() {
        let (_dir, app) = test_app();
        assert_eq!(app.instance.state, InstanceState::Stopped);
        assert_eq!(app.instance.backend, "cloud-hypervisor");
        assert_eq!(app.instance.vcpus, app.config.vcpus);
    }
}
