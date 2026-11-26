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
use crate::session::ssh::{SshIdentity, SshTarget, TitleGuard};
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

/// One resolved interactive SSH session for the TUI.
#[derive(Debug)]
pub(super) struct SshSession {
    pub(super) target: SshTarget,
    pub(super) title: String,
}

/// Host terminal title for an external SSH session: `hegn: <vm> · <project>`.
pub(super) fn ssh_session_title(vm: &str, project: Option<&Path>) -> String {
    match project
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    {
        Some(project) => format!("hegn: {vm} · {project}"),
        None => format!("hegn: {vm}"),
    }
}

/// Resolve a session record into an SSH target.
///
/// A record without the control address or the SSH user was written before
/// SSH support existed. That is a hard error with the serial recovery hint,
/// never a silent fallback to the serial relay.
pub(super) fn ssh_session_from_record(
    config: &crate::config::Config,
    vm: &str,
    record: &crate::vm::manager::SessionRecord,
) -> Result<SshSession> {
    let host = record.guest_ip.with_context(|| {
        format!(
            "VM '{vm}' has no SSH address in its session record. Run 'hegn vm apply {vm}' and \
             restart the VM, or use 'hegn attach {vm} --serial'."
        )
    })?;
    let user = record
        .ssh_user
        .as_deref()
        .filter(|user| !user.trim().is_empty())
        .with_context(|| {
            format!(
                "VM '{vm}' has no SSH user in its session record. Run 'hegn vm apply {vm}' and \
                 restart the VM, or use 'hegn attach {vm} --serial'."
            )
        })?;

    let identity = SshIdentity::new(&config.vms_dir, vm);
    identity.load().with_context(|| {
        format!(
            "cannot open an SSH session to VM '{vm}'; use 'hegn attach {vm} --serial' for the \
             serial console"
        )
    })?;

    let project = if record.project.as_os_str().is_empty() {
        record.workspace.as_path()
    } else {
        record.project.as_path()
    };
    let target = SshTarget::new(
        vm,
        host,
        user,
        identity.key().to_path_buf(),
        identity.known_hosts().to_path_buf(),
    )?;
    Ok(SshSession {
        target,
        title: ssh_session_title(vm, Some(project)),
    })
}

/// Look up a running VM and resolve its interactive SSH target.
pub(super) fn prepare_ssh_session(config: &crate::config::Config, vm: &str) -> Result<SshSession> {
    let registry = crate::vm::registry::VmRegistry::new(config.clone());
    let manager = registry.manager_by_name(vm)?;
    if !manager.is_running() {
        anyhow::bail!(
            "VM '{vm}' is not running. Start it with 'hegn vm start {vm}', or attach the serial \
             console with 'hegn attach {vm} --serial'."
        );
    }
    let record = manager.session().with_context(|| {
        format!(
            "VM '{vm}' has no session record. Run 'hegn vm apply {vm}' and restart the VM, or \
             use 'hegn attach {vm} --serial'."
        )
    })?;
    ssh_session_from_record(config, vm, &record)
}

