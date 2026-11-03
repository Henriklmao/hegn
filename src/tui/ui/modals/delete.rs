//! Destructive delete prompts for instances and projects.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::tui::app::{
    ActionId, App, DeleteInstanceModal, DeleteInstanceStage, DeleteProjectModal,
};

use super::super::ModalHits;
use super::super::screens::footer::FooterDef;
use super::super::theme::{RED, YELLOW, centered};
use super::message::render_message;

pub(crate) fn render_delete_instance(
    frame: &mut Frame,
    app: &App,
    modal: &DeleteInstanceModal,
    area: Rect,
) -> ModalHits {
    let (body, color, buttons): (String, Color, &[FooterDef]) = match modal.stage {
        DeleteInstanceStage::Confirm => (
            format!(
                "Delete instance '{}'?\n\nProject: {}\nWorkspace: {}\n\nThe VM is stopped first.",
                modal.instance.name,
                modal.instance.project.display(),
                modal.instance.workspace.display()
            ),
            YELLOW,
            &[
                FooterDef {
                    key: "y",
                    label: "Continue",
                    action: ActionId::Confirm,
                },
                FooterDef {
                    key: "n",
                    label: "Cancel",
                    action: ActionId::Cancel,
                },
            ],
        ),
        DeleteInstanceStage::Files => (
            format!(
                "Also delete the cloned workspace files?\n\n{}\n\nChoose 'Keep files' to unregister the instance but keep the clone.",
                modal.instance.workspace.display()
            ),
            RED,
            &[
                FooterDef {
                    key: "y",
                    label: "Delete files",
                    action: ActionId::Confirm,
                },
                FooterDef {
                    key: "n",
                    label: "Keep files",
                    action: ActionId::KeepFiles,
                },
                FooterDef {
                    key: "Esc",
                    label: "Cancel",
                    action: ActionId::Cancel,
                },
            ],
        ),
    };
    let (footer, scroll) = render_message(
        frame,
        app,
        area,
        72,
        42,
        1,
        "delete instance",
        &body,
        color,
        modal.scroll,
        buttons,
    );
    if let Some(error) = &modal.error {
        let rect = centered(area, 72, 42);
        frame.render_widget(
            Paragraph::new(Span::styled(format!(" {error}"), Style::default().fg(RED))),
            Rect {
                x: rect.x + 2,
                y: rect.y + rect.height.saturating_sub(3),
                width: rect.width.saturating_sub(4),
                height: 1,
            },
        );
    }
    ModalHits {
        footer,
        scroll,
        ..ModalHits::default()
    }
}

