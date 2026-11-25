//! Background workers for project scans, images, VM lifecycle and provisioning.
//!
//! Every `spawn_*` function moves blocking host work onto a named thread and
//! reports back over a [`ScanMessage`] channel. Keep the thread boundaries here:
//! the event loop must stay responsive while a scan, download, fork or VM start
//! is in flight, and a worker failure must never take the TUI down with it.
//!
//! Membership, VM and snapshot mutations go through the same library APIs the
//! CLI uses (`VmRegistry`, `VmManager`, `ProjectRegistry`, `WorkspaceService`);
//! the TUI never shells out to `hegn`.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::agent::provision::{ProvisionEvent, ProvisionPlan, ProvisionWatcher};
use crate::config::{Config, Harness};
use crate::project::{self, ProjectEntry, ProjectKind, ProjectRegistry};
use crate::project::{ForkMode, WorkspaceService};
use crate::tui::app::ToastLevel;
use crate::tui::messages::ScanMessage;
use crate::vm::config::{VmConfig, VmStore};
use crate::vm::registry::VmRegistry;

pub(super) fn spawn_scan(config: Config, tx: mpsc::Sender<ScanMessage>) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-project-scan".into())
        .spawn(move || {
            let protected = config.protected_paths();
            match project::discover_protected(&config.projects_root, &protected) {
                Ok(projects) => {
                    let _ = thread_tx.send(ScanMessage::Projects(projects));
                }
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::Error(err.to_string()));
                }
            }
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::Error(format!(
            "could not start project scanner: {err}"
        )));
    }
}

pub(super) fn spawn_image_download(
    config: Config,
    tx: mpsc::Sender<ScanMessage>,
    setup_notes: Vec<String>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-images".into())
        .spawn(move || {
            let progress_tx = thread_tx.clone();
            let result = crate::vm::images::download_missing_with_progress(
                &config,
                false,
                &mut |progress| {
                    let _ = progress_tx.send(ScanMessage::DownloadProgress(progress));
                },
            );
            let report = crate::diagnostics::doctor::run(&config);
            let mut note = setup_notes;
            note.push(match result {
                Ok(_) => "VM images downloaded and verified".to_string(),
                Err(err) => format!("image download failed: {err:#}"),
            });
            let _ = thread_tx.send(ScanMessage::Doctor {
                report,
                note: Some(note.join("; ")),
            });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::Error(format!(
            "could not start image download: {err}"
        )));
    }
}

pub(super) fn spawn_attach_vm(config: Config, vm: String, tx: mpsc::Sender<ScanMessage>) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-attach-vm".into())
        .spawn(move || {
            let registry = VmRegistry::new(config);
            let harness = registry
                .store()
                .load(&vm)
                .map(|config| config.harness)
                .unwrap_or_default();
            let warning = registry
                .manager_by_name(&vm)
                .ok()
                .and_then(|manager| manager.membership_warning());
            if let Some(warning) = warning {
                let _ = thread_tx.send(ScanMessage::VmsChanged {
                    note: warning,
                    level: ToastLevel::Warn,
                });
            }
            let result = registry
                .manager_by_name(&vm)
                .and_then(|manager| manager.ensure_console());
            match result {
                Ok(console) => {
                    let _ = thread_tx.send(ScanMessage::Started {
                        console: Box::new(console),
                        branch: None,
                        harness,
                        started: false,
                    });
                }
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::StartFailed(err.to_string()));
                }
            }
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::StartFailed(format!(
            "could not start the attach thread: {err}"
        )));
    }
}

