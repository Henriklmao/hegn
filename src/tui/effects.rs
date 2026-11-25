//! Execution of the state machine's [`Effect`]s against the host.
//!
//! This is the only place where the TUI performs blocking or privileged work
//! inline: everything long-running is delegated to [`super::jobs`]. Actions that
//! need the real terminal are wrapped in
//! [`with_suspended_terminal`](super::terminal::with_suspended_terminal) so raw
//! mode is restored even when they fail.
//!
//! Every host mutation goes through the same library APIs as the CLI
//! (`VmRegistry`, `VmManager`, `ProjectRegistry`, `WorkspaceService`,
//! `SnapshotStore`); the TUI never shells out to `hegn`.

use std::io::Stdout;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::project::{ProjectKind, ProjectRegistry};
use crate::tui::App;
use crate::tui::app::{Effect, ToastLevel};
use crate::tui::jobs::{
    spawn_apply_vm, spawn_assign_project, spawn_attach_vm, spawn_create_vm, spawn_delete_vm,
    spawn_fork_workspace, spawn_image_download, spawn_scan, spawn_set_primary, spawn_start_vm,
    spawn_stop_vm, spawn_unassign_project,
};
use crate::tui::launch::launch_agent;
use crate::tui::messages::ScanMessage;
use crate::tui::terminal::with_suspended_terminal;

/// Delete one dashboard project, routing registry-only External projects through
/// the domain helper and leaving clone/worktree removal to the VM surfaces.
pub(super) fn delete_project_entry(
    config: &crate::config::Config,
    project: &Path,
    delete_files: bool,
) -> anyhow::Result<crate::project::ProjectDeleteOutcome> {
    let canonical = project.canonicalize().ok();
    let registry = ProjectRegistry::load(&config.state_dir)?;
    let entry = canonical
        .as_deref()
        .and_then(|path| registry.find_by_path(path))
        .cloned();
    match entry {
        Some(entry) if entry.kind == ProjectKind::External => {
            crate::project::delete_registered_project(config, &entry, delete_files)
        }
        Some(entry) => {
            anyhow::bail!(
                "project '{}' is a {}; delete it from the VM/workspace surface",
                entry.name,
                match entry.kind {
                    ProjectKind::Clone => "managed clone",
                    ProjectKind::Worktree => "worktree",
                    ProjectKind::External => unreachable!(),
                }
            )
        }
        None => crate::project::delete_project(config, project, delete_files),
    }
}

