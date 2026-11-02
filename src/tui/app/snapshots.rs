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