pub(crate) fn render_delete_project(
    frame: &mut Frame,
    app: &App,
    modal: &DeleteProjectModal,
    area: Rect,
) -> ModalHits {
    let git = &modal.git;
    let git_line = if git.is_repo {
        format!(
            "{} ↑{} ↓{} · {} staged · {} modified · {} untracked",
            git.badge(),
            git.ahead,
            git.behind,
            git.staged,
            git.modified,
            git.untracked
        )
    } else if let Some(error) = &git.error {
        format!("not a git repository ({error})")
    } else {
        "not a git repository".to_string()
    };
    let mut body = format!(
        "Delete project '{}'?\n\nPath: {}\nGit:  {}\n",
        modal.project.name,
        modal.project.path.display(),
        git_line
    );
    if git.is_dirty() {
        body.push_str("\nWARNING: uncommitted changes will be lost if the files are deleted.\n");
    }
    if !modal.instances.is_empty() {
        body.push_str(&format!(
            "\nInstances to remove: {}\n",
            modal.instances.join(", ")
        ));
    }
    body.push_str(
        "\n[y] remove from the dashboard and keep every project file\n\
         [f] delete the project directory as well\n\n\
         Imported projects only lose their picker symlink; the original directory is never touched.",
    );
    let color = if git.is_dirty() { RED } else { YELLOW };
    let (footer, scroll) = render_message(
        frame,
        app,
        area,
        72,
        48,
        1,
        "delete project",
        &body,
        color,
        modal.scroll,
        &[
            FooterDef {
                key: "y",
                label: "Remove, keep files",
                action: ActionId::Confirm,
            },
            FooterDef {
                key: "f",
                label: "Delete files",
                action: ActionId::DeleteProjectFiles,
            },
            FooterDef {
                key: "Esc",
                label: "Cancel",
                action: ActionId::Cancel,
            },
        ],
    );
    ModalHits {
        footer,
        scroll,
        ..ModalHits::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;
    use crate::project::GitSummary;
    use crate::tui::app::Modal;
    use crate::tui::ui::test_support::*;
    use crate::vm::instance::InstanceSummary;
    use std::path::PathBuf;

    fn instance(app: &App, name: &str, state: &str) -> InstanceSummary {
        InstanceSummary {
            name: name.into(),
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from(format!("/workspaces/{name}")),
            harness: Harness::Opencode,
            ephemeral: false,
            source_instance: None,
            created_at: chrono::Utc::now(),
            state: state.into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.instances_dir.join(name).join("hegn.toml"),
            vcpus: 2,
            memory_mib: 2048,
            error: None,
        }
    }

    #[test]
    fn delete_project_modal_shows_git_status_and_file_choices() {
        let (_dir, mut app) = app();
        let project = app.selected_project().cloned().unwrap();
        app.modal = Some(Modal::DeleteProject(crate::tui::app::DeleteProjectModal {
            project,
            git: GitSummary {
                is_repo: true,
                branch: Some("main".into()),
                modified: 2,
                untracked: 1,
                ..GitSummary::default()
            },
            instances: vec!["demo".into()],
            scroll: 0,
        }));
        let (text, hits) = render(&app);
        assert!(text.contains("delete project"), "{text}");
        assert!(text.contains("main"), "{text}");
        assert!(text.contains("uncommitted changes"), "{text}");
        assert!(text.contains("Remove, keep files"), "{text}");
        assert!(text.contains("Delete files"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::Confirm)
        );
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::DeleteProjectFiles)
        );
    }

    #[test]
    fn delete_instance_confirm_stage_names_the_instance_and_stop_first() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::DeleteInstance(DeleteInstanceModal {
            instance: instance(&app, "build", "running"),
            stage: DeleteInstanceStage::Confirm,
            error: None,
            scroll: 0,
        }));
        let (text, hits) = render(&app);

        assert!(text.contains("delete instance"), "{text}");
        assert!(text.contains("Delete instance 'build'"), "{text}");
        assert!(text.contains("Project: /projects/demo"), "{text}");
        assert!(text.contains("Workspace: /workspaces/build"), "{text}");
        assert!(text.contains("The VM is stopped first."), "{text}");
        assert!(text.contains("<y> Continue"), "{text}");
        assert!(text.contains("<n> Cancel"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::Confirm)
        );
        assert!(hits.footer.iter().any(|hit| hit.action == ActionId::Cancel));
        assert!(
            !hits
                .footer
                .iter()
                .any(|hit| hit.action == ActionId::KeepFiles),
            "the file question must not appear before the first confirmation"
        );
    }

    #[test]
    fn delete_instance_files_stage_offers_keep_files_and_shows_errors() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::DeleteInstance(DeleteInstanceModal {
            instance: instance(&app, "build", "stopped"),
            stage: DeleteInstanceStage::Files,
            error: Some("workspace is still mounted".into()),
            scroll: 0,
        }));
        let (text, hits) = render(&app);

        assert!(
            text.contains("Also delete the cloned workspace files?"),
            "{text}"
        );
        assert!(text.contains("Choose 'Keep files'"), "{text}");
        assert!(text.contains("<y> Delete files"), "{text}");
        assert!(text.contains("<n> Keep files"), "{text}");
        assert!(text.contains("<Esc> Cancel"), "{text}");
        assert!(text.contains("workspace is still mounted"), "{text}");
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::KeepFiles)
        );
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::Confirm)
        );
        assert!(hits.footer.iter().any(|hit| hit.action == ActionId::Cancel));
    }

    #[test]
    fn delete_project_modal_explains_a_missing_repository_and_kept_files() {
        let (_dir, mut app) = app();
        let project = app.selected_project().cloned().unwrap();
        app.modal = Some(Modal::DeleteProject(DeleteProjectModal {
            project,
            git: GitSummary {
                is_repo: false,
                error: Some("git unavailable".into()),
                ..GitSummary::default()
            },
            instances: Vec::new(),
            scroll: 0,
        }));
        let (text, hits) = render(&app);

        assert!(
            text.contains("not a git repository (git unavailable)"),
            "{text}"
        );
        assert!(!text.contains("uncommitted changes"), "{text}");
        assert!(!text.contains("Instances to remove"), "{text}");
        assert!(text.contains("keep every project file"), "{text}");
        assert!(
            text.contains("Imported projects only lose their picker symlink"),
            "{text}"
        );
        assert!(
            hits.footer
                .iter()
                .any(|hit| hit.action == ActionId::DeleteProjectFiles)
        );
    }
}