pub(super) fn spawn_start_vm(
    config: Config,
    vm: String,
    project: Option<PathBuf>,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-start-vm".into())
        .spawn(move || {
            let registry = VmRegistry::new(config.clone());
            let manager = match registry.manager_by_name(&vm) {
                Ok(manager) => manager,
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::StartFailed(err.to_string()));
                    return;
                }
            };
            let harness = manager.vm().harness;
            let plan = ProvisionPlan::new(harness, &manager.vm().extra_packages).ok();
            let was_running = manager.is_running();
            let started = if was_running {
                manager.status().ok()
            } else {
                match manager.start() {
                    Ok(report) => Some(report),
                    Err(err) => {
                        let _ = thread_tx.send(ScanMessage::StartFailed(err.to_string()));
                        return;
                    }
                }
            };
            let warning = manager.membership_warning();
            if let Some(warning) = warning {
                let _ = thread_tx.send(ScanMessage::VmsChanged {
                    note: warning,
                    level: ToastLevel::Warn,
                });
            }
            if let (Some(report), Some(plan)) = (&started, plan)
                && let Some(log) = &report.serial_log
            {
                spawn_provision_watch(log.clone(), plan, thread_tx.clone());
            }
            match manager.attach_console() {
                Ok(console) => {
                    let branch = project.as_ref().and_then(|project| {
                        (config.auto_branch && project::read_git_summary(project).is_repo).then(
                            || {
                                project::create_session_branch(project)
                                    .map_err(|err| err.to_string())
                            },
                        )
                    });
                    let _ = thread_tx.send(ScanMessage::Started {
                        console: Box::new(console),
                        branch,
                        harness,
                        started: !was_running,
                    });
                }
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::StartFailed(err.to_string()));
                }
            }
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::StartFailed(format!(
            "could not start the VM start thread: {err}"
        )));
    }
}

pub(super) fn spawn_stop_vm(config: Config, vm: String, tx: mpsc::Sender<ScanMessage>) {
    spawn_lifecycle(config, vm, Lifecycle::Stop, tx);
}

pub(super) fn spawn_apply_vm(config: Config, vm: String, tx: mpsc::Sender<ScanMessage>) {
    spawn_lifecycle(config, vm, Lifecycle::Apply, tx);
}

enum Lifecycle {
    Stop,
    Apply,
}

fn spawn_lifecycle(
    config: Config,
    vm: String,
    operation: Lifecycle,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-vm-lifecycle".into())
        .spawn(move || {
            let registry = VmRegistry::new(config);
            let result = match operation {
                Lifecycle::Stop => registry.stop(&vm),
                Lifecycle::Apply => registry
                    .manager_by_name(&vm)
                    .and_then(|manager| manager.apply().map(|_| ())),
            };
            match result {
                Ok(()) => {
                    let note = match operation {
                        Lifecycle::Stop => format!("VM {vm} stopped"),
                        Lifecycle::Apply => format!("VM {vm} restarted with the current config"),
                    };
                    let _ = thread_tx.send(ScanMessage::VmsChanged {
                        note,
                        level: ToastLevel::Info,
                    });
                }
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::VmsChanged {
                        note: format!("VM operation failed: {err:#}"),
                        level: ToastLevel::Error,
                    });
                }
            }
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::VmsChanged {
            note: format!("could not start the VM thread: {err}"),
            level: ToastLevel::Error,
        });
    }
}

pub(super) fn spawn_create_vm(
    config: Config,
    name: String,
    harness: Harness,
    vcpus: u8,
    memory_mib: u32,
    packages: Vec<String>,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-create-vm".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let store = VmStore::new(&config);
                if store.dir(&name)?.exists() {
                    anyhow::bail!("VM '{name}' already exists; choose another name");
                }
                let mut vm = VmConfig::from_config(&config, &name, Some(harness));
                vm.vcpus = vcpus;
                vm.memory_mib = memory_mib;
                for package in packages {
                    if !crate::config::safe_package_name(&package) {
                        anyhow::bail!("invalid package name: {package:?}");
                    }
                    if !vm
                        .extra_packages
                        .iter()
                        .any(|existing| existing == &package)
                    {
                        vm.extra_packages.push(package);
                    }
                }
                vm.validate()?;
                store.save(&vm)?;
                Ok(())
            })();
            let (note, level) = match result {
                Ok(()) => (
                    format!("created VM {name} — assign projects from its VM screen (v)"),
                    ToastLevel::Info,
                ),
                Err(err) => (format!("create VM failed: {err:#}"), ToastLevel::Error),
            };
            let _ = thread_tx.send(ScanMessage::VmsChanged { note, level });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::VmsChanged {
            note: format!("could not start the VM create thread: {err}"),
            level: ToastLevel::Error,
        });
    }
}

