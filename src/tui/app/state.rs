//! Dashboard and VM state: loading project lists, filters, selection and the
//! periodic poll.

use std::collections::HashMap;
use std::time::Instant;

use crate::config::Config;
use crate::project::{self, GitSummary, Project, ProjectEntry, ProjectMeta, ProjectRegistry};
use crate::security::SecurityEngine;
use crate::session::snapshot::SnapshotStore;
use crate::session::state::StateStore;
use crate::session::terminal::TerminalBuffer;
use crate::vm::VmPaths;
use crate::vm::registry::VmSummary;

use super::util::clamp_index;
use super::*;

impl App {
    pub fn new(config: Config) -> Self {
        let store = StateStore::load(config.state_dir.join("state.json"));
        let snapshots = SnapshotStore::new(&config.snapshots_dir)
            .list()
            .unwrap_or_default();
        let instances = crate::vm::registry::VmRegistry::new(config.clone())
            .summaries()
            .unwrap_or_default();
        let project_entries = ProjectRegistry::load(&config.state_dir)
            .map(|registry| registry.entries().to_vec())
            .unwrap_or_default();
        let vm = VmStatus::from_config(&config);
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
            vm_open: None,
            project_view: ProjectView::Compact,
            terminal_focus: false,
            section_hits: Vec::new(),
            vm_section_hits: Vec::new(),
            terminal_hit: None,
            terminal_strip_hit: None,
            modal: None,
            vm,
            attached_vm: None,
            health: None,
            snapshots,
            selected_snapshot: 0,
            project_snapshots: Vec::new(),
            project_snapshot_selected: 0,
            vm_snapshots: Vec::new(),
            vm_snapshot_selected: 0,
            snapshot_rows: Vec::new(),
            project_entries,
            instances,
            instance_visible: Vec::new(),
            instance_selected: 0,
            instance_project: None,
            instance_rows: Vec::new(),
            vm_members: Vec::new(),
            vm_member_selected: 0,
            vm_member_rows: Vec::new(),
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
        self.refresh_project_entries();
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
        self.refresh_project_entries();
        self.refresh_instance_filter();
    }

    /// Reload the private project registry (assignment and membership).
    pub fn refresh_project_entries(&mut self) {
        self.project_entries = ProjectRegistry::load(&self.config.state_dir)
            .map(|registry| registry.entries().to_vec())
            .unwrap_or_default();
    }

    /// Registry entry for a project path, tolerating a symlinked projects root.
    pub fn project_entry(&self, path: &std::path::Path) -> Option<&ProjectEntry> {
        self.project_entries
            .iter()
            .find(|entry| entry.path == path)
            .or_else(|| {
                self.project_entries
                    .iter()
                    .find(|entry| crate::vm::same_project(&entry.path, path))
            })
    }

    /// Name of the VM a project is assigned to.
    pub fn project_vm_name(&self, path: &std::path::Path) -> Option<&str> {
        self.project_entry(path)
            .and_then(|entry| entry.vm.as_deref())
    }

    /// Summary of the VM a project is assigned to.
    pub fn project_vm(&self, path: &std::path::Path) -> Option<&VmSummary> {
        let name = self.project_vm_name(path)?;
        self.instances.iter().find(|summary| summary.name == name)
    }

    /// VM of the open project, the open VM screen, or the selected project.
    pub fn active_vm_name(&self) -> Option<String> {
        if self.screen == Screen::Vm {
            return self.vm_open.clone();
        }
        let project = self
            .active_project()
            .map(|project| project.path.clone())
            .or_else(|| self.open_project.clone())?;
        self.project_vm_name(&project).map(str::to_string)
    }

    pub fn active_vm_summary(&self) -> Option<&VmSummary> {
        self.active_vm_name()
            .and_then(|name| self.instances.iter().find(|summary| summary.name == name))
            .or_else(|| self.selected_instance())
            .or_else(|| self.instances.first())
    }

    /// VM badge shown next to a dashboard project row.
    pub fn project_vm_badge(&self, path: &std::path::Path) -> String {
        match self.project_vm_name(path) {
            Some(name) => {
                let members = self
                    .instances
                    .iter()
                    .find(|summary| summary.name == name)
                    .map(|summary| summary.members)
                    .unwrap_or(0);
                if members > 1 {
                    format!("⬢ {name} · {members}p")
                } else {
                    format!("⬢ {name}")
                }
            }
            None => "unassigned".to_string(),
        }
    }

