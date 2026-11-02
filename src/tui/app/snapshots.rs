//! Snapshot list state for the dashboard and project screens, including the
//! destructive delete/restore confirmations.

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
    }

    /// Rebuild the project-scoped snapshot list. Older snapshots without a
    /// recorded project are intentionally left out of the project screen.
    pub fn refresh_project_snapshots(&mut self) {
        let project = self
            .active_project()
            .map(|project| project.path.clone())
            .or_else(|| self.open_project.clone());
        let instance_names: Vec<&str> = project
            .as_ref()
            .map(|project| {
                self.instances
                    .iter()
                    .filter(|instance| {
                        crate::vm::instance::same_project(&instance.project, project)
                    })
                    .map(|instance| instance.name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        self.project_snapshots = match project {
            Some(project) => self
                .snapshots
                .iter()
                .enumerate()
                .filter(|(_, snapshot)| {
                    snapshot.project.as_deref() == Some(project.as_path())
                        || snapshot
                            .instance
                            .as_deref()
                            .is_some_and(|name| instance_names.contains(&name))
                })
                .map(|(index, _)| index)
                .collect(),
            None => Vec::new(),
        };
        if self.project_snapshot_selected >= self.project_snapshots.len() {
            self.project_snapshot_selected = self.project_snapshots.len().saturating_sub(1);
        }
    }

    pub fn selected_snapshot(&self) -> Option<&SnapshotMeta> {
        if self.screen == Screen::Project {
            self.project_snapshots
                .get(self.project_snapshot_selected)
                .and_then(|&index| self.snapshots.get(index))
        } else {
            self.snapshots.get(self.selected_snapshot)
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

    fn save_snapshot(app: &App, name: &str, project: Option<PathBuf>, instance: Option<&str>) {
        let paths = SnapshotStore::new(&app.config.snapshots_dir)
            .prepare(name)
            .unwrap();
        SnapshotStore::new(&app.config.snapshots_dir)
            .save_meta(&SnapshotMeta {
                name: name.into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: paths.dir,
                instance: instance.map(str::to_string),
                project,
                size_bytes: 0,
                note: None,
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
        save_snapshot(&app, "before-upgrade", Some(project.clone()), None);
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
    fn project_snapshots_include_snapshots_of_scoped_instances() {
        let (_dir, mut app) = test_app();
        let demo = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&demo).unwrap();
        app.open_project = Some(demo.clone());
        app.screen = Screen::Project;
        let mut clone = fake_instance(&app, "demo-1", demo.join("clone-1"));
        clone.project = demo.clone();
        app.instances = vec![clone];
        app.snapshots = vec![
            SnapshotMeta {
                name: "by-project".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: PathBuf::from("/snap/by-project"),
                instance: None,
                project: Some(demo.clone()),
                size_bytes: 0,
                note: None,
            },
            SnapshotMeta {
                name: "by-instance".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: PathBuf::from("/snap/by-instance"),
                instance: Some("demo-1".into()),
                project: None,
                size_bytes: 0,
                note: None,
            },
            SnapshotMeta {
                name: "unrelated".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: PathBuf::from("/snap/unrelated"),
                instance: Some("other".into()),
                project: Some(app.config.projects_root.join("other")),
                size_bytes: 0,
                note: None,
            },
        ];
        app.project_snapshot_selected = 9;
        app.refresh_project_snapshots();

        assert_eq!(app.project_snapshots, vec![0, 1]);
        assert_eq!(
            app.project_snapshot_selected, 1,
            "a zoomed-out selection clamps to the scoped list"
        );
        let names: Vec<&str> = app
            .project_snapshots
            .iter()
            .map(|&index| app.snapshots[index].name.as_str())
            .collect();
        assert_eq!(names, ["by-project", "by-instance"]);
        assert_eq!(app.selected_snapshot().unwrap().name, "by-instance");
    }

    #[test]
    fn project_snapshots_are_filtered_by_project() {
        let (_dir, mut app) = test_app();
        app.open_project = Some(PathBuf::from("/projects/demo"));
        app.snapshots = vec![
            SnapshotMeta {
                name: "one".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: PathBuf::from("/snap/one"),
                instance: None,
                project: Some(PathBuf::from("/projects/demo")),
                size_bytes: 0,
                note: None,
            },
            SnapshotMeta {
                name: "two".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: PathBuf::from("/snap/two"),
                instance: None,
                project: Some(PathBuf::from("/projects/other")),
                size_bytes: 0,
                note: None,
            },
        ];
        app.refresh_project_snapshots();
        assert_eq!(app.project_snapshots.len(), 1);
        assert_eq!(app.selected_snapshot().unwrap().name, "one");
    }
}