pub(super) fn spawn_assign_project(
    config: Config,
    project: String,
    path: PathBuf,
    vm: String,
    read_only: bool,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-assign-project".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let mut registry = ProjectRegistry::load(&config.state_dir)?;
                if registry.get(&project).is_none() {
                    let canonical = path.canonicalize().map_err(|err| {
                        anyhow::anyhow!("project path {} is not usable: {err}", path.display())
                    })?;
                    registry.upsert(ProjectEntry {
                        name: project.clone(),
                        path: canonical,
                        vm: None,
                        read_only: false,
                        kind: ProjectKind::External,
                        parent: None,
                        managed: false,
                        hidden: false,
                        created_at: chrono::Utc::now(),
                    })?;
                }
                VmStore::new(&config)
                    .load(&vm)
                    .map_err(|err| anyhow::anyhow!("unknown VM '{vm}': {err}"))?;
                registry.assign(&project, &vm, read_only)?;
                if let Err(err) = registry.validate(&config, &VmStore::new(&config)) {
                    // Never leave a validated-away assignment behind.
                    let _ = registry.unassign(&project);
                    return Err(err);
                }
                Ok(())
            })();
            let (note, level) = match result {
                Ok(()) => (format!("assigned {project} to VM {vm}"), ToastLevel::Info),
                Err(err) => (format!("assign failed: {err:#}"), ToastLevel::Error),
            };
            let _ = thread_tx.send(ScanMessage::VmsChanged { note, level });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::VmsChanged {
            note: format!("could not start the assign thread: {err}"),
            level: ToastLevel::Error,
        });
    }
}

pub(super) fn spawn_unassign_project(
    config: Config,
    project: String,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-unassign-project".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let mut registry = ProjectRegistry::load(&config.state_dir)?;
                registry.unassign(&project)?;
                registry.validate(&config, &VmStore::new(&config))?;
                Ok(())
            })();
            let (note, level) = match result {
                Ok(()) => (
                    format!("detached {project} from its VM; files untouched"),
                    ToastLevel::Info,
                ),
                Err(err) => (format!("detach failed: {err:#}"), ToastLevel::Error),
            };
            let _ = thread_tx.send(ScanMessage::VmsChanged { note, level });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::VmsChanged {
            note: format!("could not start the detach thread: {err}"),
            level: ToastLevel::Error,
        });
    }
}

pub(super) fn spawn_set_primary(
    config: Config,
    vm: String,
    project: String,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-vm-primary".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let store = VmStore::new(&config);
                let mut config_vm = store.load(&vm)?;
                let registry = ProjectRegistry::load(&config.state_dir)?;
                let entry = registry
                    .get(&project)
                    .ok_or_else(|| anyhow::anyhow!("unknown project '{project}'"))?;
                if entry.vm.as_deref() != Some(vm.as_str()) {
                    anyhow::bail!("project '{project}' is not assigned to VM '{vm}'");
                }
                if entry.kind == ProjectKind::Worktree {
                    anyhow::bail!("project '{project}' is a worktree and rides its parent's share");
                }
                config_vm.primary = Some(project.clone());
                store.save(&config_vm)?;
                Ok(())
            })();
            let (note, level) = match result {
                Ok(()) => (
                    format!("VM {vm} primary is now {project}; apply to restart"),
                    ToastLevel::Info,
                ),
                Err(err) => (format!("set primary failed: {err:#}"), ToastLevel::Error),
            };
            let _ = thread_tx.send(ScanMessage::VmsChanged { note, level });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::VmsChanged {
            note: format!("could not start the primary thread: {err}"),
            level: ToastLevel::Error,
        });
    }
}