    /// Guest paths of every exported member of a VM.
    ///
    /// Uses the boot plan when the registry is valid; otherwise falls back to
    /// the deterministic `<guest_projects_base>/<slug>` mapping so the TUI can
    /// still show a path for a broken membership.
    pub fn vm_guest_paths(&self, vm: &str) -> HashMap<String, String> {
        let store = crate::vm::config::VmStore::new(&self.config);
        let Ok(registry) = ProjectRegistry::load(&self.config.state_dir) else {
            return HashMap::new();
        };
        if let Ok(config) = store.load(vm) {
            let paths = VmPaths::from_vm(&self.config, &config);
            if let Ok(plan) = crate::vm::manager::BootPlan::build(
                &self.config,
                &config,
                &store,
                &registry,
                &paths,
            ) {
                return plan
                    .members
                    .into_iter()
                    .map(|member| (member.name, member.guest_path))
                    .collect();
            }
        }
        registry
            .exported_members(vm)
            .into_iter()
            .map(|entry| {
                (
                    entry.name.clone(),
                    format!(
                        "{}/{}",
                        self.config.guest_projects_base,
                        crate::vm::manager::slugify(&entry.name)
                    ),
                )
            })
            .collect()
    }

    /// Guest path of one project, when it is an exported member.
    pub fn project_guest_path(&self, path: &std::path::Path) -> Option<String> {
        let entry = self.project_entry(path)?;
        let vm = entry.vm.as_deref()?;
        self.vm_guest_paths(vm).remove(&entry.name)
    }

    /// Newest snapshot owned by `vm`.
    pub fn newest_snapshot_for_vm(
        &self,
        vm: &str,
    ) -> Option<&crate::session::snapshot::SnapshotMeta> {
        self.snapshots
            .iter()
            .filter(|snapshot| snapshot.vm() == Some(vm))
            .max_by_key(|snapshot| snapshot.created_at)
    }

    /// Open the reusable VM screen.
    pub fn open_vm_screen(&mut self, name: String) -> Vec<Effect> {
        if !self.instances.iter().any(|summary| summary.name == name) {
            self.toast(format!("unknown VM {name}"), ToastLevel::Warn);
            return Vec::new();
        }
        self.vm_open = Some(name);
        self.screen = Screen::Vm;
        self.focus = Focus::Projects;
        self.terminal_focus = false;
        self.refresh_vm_members();
        self.refresh_vm_snapshots();
        Vec::new()
    }

    /// Load the member list of [`App::vm_open`].
    pub fn refresh_vm_members(&mut self) {
        let Some(vm) = self.vm_open.clone() else {
            self.vm_members.clear();
            self.vm_member_selected = 0;
            return;
        };
        self.vm_members = ProjectRegistry::load(&self.config.state_dir)
            .map(|registry| {
                registry
                    .members(&vm)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if self.vm_member_selected >= self.vm_members.len() {
            self.vm_member_selected = self.vm_members.len().saturating_sub(1);
        }
    }

    /// Rebuild the VM-scoped snapshot list.
    pub fn refresh_vm_snapshots(&mut self) {
        let Some(vm) = self.vm_open.clone() else {
            self.vm_snapshots.clear();
            self.vm_snapshot_selected = 0;
            return;
        };
        self.vm_snapshots = self
            .snapshots
            .iter()
            .enumerate()
            .filter(|(_, snapshot)| snapshot.vm() == Some(vm.as_str()))
            .map(|(index, _)| index)
            .collect();
        if self.vm_snapshot_selected >= self.vm_snapshots.len() {
            self.vm_snapshot_selected = self.vm_snapshots.len().saturating_sub(1);
        }
    }

    /// Read the last `hegn-health` line from the active VM's log.
    ///
    /// The guest writes it at boot; the host never queries the guest from the
    /// renderer. A missing or stale line is shown as `not checked`.
    pub fn refresh_health(&mut self) {
        let serial = self
            .active_vm_summary()
            .and_then(|summary| summary.serial_log.clone())
            .or_else(|| {
                self.selected_instance()
                    .and_then(|summary| summary.serial_log.clone())
            });
        let Some(path) = serial else {
            self.health = None;
            return;
        };
        let bytes = crate::session::logs::tail_bytes(&path, 200).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        self.health = VmHealth::parse(&text);
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
                    .project_entries
                    .iter()
                    .find(|entry| crate::vm::same_project(&entry.path, path))
                    .map(|entry| entry.path.clone())
                    .or_else(|| {
                        self.instances
                            .iter()
                            .find(|instance| {
                                crate::vm::same_project(&instance.workspace, path)
                                    || crate::vm::same_project(&instance.project, path)
                            })
                            .map(|instance| instance.project.clone())
                    })?;
                self.projects
                    .iter()
                    .find(|project| project.path == project_path)
            }
            None => self.selected_project(),
        }
    }

