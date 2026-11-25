//! Destructive delete prompts for VMs and projects.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::tui::app::{ActionId, App, DeleteProjectModal, DeleteVmModal, DeleteVmStage};

use super::super::ModalHits;
use super::super::screens::footer::FooterDef;
use super::super::theme::{RED, YELLOW, centered};
use super::message::render_message;

pub(crate) fn render_delete_vm(
    frame: &mut Frame,
    app: &App,
    modal: &DeleteVmModal,
    area: Rect,
) -> ModalHits {
    let (body, color, buttons): (String, Color, &[FooterDef]) = match modal.stage {
        DeleteVmStage::Confirm => (
            format!(
                "Delete VM '{}'?\n\nHarness: {}\nMembers: {}\n\nThe VM is stopped first. \
                 External projects are unassigned, never deleted.",
                modal.vm.name,
                modal.vm.harness.label(),
                modal.members
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
        DeleteVmStage::Files => (
            format!(
                "Also delete the VM directory and its managed workspaces?\n\n{}\n\n\
                 Choose 'Keep files' to unassign every project and leaves the VM directory in place. \
                 External project directories are never touched.",
                modal.vm.config_path.display()
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
        "delete VM",
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
    if !modal.vms.is_empty() {
        body.push_str(&format!(
            "\nDetached from VM(s): {} (the VM itself is never deleted)\n",
            modal.vms.join(", ")
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
    use crate::vm::registry::VmSummary;
    use std::path::PathBuf;

    fn summary(app: &App, name: &str, state: &str) -> VmSummary {
        VmSummary {
            name: name.into(),
            harness: Harness::Opencode,
            primary: Some("demo".into()),
            members: 2,
            project: PathBuf::from("/projects/demo"),
            workspace: PathBuf::from(format!("/workspaces/{name}")),
            created_at: chrono::Utc::now(),
            state: state.into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.vms_dir.join(name).join("vm.toml"),
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
            vms: vec!["rust".into()],
            scroll: 0,
        }));
        let (text, hits) = render(&app);
        assert!(text.contains("delete project"), "{text}");
        assert!(text.contains("main"), "{text}");
        assert!(text.contains("uncommitted changes"), "{text}");
        assert!(text.contains("never deleted"), "{text}");
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
    fn delete_vm_confirm_stage_names_the_vm_and_protects_projects() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::DeleteVm(DeleteVmModal {
            vm: summary(&app, "build", "running"),
            members: 2,
            stage: DeleteVmStage::Confirm,
            error: None,
            scroll: 0,
        }));
        let (text, hits) = render(&app);

        assert!(text.contains("delete VM"), "{text}");
        assert!(text.contains("Delete VM 'build'"), "{text}");
        assert!(
            text.contains("External projects are unassigned, never deleted"),
            "{text}"
        );
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
    fn delete_vm_files_stage_offers_keep_files_and_shows_errors() {
        let (_dir, mut app) = app();
        app.modal = Some(Modal::DeleteVm(DeleteVmModal {
            vm: summary(&app, "build", "stopped"),
            members: 1,
            stage: DeleteVmStage::Files,
            error: Some("VM is still running".into()),
            scroll: 0,
        }));
        let (text, hits) = render(&app);

        assert!(text.contains("Also delete the VM directory"), "{text}");
        assert!(text.contains("Choose 'Keep files'"), "{text}");
        assert!(text.contains("<y> Delete files"), "{text}");
        assert!(text.contains("<n> Keep files"), "{text}");
        assert!(text.contains("<Esc> Cancel"), "{text}");
        assert!(text.contains("VM is still running"), "{text}");
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
            vms: Vec::new(),
            scroll: 0,
        }));
        let (text, hits) = render(&app);

        assert!(
            text.contains("not a git repository (git unavailable)"),
            "{text}"
        );
        assert!(!text.contains("uncommitted changes"), "{text}");
        assert!(!text.contains("Detached from VM"), "{text}");
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