/// Hand the terminal to an external OpenSSH client and come back to the TUI.
///
/// This is the only external attach transport: the embedded TUI console stays
/// on the serial relay and is neither detached nor reattached here.
pub(super) fn attach_ssh_session(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: &crate::config::Config,
    vm: &str,
    command: Option<&str>,
) -> Result<()> {
    let session = prepare_ssh_session(config, vm)?;
    with_suspended_terminal(terminal, || {
        let _title = TitleGuard::set(&session.title)?;
        session.target.interactive(command)?;
        Ok(())
    })
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
                // External sessions are SSH over the control TAP. The embedded
                // serial console is a separate transport and stays untouched;
                // there is no `ensure_console()` or reattach on this path.
                app.pending_launch = None;
                let config = app.config.clone();
                let result = attach_ssh_session(terminal, &config, &vm, None);
                // The event loop only writes the window title when it changes,
                // so restore it after the session cleared it.
                let _ = super::terminal::set_title(terminal, &app.window_title());
                match result {
                    Ok(()) => app.toast(
                        format!("returned from SSH session to {vm}"),
                        ToastLevel::Info,
                    ),
                    Err(err) => app.toast(
                        format!("SSH attach to {vm} failed: {err:#}"),
                        ToastLevel::Error,
                    ),
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
                    let vms = if outcome.vms.is_empty() {
                        String::new()
                    } else {
                        format!(" · removed {}", outcome.vms.join(", "))
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

    fn keygen_available() -> bool {
        std::process::Command::new("ssh-keygen")
            .arg("-V")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok()
    }

    fn session_record(
        project: &Path,
        guest_ip: Option<std::net::Ipv4Addr>,
        ssh_user: Option<&str>,
    ) -> crate::vm::manager::SessionRecord {
        crate::vm::manager::SessionRecord {
            project: project.to_path_buf(),
            workspace: project.to_path_buf(),
            vm: "rust".into(),
            harness: crate::config::Harness::Shell,
            started_at: chrono::Utc::now(),
            vm_pid: 1,
            vm_binary: None,
            relay_pid: 0,
            virtiofs_pids: Vec::new(),
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
            guest_ip,
            ssh_user: ssh_user.map(str::to_string),
        }
    }

    #[test]
    fn ssh_session_title_uses_vm_and_project() {
        assert_eq!(
            ssh_session_title("rust", Some(Path::new("/projects/demo"))),
            "hegn: rust · demo"
        );
        assert_eq!(ssh_session_title("rust", None), "hegn: rust");
        assert_eq!(
            ssh_session_title("rust", Some(Path::new(""))),
            "hegn: rust",
            "an empty record project must not produce a dangling separator"
        );
    }

    #[test]
    fn a_complete_record_resolves_to_an_ssh_target() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        SshIdentity::new(&config.vms_dir, "rust").ensure().unwrap();
        let record = session_record(
            Path::new("/projects/demo"),
            Some(std::net::Ipv4Addr::new(10, 214, 0, 2)),
            Some("hegn"),
        );

        let session = ssh_session_from_record(&config, "rust", &record).unwrap();

        assert_eq!(session.target.host, std::net::Ipv4Addr::new(10, 214, 0, 2));
        assert_eq!(session.target.user, "hegn");
        assert_eq!(
            session.target.key,
            config.vms_dir.join("rust/ssh/id_ed25519")
        );
        assert_eq!(
            session.target.known_hosts,
            config.vms_dir.join("rust/ssh/known_hosts")
        );
        assert_eq!(session.title, "hegn: rust · demo");
    }

    #[test]
    fn a_record_without_a_control_ip_is_a_hard_error_with_the_serial_hint() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let record = session_record(Path::new("/projects/demo"), None, Some("hegn"));

        let error = format!(
            "{:#}",
            ssh_session_from_record(&config, "rust", &record).unwrap_err()
        );

        assert!(error.contains("hegn attach rust --serial"), "{error}");
        assert!(error.contains("hegn vm apply rust"), "{error}");
    }

    #[test]
    fn a_record_without_an_ssh_user_is_a_hard_error_with_the_serial_hint() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let record = session_record(
            Path::new("/projects/demo"),
            Some(std::net::Ipv4Addr::new(10, 214, 0, 2)),
            None,
        );

        let error = format!(
            "{:#}",
            ssh_session_from_record(&config, "rust", &record).unwrap_err()
        );

        assert!(error.contains("no SSH user"), "{error}");
        assert!(error.contains("hegn attach rust --serial"), "{error}");
        assert!(error.contains("hegn vm apply rust"), "{error}");
    }

    #[test]
    fn a_missing_identity_points_at_apply_and_the_serial_console() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let record = session_record(
            Path::new("/projects/demo"),
            Some(std::net::Ipv4Addr::new(10, 214, 0, 2)),
            Some("root"),
        );

        let error = format!(
            "{:#}",
            ssh_session_from_record(&config, "rust", &record).unwrap_err()
        );

        assert!(error.contains("hegn attach rust --serial"), "{error}");
        assert!(error.contains("hegn vm apply rust"), "{error}");
        assert!(!config.vms_dir.join("rust/ssh").exists());
    }

    #[test]
    fn legacy_projects_fall_back_to_the_workspace_title() {
        if !keygen_available() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        SshIdentity::new(&config.vms_dir, "rust").ensure().unwrap();
        let mut record = session_record(
            Path::new(""),
            Some(std::net::Ipv4Addr::new(10, 214, 0, 2)),
            Some("root"),
        );
        record.workspace = std::path::PathBuf::from("/projects/legacy");

        let session = ssh_session_from_record(&config, "rust", &record).unwrap();

        assert_eq!(session.title, "hegn: rust · legacy");
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