    pub fn selected_instance(&self) -> Option<&VmSummary> {
        self.instance_visible
            .get(self.instance_selected)
            .and_then(|&index| self.instances.get(index))
    }

    /// Rebuild the VM list so it only contains the VM of the project selected
    /// in the project list.
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

        let vm = project
            .as_deref()
            .and_then(|project| self.project_vm_name(project))
            .map(str::to_string);
        self.instance_visible = match vm {
            Some(vm) => self
                .instances
                .iter()
                .enumerate()
                .filter(|(_, summary)| summary.name == vm)
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
        self.instance_selected = 0;
    }

    /// Refresh the VM list from disk/process state. Cheap enough for the UI
    /// tick; it only reads files and `/proc`.
    pub fn refresh_vms(&mut self) {
        let selected = self
            .selected_instance()
            .map(|instance| instance.name.clone());
        let registry = crate::vm::registry::VmRegistry::new(self.config.clone());
        if let Ok(instances) = registry.summaries() {
            self.instances = instances;
        }
        self.refresh_project_entries();
        self.refresh_instance_filter_named(selected);
        self.refresh_vm_members();
        self.refresh_project_snapshots();
        self.refresh_vm_snapshots();
        let active = self.active_vm_summary().cloned();
        if let Some(summary) = active {
            self.vm.vcpus = summary.vcpus;
            self.vm.memory_mib = summary.memory_mib;
            // Do not overwrite the optimistic Starting state while the attach
            // thread is still booting; it would re-enable double starts.
            if self.vm.state != VmUiState::Starting {
                self.vm.state = match summary.state.as_str() {
                    "running" => VmUiState::Running,
                    "paused" => VmUiState::Paused,
                    _ => VmUiState::Stopped,
                };
            }
        } else if self.session.is_none() && self.vm.state != VmUiState::Starting {
            self.vm.state = VmUiState::Stopped;
            self.vm.started_at = None;
            self.vm.note = None;
            self.vm.socket = None;
        }
        self.refresh_health();
    }

    pub fn focus_next(&mut self) {
        if self.screen == Screen::Dashboard {
            self.focus = Focus::Projects;
            return;
        }
        if self.screen == Screen::Vm {
            self.focus = match self.focus {
                Focus::Projects => Focus::Snapshots,
                Focus::Snapshots => Focus::Tools,
                Focus::Tools => Focus::Health,
                Focus::Health => Focus::Projects,
                Focus::Vm | Focus::Git => Focus::Projects,
            };
            return;
        }
        self.focus = match self.focus {
            Focus::Projects | Focus::Vm => Focus::Snapshots,
            Focus::Snapshots => Focus::Health,
            Focus::Health => Focus::Git,
            Focus::Git | Focus::Tools => Focus::Vm,
        };
        if self.project_view == ProjectView::Section {
            self.project_section();
        }
    }