pub(super) fn handle_effects(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    effects: Vec<Effect>,
    scan_tx: &mpsc::Sender<ScanMessage>,
    runtime: &tokio::runtime::Runtime,
) -> Result<()> {
    for effect in effects {
        match effect {
            Effect::Quit => app.should_quit = true,
            Effect::RefreshProjects => {
                app.loading = true;
                app.toast("refreshing projects…", ToastLevel::Info);
                spawn_scan(app.config.clone(), scan_tx.clone());
            }
            Effect::RefreshVms => app.refresh_vms(),
            Effect::DownloadImages => {
                app.toast("downloading VM images…", ToastLevel::Info);
                spawn_image_download(app.config.clone(), scan_tx.clone(), Vec::new());
            }
            Effect::SetupEnvironment => {
                let actions = crate::diagnostics::doctor::setup_actions(&app.config);
                let mut notes = Vec::new();
                let mut failed = false;
                let mut modal =
                    crate::tui::app::DoctorModal::new(crate::diagnostics::doctor::run(&app.config));
                modal.setting_up = true;
                modal.downloading = actions.download_images;
                app.modal = Some(crate::tui::app::Modal::Doctor(modal));
                app.toast("setting up hegn…", ToastLevel::Info);

                if actions.create_dirs {
                    match app.config.ensure_dirs() {
                        Ok(()) => notes.push("directories created".to_string()),
                        Err(err) => {
                            failed = true;
                            notes.push(format!("directory setup failed: {err:#}"));
                        }
                    }
                }
                if actions.init_config {
                    match app.config.write_default_if_missing() {
                        Ok(true) => notes.push("default config written".to_string()),
                        Ok(false) => {}
                        Err(err) => {
                            failed = true;
                            notes.push(format!("config setup failed: {err:#}"));
                        }
                    }
                }
                if actions.init_profiles {
                    match crate::agent::profile::ProfileStore::new(&app.config).write_templates() {
                        Ok(paths) if !paths.is_empty() => {
                            notes.push(format!("{} profile template(s) written", paths.len()))
                        }
                        Ok(_) => {}
                        Err(err) => {
                            failed = true;
                            notes.push(format!("profile setup failed: {err:#}"));
                        }
                    }
                }
                if actions.configure_network {
                    app.toast("configuring the guarded network…", ToastLevel::Info);
                    let ports = app.config.endpoint_ports();
                    match with_suspended_terminal(terminal, || setup_network(&ports)) {
                        Ok(()) => notes.push("guarded network configured".to_string()),
                        Err(err) => {
                            failed = true;
                            notes.push(format!("network setup failed: {err:#}"));
                        }
                    }
                }
                if actions.download_images {
                    app.toast("downloading VM images…", ToastLevel::Info);
                    spawn_image_download(app.config.clone(), scan_tx.clone(), notes);
                } else {
                    let note = if notes.is_empty() {
                        "nothing to set up".to_string()
                    } else {
                        notes.join("; ")
                    };
                    app.show_doctor(crate::diagnostics::doctor::run(&app.config), Some(note));
                }
                if failed && actions.download_images {
                    app.toast("some setup steps failed; see the doctor", ToastLevel::Error);
                }
            }
            Effect::OpenEditor(path) => {
                match with_suspended_terminal(terminal, || open_editor(&path)) {
                    Ok(()) => {
                        app.reload_after_edit();
                        app.toast("returned from editor", ToastLevel::Info);
                    }
                    Err(err) => app.toast(format!("editor failed: {err}"), ToastLevel::Error),
                }
            }
            Effect::OpenProfileEditor(harness) => {
                let store = crate::agent::profile::ProfileStore::new(&app.config);
                let path = store.path(harness);
                if !path.is_file()
                    && let Err(err) = store.write_templates()
                {
                    app.toast(
                        format!("could not create profile template: {err:#}"),
                        ToastLevel::Error,
                    );
                    continue;
                }
                match with_suspended_terminal(terminal, || open_editor(&path)) {
                    Ok(()) => {
                        app.reload_after_edit();
                        app.reopen_global_config(
                            crate::config::form::ConfigPage::Profiles,
                            Some(format!("{} profile edited", harness.label())),
                        );
                    }
                    Err(err) => app.toast(format!("editor failed: {err}"), ToastLevel::Error),
                }
            }
            Effect::OpenLazygit(path) => {
                if let Err(err) = with_suspended_terminal(terminal, || open_lazygit(&path)) {
                    app.toast(format!("lazygit failed: {err}"), ToastLevel::Error);
                }
            }
            Effect::StartVm { vm, project } => {
                app.toast(format!("starting VM {vm}…"), ToastLevel::Info);
                spawn_start_vm(app.config.clone(), vm, project, scan_tx.clone());
            }
            Effect::AttachVm { vm } => {
                app.toast(format!("attaching to VM {vm}…"), ToastLevel::Info);
                spawn_attach_vm(app.config.clone(), vm, scan_tx.clone());
            }
            Effect::ApplyVm { vm } => {
                app.toast(format!("restarting VM {vm}…"), ToastLevel::Info);
                spawn_apply_vm(app.config.clone(), vm, scan_tx.clone());
            }
            Effect::StopVm { vm } => {
                app.toast(format!("stopping VM {vm}…"), ToastLevel::Info);
                spawn_stop_vm(app.config.clone(), vm, scan_tx.clone());
            }
            Effect::DeleteVm { vm, delete_files } => {
                spawn_delete_vm(app.config.clone(), vm, delete_files, scan_tx.clone());
            }
            Effect::CreateVm {
                name,
                harness,
                vcpus,
                memory_mib,
                packages,
            } => {
                app.toast(format!("creating VM {name}…"), ToastLevel::Info);
                spawn_create_vm(
                    app.config.clone(),
                    name,
                    harness,
                    vcpus,
                    memory_mib,
                    packages,
                    scan_tx.clone(),
                );
            }
            Effect::AssignProject {
                project,
                path,
                vm,
                read_only,
            } => {
                app.toast(format!("assigning {project} to {vm}…"), ToastLevel::Info);
                spawn_assign_project(
                    app.config.clone(),
                    project,
                    path,
                    vm,
                    read_only,
                    scan_tx.clone(),
                );
            }
            Effect::UnassignProject { project } => {
                app.toast(format!("detaching {project}…"), ToastLevel::Info);
                spawn_unassign_project(app.config.clone(), project, scan_tx.clone());
            }
            Effect::SetVmPrimary { vm, project } => {
                app.toast(format!("setting {project} as primary…"), ToastLevel::Info);
                spawn_set_primary(app.config.clone(), vm, project, scan_tx.clone());
            }
            Effect::ForkWorkspace { parent, name, mode } => {
                app.toast(format!("forking {parent}…"), ToastLevel::Info);
                spawn_fork_workspace(app.config.clone(), parent, name, mode, scan_tx.clone());
            }
            Effect::LaunchAgent { spec, vm, project } => {
                launch_agent(terminal, app, spec, vm, project)?;
            }
            Effect::AttachConsole { vm } => {
                let registry = crate::vm::registry::VmRegistry::new(app.config.clone());
                // Release the embedded client first. Otherwise the external
                // attach replaces it in the relay and the TUI keeps a dead
                // socket when the user comes back.
                app.pending_launch = None;
                app.detach_console();
                let result = with_suspended_terminal(terminal, || {
                    let manager = registry.manager_by_name(&vm)?;
                    let console = manager.ensure_console()?;
                    crate::session::console::run_interactive(console, true)
                });
                let reattached = registry
                    .manager_by_name(&vm)
                    .ok()
                    .and_then(|manager| manager.attach_console().ok());
                if let Some(console) = reattached {
                    app.attach_console(Box::new(console));
                    app.project_view = crate::tui::app::ProjectView::Split;
                    app.terminal_focus = true;
                } else {
                    match result {
                        Ok(()) => {
                            app.project_view = crate::tui::app::ProjectView::Split;
                            app.terminal_focus = true;
                            app.toast("detached from console", ToastLevel::Info);
                        }
                        Err(err) => app.toast(format!("attach failed: {err}"), ToastLevel::Error),
                    }
                }
            }
            Effect::DeleteProject {
                project,
                delete_files,
            } => match delete_project_entry(&app.config, &project, delete_files) {
                Ok(outcome) => {
                    app.reload_state();
                    app.forget_project_locally(&project);
                    app.refresh_vms();
                    let files = if outcome.files_deleted {
                        "files deleted"
                    } else {
                        "files kept"
                    };
                    let vms = if outcome.instances.is_empty() {
                        String::new()
                    } else {
                        format!(" · removed {}", outcome.instances.join(", "))
                    };
                    app.toast(
                        format!("project {} deleted ({files}){vms}", project.display()),
                        ToastLevel::Info,
                    );
                }
                Err(err) => app.toast(format!("delete project failed: {err:#}"), ToastLevel::Error),
            },
            Effect::RestoreSnapshot(snapshot) => {
                let registry = crate::vm::registry::VmRegistry::new(app.config.clone());
                let Some(vm) = snapshot
                    .vm()
                    .and_then(|name| registry.store().load(name).ok())
                else {
                    app.toast("restore failed: no VM for snapshot", ToastLevel::Error);
                    continue;
                };
                let manager = registry.manager(vm)?;
                match runtime.block_on(manager.restore_snapshot(&snapshot)) {
                    Ok(()) => match manager.attach_console() {
                        Ok(console) => {
                            app.attach_console(Box::new(console));
                            app.toast(format!("restored {}", snapshot.name), ToastLevel::Info);
                        }
                        Err(err) => app.fail_start(format!(
                            "snapshot restored, but console attach failed: {err}"
                        )),
                    },
                    Err(err) => app.toast(format!("restore failed: {err}"), ToastLevel::Error),
                }
            }
        }
    }
    Ok(())
}

