//! `hegn instance`, `open`, `attach` and `launch` handlers, plus the helpers
//! they share for picking the only running VM and drawing provisioning
//! progress.
//!
//! Phase 2 keeps the legacy `instance` command as a mechanical adapter over
//! [`VmRegistry`]. The project-first `hegn vm`/`hegn project` surface lands in
//! Phase 5.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::agent::launch::{self, Integration, LaunchAction};
use crate::agent::provision::{ProvisionPlan, ProvisionProgress, ProvisionWatcher};
use crate::config::Config;
use crate::project;
use crate::vm::config::VmConfig;
use crate::vm::registry::VmRegistry;

use super::config::run_editor;
use crate::cli::args::InstanceCommands;
use crate::cli::output::{print_instances, print_report, runtime};
use crate::cli::resolve::resolve_project;

pub(crate) fn instance_command(
    command: InstanceCommands,
    config: Config,
    json: bool,
) -> Result<i32> {
    let registry = VmRegistry::new(config.clone());
    match command {
        InstanceCommands::List => {
            print_instances(&registry.summaries()?, json)?;
            Ok(0)
        }
        InstanceCommands::Create { .. } => bail!(
            "`hegn instance create` was replaced by `hegn vm create <name>` plus \
             `hegn project assign <project> --vm <name>`"
        ),
        InstanceCommands::Clone { .. } => bail!(
            "`hegn instance clone` was replaced by `hegn project fork <project>` \
             (workspaces land in Phase 7; use a second VM until then)"
        ),
        InstanceCommands::Start {
            name,
            harness,
            clone,
        } => {
            if clone {
                bail!(
                    "`--clone` was replaced by `hegn project fork` and is not available yet; \
                     start the VM without it"
                );
            }
            let vm = match registry.resolve(&name) {
                Ok(vm) => vm,
                Err(_) => {
                    let path = resolve_project(&config, &name)?;
                    let meta = project::read_meta(&path);
                    let harness = harness.or(meta.harness).unwrap_or(config.harness);
                    registry.ensure_for_project(&path, harness)?
                }
            };
            let manager = registry.manager(vm)?;
            let report = manager.start()?;
            print_report(&report, json)?;
            Ok(0)
        }
        InstanceCommands::Stop { name } => {
            let vm = match name {
                Some(selector) => registry.resolve(&selector)?,
                None => only_running_vm(&registry)?,
            };
            registry.manager(vm.clone())?.stop()?;
            if json {
                println!("{}", serde_json::json!({"stopped": vm.name}));
            } else {
                println!("VM '{}' stopped.", vm.name);
            }
            Ok(0)
        }
        InstanceCommands::Status { name } => {
            match name {
                Some(selector) => {
                    let vm = registry.resolve(&selector)?;
                    let report = registry.manager(vm)?.status()?;
                    print_report(&report, json)?;
                }
                None => print_instances(&registry.summaries()?, json)?,
            }
            Ok(0)
        }
        InstanceCommands::Pause { name } => {
            let vm = registry.resolve(&name)?;
            let vm_name = vm.name.clone();
            runtime()?.block_on(registry.manager(vm)?.pause())?;
            if json {
                println!("{}", serde_json::json!({"paused": vm_name}));
            } else {
                println!("VM '{vm_name}' paused.");
            }
            Ok(0)
        }
        InstanceCommands::Resume { name } => {
            let vm = registry.resolve(&name)?;
            let vm_name = vm.name.clone();
            runtime()?.block_on(registry.manager(vm)?.resume())?;
            if json {
                println!("{}", serde_json::json!({"resumed": vm_name}));
            } else {
                println!("VM '{vm_name}' resumed.");
            }
            Ok(0)
        }
        InstanceCommands::Config { name, edit, path } => {
            let vm = registry.store().load(&name)?;
            let config_path = registry.store().config_path(&name)?;
            if path {
                println!("{}", config_path.display());
            } else if edit {
                run_editor(&config_path)?;
            } else if json {
                println!("{}", serde_json::to_string_pretty(&vm)?);
            } else {
                println!("{}", toml::to_string_pretty(&vm)?);
            }
            Ok(0)
        }
        InstanceCommands::Delete {
            name,
            delete_files,
            keep_files,
            yes,
        } => {
            let vm = registry.store().load(&name)?;
            if !yes && !prompt_yes(&format!("Delete VM '{}'?", vm.name))? {
                println!("Cancelled.");
                return Ok(0);
            }
            let delete_files = if delete_files {
                true
            } else if keep_files {
                false
            } else if std::io::stdin().is_terminal() {
                prompt_yes("Also delete hegn-managed workspace files?")?
            } else {
                bail!("choose --delete-files or --keep-files in non-interactive mode");
            };
            let outcome = registry.delete(&vm.name, delete_files)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "deleted": vm.name,
                        "delete_files": delete_files,
                        "kept_workspace": outcome.kept_workspace,
                    })
                );
            } else if let Some(kept) = outcome.kept_workspace {
                println!(
                    "VM '{}' removed. Workspaces kept at {}",
                    vm.name,
                    kept.display()
                );
            } else if delete_files {
                println!("VM '{}' and its managed workspaces deleted.", vm.name);
            } else {
                println!("VM '{}' removed. Project files were not touched.", vm.name);
            }
            Ok(0)
        }
    }
}