    pub fn select_next(&mut self, delta: isize) {
        match self.focus {
            Focus::Projects => {
                if self.screen == Screen::Vm {
                    if self.vm_members.is_empty() {
                        return;
                    }
                    let max = self.vm_members.len() - 1;
                    self.vm_member_selected = clamp_index(self.vm_member_selected, delta, max);
                    return;
                }
                if self.visible.is_empty() {
                    return;
                }
                let max = self.visible.len() - 1;
                self.selected = clamp_index(self.selected, delta, max);
                self.refresh_instance_filter();
            }
            Focus::Vm | Focus::Tools => {}
            Focus::Snapshots => {
                let (selected, len, screen) = if self.screen == Screen::Vm {
                    (
                        self.vm_snapshot_selected,
                        self.vm_snapshots.len(),
                        Screen::Vm,
                    )
                } else if self.screen == Screen::Project {
                    (
                        self.project_snapshot_selected,
                        self.project_snapshots.len(),
                        Screen::Project,
                    )
                } else {
                    (
                        self.selected_snapshot,
                        self.snapshots.len(),
                        Screen::Dashboard,
                    )
                };
                if len == 0 {
                    return;
                }
                let next = clamp_index(selected, delta, len - 1);
                match screen {
                    Screen::Vm => self.vm_snapshot_selected = next,
                    Screen::Project => self.project_snapshot_selected = next,
                    Screen::Dashboard => self.selected_snapshot = next,
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
            self.refresh_vms();
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
            self.vm.state = VmUiState::Stopped;
            self.vm.started_at = None;
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

    /// Terminal window title. The project screen shows `project · vm` once a
    /// VM is assigned, matching the header.
    pub fn window_title(&self) -> String {
        match self.screen {
            Screen::Vm => match &self.vm_open {
                Some(vm) => format!("hegn — {vm}"),
                None => "hegn".to_string(),
            },
            Screen::Project => {
                let project = self.active_project();
                match (project, self.active_vm_name()) {
                    (Some(project), Some(vm)) => format!("hegn — {} · {vm}", project.name),
                    (Some(project), None) => format!("hegn — {}", project.name),
                    _ => "hegn".to_string(),
                }
            }
            Screen::Dashboard => "hegn".to_string(),
        }
    }

    pub fn project_section(&self) -> ProjectSection {
        ProjectSection::from_focus(self.focus)
    }

    pub fn vm_section(&self) -> VmSection {
        VmSection::from_focus(self.focus)
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
    fn instance_list_is_scoped_to_the_selected_projects_vm() {
        let (_dir, mut app) = test_app();
        let demo = app.config.projects_root.join("demo");
        let other = app.config.projects_root.join("other");
        std::fs::create_dir_all(&demo).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let mut base = fake_instance(&app, "demo", demo.clone());
        base.project = demo.clone();
        let mut other_instance = fake_instance(&app, "other", other.clone());
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
        app.instances = vec![base, other_instance];

        app.selected = app
            .visible
            .iter()
            .position(|&index| app.projects[index].path == demo)
            .unwrap();
        assign_fake_project(&mut app, &demo, "demo");
        app.refresh_instance_filter();
        assert_eq!(app.instance_visible.len(), 1);
        assert_eq!(app.selected_instance().unwrap().name, "demo");

        app.selected = app
            .visible
            .iter()
            .position(|&index| app.projects[index].path == other)
            .unwrap();
        assign_fake_project(&mut app, &other, "other");
        app.refresh_instance_filter();
        assert_eq!(app.instance_visible.len(), 1);
        assert_eq!(app.selected_instance().unwrap().name, "other");
    }

    #[test]
    fn filtering_clamps_the_selection_to_the_visible_rows() {
        let (_dir, mut app) = test_app();
        let projects = ["alpha", "beta", "gamma"]
            .into_iter()
            .map(|name| {
                let mut project = fake_project(name);
                project.path = app.config.projects_root.join(name);
                project
            })
            .collect();
        app.set_projects(projects);
        app.selected = 2;

        app.query = "alpha".into();
        app.refresh_filter();
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.selected, 0);
        assert_eq!(app.selected_project().unwrap().name, "alpha");

        app.query.clear();
        app.refresh_filter();
        assert_eq!(app.visible.len(), 3);
        app.select_next(10);
        assert_eq!(app.selected, 2, "selection never exceeds the list");
        app.select_next(-10);
        assert_eq!(app.selected, 0, "selection never goes below zero");
    }

    #[test]
    fn refreshing_vms_keeps_the_selection_by_name() {
        let (_dir, mut app) = test_app();
        let demo = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&demo).unwrap();
        let mut base = fake_instance(&app, "demo", demo.clone());
        base.project = demo.clone();

        app.set_projects(vec![Project {
            name: "demo".into(),
            path: demo.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }]);
        app.instances = vec![base];
        assign_fake_project(&mut app, &demo, "demo");
        app.refresh_instance_filter();
        assert_eq!(app.selected_instance().unwrap().name, "demo");

        app.instances.reverse();
        app.refresh_instance_filter_named(Some("demo".into()));
        assert_eq!(app.selected_instance().unwrap().name, "demo");
    }

    #[test]
    fn readding_a_hidden_project_makes_it_visible_again() {
        let (_dir, mut app) = test_app();
        let mut project = fake_project("hidden");
        project.path = app.config.projects_root.join("hidden");
        app.store.set_project_hidden(&project.path, true).unwrap();
        app.set_projects(vec![project.clone()]);
        assert!(app.visible.is_empty());
        assert!(app.store.is_hidden(&project.path));

        app.add_project(project.clone());

        assert!(!app.store.is_hidden(&project.path));
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.selected_project().unwrap().name, "hidden");
    }

    fn pending_launch(since: Instant, provision_seen: bool, ready: bool) -> PendingLaunch {
        PendingLaunch {
            spec: crate::agent::launch::LaunchSpec {
                name: "opencode".into(),
                command: "opencode".into(),
                integration: crate::agent::launch::Integration::External,
                bar: true,
            },
            vm: "demo".into(),
            project: PathBuf::from("/projects/demo"),
            since,
            provision_seen,
            ready,
        }
    }

    #[test]
    fn due_launch_waits_for_the_ready_marker_or_the_grace_period() {
        let (_dir, mut app) = test_app();
        // Without cloud-init, a prompt should appear within seconds.
        app.config.cloud_init = false;
        app.pending_launch = Some(pending_launch(
            Instant::now() - std::time::Duration::from_secs(10),
            false,
            false,
        ));
        let (spec, vm, _project) = app.take_due_launch().expect("grace elapsed");
        assert_eq!(spec.command, "opencode");
        assert_eq!(vm, "demo");
        assert!(app.pending_launch.is_none(), "a release is consumed once");

        // A cloud image must not race cloud-init/provisioning.
        app.config.cloud_init = true;
        app.pending_launch = Some(pending_launch(
            Instant::now() - std::time::Duration::from_secs(10),
            false,
            false,
        ));
        assert!(app.take_due_launch().is_none());
        assert!(app.pending_launch.is_some());

        // An explicit ready marker releases immediately.
        if let Some(pending) = app.pending_launch.as_mut() {
            pending.ready = true;
        }
        assert!(app.take_due_launch().is_some());
    }

    #[test]
    fn provisioning_in_progress_never_releases_on_a_timeout() {
        let (_dir, mut app) = test_app();
        app.config.cloud_init = false;
        app.pending_launch = Some(pending_launch(
            Instant::now() - std::time::Duration::from_secs(3600),
            true,
            false,
        ));
        assert!(
            app.take_due_launch().is_none(),
            "the installer must finish before an agent starts"
        );
        assert!(app.pending_launch.is_some());
    }

    #[test]
    fn poll_reports_a_closed_session_once() {
        let (_dir, mut app) = test_app();
        let log = ConsoleLog::default();
        app.attach_console(log.console("/projects/demo"));
        log.push_output(b"hello\nworld\n");
        log.set_exited(true);

        app.poll();
        assert_eq!(app.vm.state, VmUiState::Stopped);
        assert!(app.terminal.len() >= 2);
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("session exited"))
        );

        app.toast = None;
        app.poll();
        assert!(
            app.toast.is_none(),
            "a closed session is reported only once"
        );
    }