fn open_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$1\""))
        .arg("hegn-editor")
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    Ok(())
}

fn setup_network(ports: &[u16]) -> Result<()> {
    let user = crate::network::invoking_user()
        .context("could not determine the current user for TAP ownership")?;
    crate::network::setup_with_privilege(&user, ports)
}

fn open_lazygit(path: &Path) -> Result<()> {
    let status = Command::new("lazygit")
        .current_dir(path)
        .status()
        .context("failed to launch lazygit (is it installed?)")?;
    if !status.success() {
        anyhow::bail!("lazygit exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.projects_root = dir.join("projects");
        config.state_dir = dir.join("state");
        config.snapshots_dir = dir.join("snapshots");
        config.vms_dir = dir.join("vms");
        config.templates_dir = dir.join("templates");
        config.profiles_dir = dir.join("profiles");
        std::fs::create_dir_all(&config.projects_root).unwrap();
        config
    }

    fn register(config: &Config, name: &str, path: &Path, kind: ProjectKind) -> ProjectEntry {
        let entry = ProjectEntry {
            name: name.into(),
            path: path.canonicalize().unwrap(),
            vm: None,
            read_only: false,
            kind,
            parent: None,
            managed: matches!(kind, ProjectKind::Clone),
            hidden: false,
            created_at: chrono::Utc::now(),
        };
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(entry.clone())
            .unwrap();
        entry
    }

    #[test]
    fn outside_keep_files_unregisters_without_touching_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let outside = dir.path().join("code/parser");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("README.md"), b"keep").unwrap();
        register(&config, "parser", &outside, ProjectKind::External);

        let outcome = delete_project_entry(&config, &outside, false).unwrap();

        assert!(!outcome.files_deleted);
        assert!(outside.join("README.md").is_file());
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("parser")
                .is_none()
        );
    }

    #[test]
    fn outside_delete_files_removes_the_canonical_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let outside = dir.path().join("code/parser");
        std::fs::create_dir_all(&outside).unwrap();
        let canonical = outside.canonicalize().unwrap();
        register(&config, "parser", &outside, ProjectKind::External);

        let outcome = delete_project_entry(&config, &outside, true).unwrap();

        assert!(outcome.files_deleted);
        assert!(!canonical.exists());
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("parser")
                .is_none()
        );
    }

    #[test]
    fn unsafe_registry_path_is_refused_and_stays_registered() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        register(&config, "root", Path::new("/"), ProjectKind::External);

        let err = delete_project_entry(&config, Path::new("/"), true)
            .unwrap_err()
            .to_string();

        assert!(err.contains("refusing"), "{err}");
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("root")
                .is_some(),
            "a refused delete must keep the registry entry"
        );
        assert!(Path::new("/").is_dir());
    }

    #[test]
    fn picker_delete_keeps_the_existing_keep_files_behaviour() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let picker = config.projects_root.join("demo");
        std::fs::create_dir_all(&picker).unwrap();
        std::fs::write(picker.join("README.md"), b"keep").unwrap();
        register(&config, "demo", &picker, ProjectKind::External);

        let outcome = delete_project_entry(&config, &picker, false).unwrap();

        assert!(!outcome.files_deleted);
        assert!(picker.join("README.md").is_file());
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("demo")
                .is_none()
        );
    }

    #[test]
    fn clone_registered_paths_are_left_to_the_workspace_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let clone = config.vms_dir.join("rust/workspaces/parser-fix");
        std::fs::create_dir_all(&clone).unwrap();
        register(&config, "parser-fix", &clone, ProjectKind::Clone);

        let err = delete_project_entry(&config, &clone, false)
            .unwrap_err()
            .to_string();

        assert!(err.contains("VM/workspace surface"), "{err}");
        assert!(clone.is_dir());
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("parser-fix")
                .is_some()
        );
    }
}