pub(super) fn spawn_fork_workspace(
    config: Config,
    parent: String,
    name: String,
    mode: ForkMode,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-fork-workspace".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<String> {
                let store = VmStore::new(&config);
                let mut registry = ProjectRegistry::load(&config.state_dir)?;
                let service = WorkspaceService::new(&config, &store);
                let entry = service.fork(&mut registry, &parent, &name, mode, None)?;
                Ok(entry.path.display().to_string())
            })();
            let (note, level) = match result {
                Ok(path) => {
                    let verb = match mode {
                        ForkMode::Clone => "clone",
                        ForkMode::Worktree => "worktree",
                    };
                    (format!("created {verb} {name} at {path}"), ToastLevel::Info)
                }
                Err(err) => (format!("fork failed: {err:#}"), ToastLevel::Error),
            };
            let _ = thread_tx.send(ScanMessage::VmsChanged { note, level });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::VmsChanged {
            note: format!("could not start the fork thread: {err}"),
            level: ToastLevel::Error,
        });
    }
}

pub(super) fn spawn_delete_vm(
    config: Config,
    vm: String,
    delete_files: bool,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-delete-vm".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<String> {
                let registry = VmRegistry::new(config.clone());
                let (worktrees, clones) = vm_workspace_entries(&config, &vm)?;
                let outcome = registry.delete(&vm, delete_files)?;
                let mut projects = ProjectRegistry::load(&config.state_dir)?;
                for name in worktrees.iter().chain(clones.iter()) {
                    projects.remove(name)?;
                }
                Ok(match (&outcome.kept_workspace, delete_files) {
                    (Some(path), _) => {
                        format!("VM {vm} removed; workspaces kept at {}", path.display())
                    }
                    (None, true) => format!("VM {vm} and its managed workspaces deleted"),
                    (None, false) => format!("VM {vm} removed; files kept"),
                })
            })();
            let (note, level) = match result {
                Ok(note) => (note, ToastLevel::Info),
                Err(err) => (format!("delete VM failed: {err:#}"), ToastLevel::Error),
            };
            let _ = thread_tx.send(ScanMessage::VmsChanged { note, level });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::VmsChanged {
            note: format!("could not start the VM delete thread: {err}"),
            level: ToastLevel::Error,
        });
    }
}

/// Managed clone and worktree entries of a VM, in removal order.
fn vm_workspace_entries(config: &Config, vm: &str) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let registry = ProjectRegistry::load(&config.state_dir)?;
    let mut worktrees = Vec::new();
    let mut clones = Vec::new();
    for entry in registry.entries() {
        if entry.vm.as_deref() != Some(vm) {
            continue;
        }
        match entry.kind {
            ProjectKind::Worktree => worktrees.push(entry.name.clone()),
            ProjectKind::Clone => clones.push(entry.name.clone()),
            ProjectKind::External => {}
        }
    }
    Ok((worktrees, clones))
}

