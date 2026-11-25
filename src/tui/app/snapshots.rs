//! Snapshot list state for the dashboard, project and VM screens, including
//! the destructive delete/restore confirmations.

use crate::session::snapshot::{SnapshotMeta, SnapshotStore};

use super::*;

impl App {
    pub fn refresh_snapshots(&mut self) {
        self.snapshots = SnapshotStore::new(&self.config.snapshots_dir)
            .list()
            .unwrap_or_default();
        if self.selected_snapshot >= self.snapshots.len() {
            self.selected_snapshot = self.snapshots.len().saturating_sub(1);
        }
        self.refresh_project_snapshots();
        self.refresh_vm_snapshots();
    }

    /// Rebuild the project-scoped snapshot list. A project screen is VM-scoped:
    /// every snapshot of the project's VM is relevant. Legacy snapshots without
    /// a recorded owner stay out.
    pub fn refresh_project_snapshots(&mut self) {
        let Some(project) = self
            .active_project()
            .map(|project| project.path.clone())
            .or_else(|| self.open_project.clone())
        else {
            self.project_snapshots.clear();
            return;
        };
        let vm = self.project_vm_name(&project).map(str::to_string);
        self.project_snapshots = match vm {
            Some(vm) => self
                .snapshots
                .iter()
                .enumerate()
                .filter(|(_, snapshot)| {
                    snapshot.vm() == Some(vm.as_str())
                        || snapshot.project.as_deref() == Some(project.as_path())
                })
                .map(|(index, _)| index)
                .collect(),
            None => self
                .snapshots
                .iter()
                .enumerate()
                .filter(|(_, snapshot)| snapshot.project.as_deref() == Some(project.as_path()))
                .map(|(index, _)| index)
                .collect(),
        };
        if self.project_snapshot_selected >= self.project_snapshots.len() {
            self.project_snapshot_selected = self.project_snapshots.len().saturating_sub(1);
        }
    }

    pub fn selected_snapshot(&self) -> Option<&SnapshotMeta> {
        match self.screen {
            Screen::Project => self
                .project_snapshots
                .get(self.project_snapshot_selected)
                .and_then(|&index| self.snapshots.get(index)),
            Screen::Vm => self
                .vm_snapshots
                .get(self.vm_snapshot_selected)
                .and_then(|&index| self.snapshots.get(index)),
            Screen::Dashboard => self.snapshots.get(self.selected_snapshot),
        }
    }

    pub(super) fn confirm_delete_snapshot(&mut self) -> Vec<Effect> {
        if let Some(snapshot) = self.selected_snapshot().cloned() {
            self.modal = Some(Modal::Confirm(ConfirmPrompt {
                title: "Delete snapshot".into(),
                message: format!(
                    "Delete '{}' ({})?\nThis cannot be undone.",
                    snapshot.name,
                    snapshot.size_label()
                ),
                confirm_label: "Delete".into(),
                action: ConfirmAction::DeleteSnapshot(snapshot.name),
                scroll: 0,
            }));
        } else {
            self.toast("no snapshot selected", ToastLevel::Warn);
        }
        Vec::new()
    }