    #[test]
    fn refresh_does_not_clear_the_starting_state() {
        let (_dir, mut app) = test_app();
        app.vm.state = VmUiState::Starting;
        app.refresh_vms();
        assert_eq!(app.vm.state, VmUiState::Starting);
    }

    #[test]
    fn vm_status_reflects_config() {
        let (_dir, app) = test_app();
        assert_eq!(app.vm.state, VmUiState::Stopped);
        assert_eq!(app.vm.backend, "cloud-hypervisor");
        assert_eq!(app.vm.vcpus, app.config.vcpus);
    }

    #[test]
    fn window_title_shows_project_and_vm_once_assigned() {
        let (_dir, mut app) = test_app();
        assert_eq!(app.window_title(), "hegn");
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
        app.open_project = Some(project);
        assert_eq!(app.window_title(), "hegn — demo · rust");
    }

    #[test]
    fn vm_badges_and_preview_fields_resolve_registry_assignments() {
        let (_dir, mut app) = test_app();
        let demo = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&demo).unwrap();
        let mut summary = fake_instance(&app, "rust", demo.clone());
        summary.members = 2;
        summary.harness = crate::config::Harness::Opencode;
        summary.state = "running".into();
        app.instances = vec![summary];
        assign_fake_project(&mut app, &demo, "rust");

        assert_eq!(app.project_vm_name(&demo), Some("rust"));
        assert_eq!(app.project_vm_badge(&demo), "⬢ rust · 2p");
        assert_eq!(
            app.project_vm_badge(&app.config.projects_root.join("other")),
            "unassigned"
        );
        assert!(app.project_guest_path(&demo).is_some());
    }
}