/// Follow the session log while cloud-init installs the harness preset.
///
/// The watcher never owns the console socket; it only reads the relay's log, so
/// the user can attach immediately and still see the progress notification.
fn spawn_provision_watch(log: PathBuf, plan: ProvisionPlan, tx: mpsc::Sender<ScanMessage>) {
    let spawned = std::thread::Builder::new()
        .name("hegn-provision-watch".into())
        .spawn(move || {
            let Ok(mut watcher) = ProvisionWatcher::open(&log, &plan) else {
                return;
            };
            let started = Instant::now();
            let mut announced = false;
            loop {
                let events = match watcher.poll() {
                    Ok(events) => events,
                    Err(err) => {
                        if announced {
                            let _ = tx.send(ScanMessage::ProvisionFailed(format!(
                                "could not read the provisioning log: {err}"
                            )));
                        }
                        return;
                    }
                };
                if !events.is_empty() {
                    announced = true;
                    if tx
                        .send(ScanMessage::Provision(watcher.progress().clone()))
                        .is_err()
                    {
                        return;
                    }
                }
                for event in events {
                    match event {
                        ProvisionEvent::Stage(_) => {}
                        ProvisionEvent::Ready => {
                            let _ = tx.send(ScanMessage::ProvisionReady);
                            return;
                        }
                        ProvisionEvent::Failed { code, message } => {
                            let detail = message.unwrap_or_else(|| {
                                code.map_or_else(
                                    || "the guest provisioner failed".to_string(),
                                    |code| format!("exit status {code}"),
                                )
                            });
                            let _ = tx.send(ScanMessage::ProvisionFailed(detail));
                            return;
                        }
                    }
                }
                if watcher.finished() {
                    return;
                }
                // A pre-provisioned image (or `cloud_init = false`) never emits
                // progress. Keep the watcher long enough to cover a slow boot,
                // then stop quietly instead of leaving a thread behind.
                if !announced && started.elapsed() > Duration::from_secs(5 * 60) {
                    return;
                }
                if started.elapsed() > Duration::from_secs(60 * 60) {
                    let _ = tx.send(ScanMessage::ProvisionFailed(
                        "timed out waiting for the harness preset".into(),
                    ));
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
    // The VM is already running. Progress is best-effort; never turn a
    // watcher failure into a start failure.
    let _ = spawned;
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.projects_root = dir.join("projects");
        config.state_dir = dir.join("state");
        config.snapshots_dir = dir.join("snapshots");
        config.templates_dir = dir.join("templates");
        config.vms_dir = dir.join("vms");
        config.profiles_dir = dir.join("profiles");
        config.kernel_image = dir.join("missing-kernel");
        config.rootfs_image = dir.join("missing-rootfs");
        std::fs::create_dir_all(&config.projects_root).unwrap();
        config
    }

    #[test]
    fn vm_start_runs_off_the_ui_thread_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn_start_vm(config, "missing".into(), Some(project), tx);
        let message = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker did not report a result");
        assert!(matches!(message, ScanMessage::StartFailed(_)));
    }

    #[test]
    fn create_vm_reports_success_and_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let (tx, rx) = mpsc::channel();
        spawn_create_vm(
            config.clone(),
            "rust".into(),
            Harness::Shell,
            2,
            2048,
            vec!["git".into()],
            tx,
        );
        let ScanMessage::VmsChanged { note, level } = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker did not report")
        else {
            panic!("expected VmsChanged");
        };
        assert_eq!(level, ToastLevel::Info, "{note}");
        let vm = VmStore::new(&config).load("rust").unwrap();
        assert_eq!(vm.vcpus, 2);
        assert!(vm.extra_packages.contains(&"git".to_string()));

        let (tx, rx) = mpsc::channel();
        spawn_create_vm(
            config,
            "rust".into(),
            Harness::Shell,
            2,
            2048,
            Vec::new(),
            tx,
        );
        let ScanMessage::VmsChanged { level, .. } =
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected VmsChanged");
        };
        assert_eq!(level, ToastLevel::Error);
    }

    #[test]
    fn assign_and_unassign_project_round_trip_through_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        VmStore::new(&config)
            .save(&VmConfig::from_config(
                &config,
                "rust",
                Some(Harness::Shell),
            ))
            .unwrap();

        let (tx, rx) = mpsc::channel();
        spawn_assign_project(
            config.clone(),
            "demo".into(),
            project.clone(),
            "rust".into(),
            false,
            tx,
        );
        let ScanMessage::VmsChanged { level, note } =
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected VmsChanged");
        };
        assert_eq!(level, ToastLevel::Info, "{note}");
        assert_eq!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("demo")
                .unwrap()
                .vm
                .as_deref(),
            Some("rust")
        );

        let (tx, rx) = mpsc::channel();
        spawn_unassign_project(config.clone(), "demo".into(), tx);
        let ScanMessage::VmsChanged { level, note } =
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected VmsChanged");
        };
        assert_eq!(level, ToastLevel::Info, "{note}");
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("demo")
                .unwrap()
                .vm
                .is_none()
        );
    }

    #[test]
    fn delete_vm_never_deletes_external_projects() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let registry = VmRegistry::new(config.clone());
        registry
            .ensure_for_project(&project, Harness::Shell)
            .unwrap();

        let (tx, rx) = mpsc::channel();
        spawn_delete_vm(config.clone(), "demo".into(), true, tx);
        let ScanMessage::VmsChanged { level, note } =
            rx.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("expected VmsChanged");
        };
        assert_eq!(level, ToastLevel::Info, "{note}");
        assert!(project.is_dir(), "external projects must survive");
        assert!(!config.vms_dir.join("demo").exists());
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("demo")
                .unwrap()
                .vm
                .is_none()
        );
    }
}
