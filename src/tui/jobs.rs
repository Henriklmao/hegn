//! Background workers for project scans, images, VM lifecycle and provisioning.
//!
//! Every `spawn_*` function moves blocking host work onto a named thread and
//! reports back over a [`ScanMessage`] channel. Keep the thread boundaries here:
//! the event loop must stay responsive while a scan, download, clone or VM start
//! is in flight, and a worker failure must never take the TUI down with it.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::agent::provision::{ProvisionEvent, ProvisionPlan, ProvisionWatcher};
use crate::config::{Config, Harness};
use crate::project;
use crate::tui::app::ToastLevel;
use crate::tui::messages::ScanMessage;

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

pub(super) fn spawn_attach(config: Config, instance: String, tx: mpsc::Sender<ScanMessage>) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-attach-vm".into())
        .spawn(move || {
            let registry = crate::vm::instance::InstanceRegistry::new(config);
            let harness = registry
                .store()
                .load(&instance)
                .map(|config| config.harness)
                .unwrap_or_default();
            let result = registry
                .manager_by_name(&instance)
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

pub(super) fn spawn_clones(
    config: Config,
    source: String,
    count: usize,
    harness: Harness,
    start: bool,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-clone-agents".into())
        .spawn(move || {
            let registry = crate::vm::instance::InstanceRegistry::new(config.clone());
            let source = match registry.resolve(&source) {
                Ok(source) => source,
                Err(err) => {
                    // Clone failures must not tear down the attached VM.
                    let _ = thread_tx.send(ScanMessage::InstanceError(format!(
                        "could not create clones: {err}"
                    )));
                    return;
                }
            };
            let mut created = 0;
            let mut failures = Vec::new();
            let mut warnings = Vec::new();
            for index in 1..=count {
                let base = format!("{}-agent", source.name);
                let name = match registry.unique_name(&format!("{base}-{index}")) {
                    Ok(name) => name,
                    Err(err) => {
                        failures.push(err.to_string());
                        continue;
                    }
                };
                match registry.clone_from(&source, &name, Some(harness)) {
                    Ok(outcome) => {
                        for warning in &outcome.warnings {
                            if !warnings.iter().any(|existing| existing == warning) {
                                warnings.push(warning.clone());
                            }
                        }
                        if start
                            && let Err(err) = registry.manager(outcome.instance.clone()).start()
                        {
                            failures.push(format!("{name}: {err}"));
                        }
                        created += 1;
                    }
                    Err(err) => failures.push(format!("{name}: {err}")),
                }
            }
            let mut note = if failures.is_empty() {
                format!("created {created} clone(s) for parallel agents")
            } else {
                format!(
                    "created {created} clone(s); {} failed: {}",
                    failures.len(),
                    failures.join("; ")
                )
            };
            if !warnings.is_empty() {
                note.push_str("; warning: ");
                note.push_str(&warnings.join("; "));
            }
            let level = if failures.is_empty() && warnings.is_empty() {
                ToastLevel::Info
            } else {
                ToastLevel::Warn
            };
            let _ = thread_tx.send(ScanMessage::InstancesChanged { note, level });
        });
    if let Err(err) = spawned {
        let _ = tx.send(ScanMessage::InstanceError(format!(
            "could not start the clone thread: {err}"
        )));
    }
}

pub(super) fn spawn_start(
    config: Config,
    project: std::path::PathBuf,
    harness: Harness,
    tx: mpsc::Sender<ScanMessage>,
) {
    let thread_tx = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hegn-start-vm".into())
        .spawn(move || {
            let registry = crate::vm::instance::InstanceRegistry::new(config.clone());
            let instance = match registry.ensure_for_project(&project, harness) {
                Ok(instance) => instance,
                Err(err) => {
                    let _ = thread_tx.send(ScanMessage::StartFailed(err.to_string()));
                    return;
                }
            };
            let workspace = instance.workspace.clone();
            let auto_branch = instance.auto_branch;
            let harness = instance.harness;
            let plan = ProvisionPlan::new(instance.harness, &instance.extra_packages).ok();
            let manager = registry.manager(instance);
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
            if let (Some(report), Some(plan)) = (&started, plan)
                && let Some(log) = &report.serial_log
            {
                spawn_provision_watch(log.clone(), plan, thread_tx.clone());
            }
            match manager.attach_console() {
                Ok(console) => {
                    let branch = if auto_branch && project::read_git_summary(&workspace).is_repo {
                        Some(
                            project::create_session_branch(&workspace)
                                .map_err(|err| err.to_string()),
                        )
                    } else {
                        None
                    };
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

/// Follow the session log while cloud-init installs the harness preset.
///
/// The watcher never owns the console socket; it only reads the relay's log, so
/// the user can attach immediately and still see the progress notification.
fn spawn_provision_watch(
    log: std::path::PathBuf,
    plan: ProvisionPlan,
    tx: mpsc::Sender<ScanMessage>,
) {
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

    #[test]
    fn vm_start_runs_off_the_ui_thread_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.projects_root = dir.path().join("projects");
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.templates_dir = dir.path().join("templates");
        config.instances_dir = dir.path().join("instances");
        config.profiles_dir = dir.path().join("profiles");
        config.kernel_image = dir.path().join("missing-kernel");
        config.rootfs_image = dir.path().join("missing-rootfs");
        std::fs::create_dir_all(&config.projects_root).unwrap();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn_start(config, project, Harness::Shell, tx);
        let message = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker did not report a result");
        assert!(matches!(message, ScanMessage::StartFailed(_)));
    }
}
