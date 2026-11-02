//! Execution of the state machine's [`Effect`]s against the host.
//!
//! This is the only place where the TUI performs blocking or privileged work
//! inline: everything long-running is delegated to [`super::jobs`]. Actions that
//! need the real terminal are wrapped in
//! [`with_suspended_terminal`](super::terminal::with_suspended_terminal) so raw
//! mode is restored even when they fail.

use std::io::Stdout;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::tui::App;
use crate::tui::app::{Effect, ToastLevel};
use crate::tui::jobs::{spawn_attach, spawn_clones, spawn_image_download, spawn_scan, spawn_start};
use crate::tui::launch::launch_agent;
use crate::tui::messages::ScanMessage;
use crate::tui::terminal::with_suspended_terminal;

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
            Effect::StartProjectInstance { project, harness } => {
                app.toast("starting VM…", ToastLevel::Info);
                spawn_start(app.config.clone(), project, harness, scan_tx.clone());
            }
            Effect::LaunchAgent {
                spec,
                instance,
                project,
            } => {
                launch_agent(terminal, app, spec, instance, project)?;
            }
            Effect::AttachInstance { instance } => {
                app.toast(format!("attaching to {instance}…"), ToastLevel::Info);
                spawn_attach(app.config.clone(), instance, scan_tx.clone());
            }
            Effect::AttachConsole { instance } => {
                let registry = crate::vm::instance::InstanceRegistry::new(app.config.clone());
                // Release the embedded client first. Otherwise the external
                // attach replaces it in the relay and the TUI keeps a dead
                // socket when the user comes back.
                app.pending_launch = None;
                app.detach_console();
                let result = with_suspended_terminal(terminal, || {
                    let config = registry.resolve(&instance)?;
                    let console = registry.manager(config).ensure_console()?;
                    crate::session::console::run_interactive(console, true)
                });
                let reattached = registry
                    .resolve(&instance)
                    .ok()
                    .and_then(|config| registry.manager(config).attach_console().ok());
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
            Effect::StopInstance { instance } => {
                let registry = crate::vm::instance::InstanceRegistry::new(app.config.clone());
                app.pending_launch = None;
                match registry.stop(&instance) {
                    Ok(()) => {
                        app.toast(format!("instance {instance} stopped"), ToastLevel::Info);
                        app.refresh_instances();
                    }
                    Err(err) => app.toast(format!("stop failed: {err}"), ToastLevel::Error),
                }
            }
            Effect::DeleteInstance {
                instance,
                delete_files,
            } => {
                let registry = crate::vm::instance::InstanceRegistry::new(app.config.clone());
                match registry.delete(&instance, delete_files) {
                    Ok(outcome) => {
                        let message = match outcome.kept_workspace {
                            Some(path) => format!(
                                "instance {instance} removed; files kept at {}",
                                path.display()
                            ),
                            None if delete_files => {
                                format!("instance {instance} and files deleted")
                            }
                            None => format!("instance {instance} removed; project files kept"),
                        };
                        app.toast(message, ToastLevel::Info);
                        app.refresh_instances();
                    }
                    Err(err) => app.toast(format!("delete failed: {err}"), ToastLevel::Error),
                }
            }
            Effect::DeleteProject {
                project,
                delete_files,
            } => match crate::project::delete_project(&app.config, &project, delete_files) {
                Ok(outcome) => {
                    app.reload_state();
                    app.forget_project_locally(&project);
                    app.refresh_instances();
                    let files = if outcome.files_deleted {
                        "files deleted"
                    } else {
                        "files kept"
                    };
                    let instances = if outcome.instances.is_empty() {
                        String::new()
                    } else {
                        format!(" · removed {}", outcome.instances.join(", "))
                    };
                    app.toast(
                        format!("project {} deleted ({files}){instances}", project.display()),
                        ToastLevel::Info,
                    );
                }
                Err(err) => app.toast(format!("delete project failed: {err:#}"), ToastLevel::Error),
            },
            Effect::CloneInstance {
                source,
                count,
                harness,
                start,
            } => {
                app.toast(format!("creating {count} clone(s)…"), ToastLevel::Info);
                spawn_clones(
                    app.config.clone(),
                    source,
                    count,
                    harness,
                    start,
                    scan_tx.clone(),
                );
            }
            Effect::RefreshInstances => app.refresh_instances(),
            Effect::RestoreSnapshot(snapshot) => {
                let registry = crate::vm::instance::InstanceRegistry::new(app.config.clone());
                let instance = snapshot
                    .instance
                    .as_deref()
                    .and_then(|name| registry.store().load(name).ok())
                    .or_else(|| {
                        snapshot
                            .project
                            .as_deref()
                            .and_then(|project| registry.resolve(&project.to_string_lossy()).ok())
                    });
                let Some(instance) = instance else {
                    app.toast(
                        "restore failed: no instance for snapshot",
                        ToastLevel::Error,
                    );
                    continue;
                };
                let manager = registry.manager(instance);
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