    pub(super) fn confirm_restore_snapshot(&mut self) -> Vec<Effect> {
        if let Some(snapshot) = self.selected_snapshot().cloned() {
            self.modal = Some(Modal::Confirm(ConfirmPrompt {
                title: "Restore snapshot".into(),
                message: format!(
                    "Load VM state '{}'?\nThe current VM session will be replaced.",
                    snapshot.name
                ),
                confirm_label: "Restore".into(),
                action: ConfirmAction::RestoreSnapshot(Box::new(snapshot)),
                scroll: 0,
            }));
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn delete_snapshot_requires_confirmation() {
        let (_dir, mut app) = test_app();
        let paths = SnapshotStore::new(&app.config.snapshots_dir)
            .prepare("old")
            .unwrap();
        SnapshotStore::new(&app.config.snapshots_dir)
            .save_meta(&SnapshotMeta {
                name: "old".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: paths.dir,
                instance: None,
                project: None,
                size_bytes: 0,
                note: None,
                ..SnapshotMeta::default()
            })
            .unwrap();
        app.refresh_snapshots();
        app.focus = Focus::Snapshots;
        app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(app.snapshots.is_empty());
        assert!(!app.config.snapshots_dir.join("old").exists());
    }

    fn save_snapshot(app: &App, name: &str, project: Option<PathBuf>, vm: Option<&str>) {
        let paths = SnapshotStore::new(&app.config.snapshots_dir)
            .prepare(name)
            .unwrap();
        SnapshotStore::new(&app.config.snapshots_dir)
            .save_meta(&SnapshotMeta {
                name: name.into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: paths.dir,
                instance: vm.map(str::to_string),
                project,
                size_bytes: 0,
                note: None,
                ..SnapshotMeta::default()
            })
            .unwrap();
    }

    fn confirm_prompt(app: &App) -> &ConfirmPrompt {
        match &app.modal {
            Some(Modal::Confirm(prompt)) => prompt,
            other => panic!("expected a confirmation, got {other:?}"),
        }
    }

    #[test]
    fn enter_never_deletes_a_snapshot() {
        let (_dir, mut app) = test_app();
        save_snapshot(&app, "old", None, None);
        app.refresh_snapshots();
        app.focus = Focus::Snapshots;

        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(confirm_prompt(&app).title, "Delete snapshot");
        let effects = app.handle_key(key(KeyCode::Enter));

        assert!(effects.is_empty());
        assert_eq!(confirm_prompt(&app).title, "Delete snapshot");
        assert!(app.config.snapshots_dir.join("old").exists());

        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert!(app.config.snapshots_dir.join("old").exists());
    }

    #[test]
    fn restoring_a_project_snapshot_needs_explicit_consent() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.projects = vec![Project {
            name: "demo".into(),
            path: project.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.instances = vec![fake_instance(&app, "rust", project.clone())];
        assign_fake_project(&mut app, &project, "rust");
        save_snapshot(&app, "before-upgrade", Some(project.clone()), Some("rust"));
        app.open_project = Some(project);
        app.screen = Screen::Project;
        app.refresh_snapshots();
        app.focus = Focus::Snapshots;

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(confirm_prompt(&app).title, "Restore snapshot");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(confirm_prompt(&app).title, "Restore snapshot");

        let effects = app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::RestoreSnapshot(snapshot)] if snapshot.name == "before-upgrade"
        ));
        assert!(app.modal.is_none());
    }

    #[test]
    fn project_snapshots_are_scoped_to_the_projects_vm() {
        let (_dir, mut app) = test_app();
        let demo = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&demo).unwrap();
        app.projects = vec![Project {
            name: "demo".into(),
            path: demo.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.instances = vec![fake_instance(&app, "rust", demo.clone())];
        assign_fake_project(&mut app, &demo, "rust");
        app.open_project = Some(demo.clone());
        app.screen = Screen::Project;
        save_snapshot(&app, "mine", None, Some("rust"));
        save_snapshot(&app, "other", None, Some("other"));
        app.refresh_snapshots();

        assert_eq!(app.project_snapshots.len(), 1);
        assert_eq!(app.selected_snapshot().unwrap().name, "mine");
    }

    #[test]
    fn vm_screen_snapshots_are_scoped_to_the_open_vm() {
        let (_dir, mut app) = test_app();
        app.instances = vec![
            fake_instance(&app, "rust", app.config.projects_root.join("demo")),
            fake_instance(&app, "go", app.config.projects_root.join("other")),
        ];
        save_snapshot(&app, "rust-one", None, Some("rust"));
        save_snapshot(&app, "go-one", None, Some("go"));
        app.refresh_snapshots();
        app.open_vm_screen("rust".into());

        assert_eq!(app.screen, Screen::Vm);
        assert_eq!(app.vm_snapshots.len(), 1);
        assert_eq!(app.selected_snapshot().unwrap().name, "rust-one");
    }
}