pub(crate) fn only_running_vm(registry: &VmRegistry) -> Result<VmConfig> {
    let mut running: Vec<_> = registry
        .summaries()?
        .into_iter()
        .filter(|vm| vm.state == "running")
        .collect();
    match running.len() {
        0 => bail!("no running VM; start one with `hegn instance start <name>`"),
        1 => registry.store().load(&running.remove(0).name),
        _ => {
            let names: Vec<String> = running.into_iter().map(|vm| vm.name).collect();
            bail!(
                "multiple VMs are running ({}); pass a VM name",
                names.join(", ")
            )
        }
    }
}

pub(crate) fn prompt_yes(question: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        bail!("confirmation required; pass --yes and a file policy flag in non-interactive mode");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub(crate) fn snapshot_vm(registry: &VmRegistry, requested: Option<String>) -> Result<VmConfig> {
    match requested {
        Some(selector) => registry.resolve(&selector),
        None => only_running_vm(registry),
    }
}

pub(crate) fn open(
    project: String,
    editor: bool,
    lazygit: bool,
    no_branch: bool,
    no_bar: bool,
    command: Option<String>,
    config: Config,
) -> Result<i32> {
    let path = resolve_project(&config, &project)?;
    if editor {
        return run_editor(&path);
    }
    if lazygit {
        let status = Command::new("lazygit")
            .current_dir(&path)
            .status()
            .context("failed to launch lazygit (is it installed?)")?;
        return Ok(status.code().unwrap_or(1));
    }

    let meta = project::read_meta(&path);
    let harness = meta.harness.unwrap_or(config.harness);
    let registry = VmRegistry::new(config.clone());
    let vm = registry.ensure_for_project(&path, harness)?;
    let plan = ProvisionPlan::new(vm.harness, &vm.extra_packages).ok();
    let manager = registry.manager(vm.clone())?;
    let report = if manager.is_running() {
        manager.status().ok()
    } else {
        Some(manager.start()?)
    };
    if config.cloud_init
        && let (Some(report), Some(plan)) = (&report, plan)
    {
        wait_for_provision(report.serial_log.as_deref(), &plan);
    }
    if let Some(warning) = manager.membership_warning() {
        eprintln!("warning: {warning}");
    }
    let console = manager.attach_console()?;

    // Only mutate the workspace repository once a VM is actually available.
    let workspace = primary_path(&registry, &vm);
    if !no_branch && let Some(workspace) = workspace {
        let git = project::read_git_summary(&workspace);
        if git.is_repo {
            match project::create_session_branch(&workspace) {
                Ok(branch) => println!("Session branch: {branch}"),
                Err(err) => eprintln!("warning: could not create session branch: {err}"),
            }
        }
    }

    crate::session::console::run_interactive_with_command(console, !no_bar, command.as_deref())?;
    Ok(0)
}

/// Primary (or first) exported project of a VM.
fn primary_path(registry: &VmRegistry, vm: &VmConfig) -> Option<std::path::PathBuf> {
    let projects = crate::project::ProjectRegistry::load(&registry.config().state_dir).ok()?;
    let members = projects.exported_members(&vm.name);
    members
        .iter()
        .find(|entry| vm.primary.as_deref() == Some(entry.name.as_str()))
        .or_else(|| members.first())
        .map(|entry| entry.path.clone())
}

/// Wait for the guest provisioner while showing a rough progress bar.
///
/// Pre-provisioned images never emit progress, so the wait is bounded: after a
/// short grace period `hegn open` attaches normally.
fn wait_for_provision(log: Option<&Path>, plan: &ProvisionPlan) {
    let Some(log) = log else {
        return;
    };
    let Ok(mut watcher) = ProvisionWatcher::open(log, plan) else {
        return;
    };
    let tty = std::io::stderr().is_terminal();
    let started = Instant::now();
    let mut last_update = started;
    let mut last_label = String::new();
    loop {
        match watcher.poll() {
            Ok(events) if !events.is_empty() => last_update = Instant::now(),
            Ok(_) => {}
            Err(_) => break,
        }
        if watcher.saw_progress() {
            let progress = watcher.progress();
            if tty {
                draw_cli_provision(progress);
            } else {
                let label = progress.stage_label();
                if label != last_label {
                    eprintln!("hegn: {label}");
                    last_label = label.to_string();
                }
            }
        }
        if watcher.finished() {
            break;
        }
        if !watcher.saw_progress() && started.elapsed() > Duration::from_secs(20) {
            break;
        }
        // A guest that stops reporting for several minutes is better watched
        // through the console than through a frozen bar.
        if watcher.saw_progress() && last_update.elapsed() > Duration::from_secs(5 * 60) {
            break;
        }
        if started.elapsed() > Duration::from_secs(15 * 60) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if tty && watcher.saw_progress() {
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }
}

fn draw_cli_provision(progress: &ProvisionProgress) {
    const WIDTH: usize = 28;
    let filled = (progress.fraction() * WIDTH as f64).round() as usize;
    let bar = format!(
        "{}{}",
        "█".repeat(filled.min(WIDTH)),
        "░".repeat(WIDTH.saturating_sub(filled))
    );
    eprint!(
        "\r\x1b[2K setting up {}  [{bar}]  {}  ",
        progress.harness_label(),
        progress.eta_label()
    );
    let _ = std::io::stderr().flush();
}

pub(crate) fn attach(
    project: Option<String>,
    no_bar: bool,
    command: Option<String>,
    config: Config,
) -> Result<i32> {
    let registry = VmRegistry::new(config.clone());
    let vm = match project {
        Some(selector) => registry.resolve(&selector)?,
        None => only_running_vm(&registry)?,
    };
    // VmConsole::attach re-verifies the recorded pid, VM binary and relay.
    let manager = registry.manager(vm)?;
    if let Some(warning) = manager.membership_warning() {
        eprintln!("warning: {warning}");
    }
    let console = manager.attach_console()?;
    crate::session::console::run_interactive_with_command(console, !no_bar, command.as_deref())?;
    Ok(0)
}

pub(crate) fn launch_command(
    selector: Option<String>,
    command: Option<String>,
    integration: Option<Integration>,
    no_bar: bool,
    config: Config,
) -> Result<i32> {
    let registry = VmRegistry::new(config.clone());
    let vm = match selector {
        Some(selector) => match registry.resolve(&selector) {
            Ok(vm) => vm,
            Err(_) => {
                let path = resolve_project(&config, &selector)?;
                let meta = project::read_meta(&path);
                let harness = meta.harness.unwrap_or(config.harness);
                registry.ensure_for_project(&path, harness)?
            }
        },
        None => only_running_vm(&registry)?,
    };

    let manager = registry.manager(vm.clone())?;
    let started_now = !manager.is_running();
    let report = if started_now {
        Some(manager.start()?)
    } else {
        manager.status().ok()
    };
    if config.cloud_init {
        let plan = ProvisionPlan::new(vm.harness, &vm.extra_packages).ok();
        if let (Some(report), Some(plan)) = (&report, plan) {
            wait_for_provision(report.serial_log.as_deref(), &plan);
        }
    }

    let workspace = primary_path(&registry, &vm);
    let meta = workspace
        .as_deref()
        .map(project::read_meta)
        .unwrap_or_default();
    let mut spec = launch::resolve(
        &config,
        vm.harness,
        command.as_deref().or(meta.launch.as_deref()),
        integration.or(meta.integration),
    )?;
    if no_bar {
        spec.bar = false;
    }
    let hegn = std::env::current_exe().context("failed to locate the hegn executable")?;
    let workspace = workspace.unwrap_or_default();
    match launch::action(&spec, &vm.name, &workspace, &hegn)? {
        LaunchAction::Embedded { command } => {
            let console = manager.attach_console()?;
            crate::session::console::run_interactive_with_command(
                console,
                spec.bar,
                Some(&command),
            )?;
        }
        LaunchAction::External {
            instance,
            command,
            bar,
        } => {
            let console = registry.manager_by_name(&instance)?.attach_console()?;
            crate::session::console::run_interactive_with_command(
                console,
                bar,
                command.as_deref(),
            )?;
        }
        LaunchAction::Spawn { program, args } => {
            let status = Command::new(&program)
                .args(&args)
                .status()
                .with_context(|| format!("failed to run {program}"))?;
            if !status.success() {
                bail!("{program} launch integration failed");
            }
        }
        LaunchAction::Herdr { pane_command, cwd } => {
            launch::spawn_herdr(&pane_command, &cwd)?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;
    use crate::config::Harness;

    #[test]
    fn instance_status_json_works_without_a_vm() {
        let (_dir, config) = config();
        let code = instance_command(InstanceCommands::Status { name: None }, config, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn legacy_create_and_clone_point_at_the_new_commands() {
        let (_dir, config) = config();
        let err = instance_command(
            InstanceCommands::Create {
                project: "demo".into(),
                name: None,
                harness: None,
                clone: false,
            },
            config.clone(),
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hegn vm create"), "{err}");

        let err = instance_command(
            InstanceCommands::Clone {
                source: "demo".into(),
                name: None,
                count: 1,
                harness: None,
                no_start: true,
            },
            config,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hegn project fork"), "{err}");
    }

    #[test]
    fn start_status_config_and_delete_use_the_vm_manager() {
        let (_dir, config) = config();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"demo").unwrap();

        // Create the transitional solo VM the way `open` does.
        let registry = VmRegistry::new(config.clone());
        let vm = registry
            .ensure_for_project(&project, Harness::Codex)
            .unwrap();
        assert!(config.vms_dir.join("demo/vm.toml").is_file());

        assert_eq!(
            instance_command(InstanceCommands::List, config.clone(), true).unwrap(),
            0
        );
        assert_eq!(
            instance_command(
                InstanceCommands::Config {
                    name: "demo".into(),
                    edit: false,
                    path: true,
                },
                config.clone(),
                true,
            )
            .unwrap(),
            0
        );
        // A VM with no assets cannot start, but the command must reach the
        // manager rather than the deleted instance store.
        let err = instance_command(
            InstanceCommands::Start {
                name: "demo".into(),
                harness: None,
                clone: false,
            },
            config.clone(),
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("VM log:") || err.contains("missing VM assets"),
            "{err}"
        );

        assert_eq!(
            instance_command(
                InstanceCommands::Delete {
                    name: vm.name,
                    delete_files: false,
                    keep_files: true,
                    yes: true,
                },
                config.clone(),
                true,
            )
            .unwrap(),
            0
        );
        assert!(!config.vms_dir.join("demo").exists());
        assert!(project.join("README.md").is_file());
        let _ = crate::vm::same_project(&project, &project);
    }

    #[test]
    fn attach_without_a_session_is_a_hard_error() {
        let (_dir, config) = config();
        let err = attach(None, false, None, config).unwrap_err().to_string();
        assert!(err.contains("no running VM"), "{err}");
    }
}
