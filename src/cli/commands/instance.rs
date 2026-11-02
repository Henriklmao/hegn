//! `hegn instance`, `open`, `attach` and `launch` handlers, plus the helpers
//! they share for picking the only running instance and drawing provisioning
//! progress.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::agent::launch::{self, Integration, LaunchAction};
use crate::agent::provision::{ProvisionPlan, ProvisionProgress, ProvisionWatcher};
use crate::config::Config;
use crate::project;
use crate::vm::instance::InstanceRegistry;

use super::config::run_editor;
use crate::cli::args::InstanceCommands;
use crate::cli::output::{print_instance, print_instances, print_report, runtime};
use crate::cli::resolve::resolve_project;

pub(crate) fn instance_command(
    command: InstanceCommands,
    config: Config,
    json: bool,
) -> Result<i32> {
    let registry = InstanceRegistry::new(config.clone());
    match command {
        InstanceCommands::List => {
            print_instances(&registry.summaries()?, json)?;
            Ok(0)
        }
        InstanceCommands::Create {
            project,
            name,
            harness,
            clone,
        } => {
            let path = resolve_project(&config, &project)?;
            let harness = harness.or_else(|| project::read_meta(&path).harness);
            let instance = if clone {
                let outcome =
                    registry.create_with_warnings(&path, name.as_deref(), harness, true)?;
                for warning in &outcome.warnings {
                    eprintln!("warning: {warning}");
                }
                outcome.instance
            } else {
                registry.create(&path, name.as_deref(), harness, false)?
            };
            print_instance(&registry.summary(&instance), json)?;
            Ok(0)
        }
        InstanceCommands::Clone {
            source,
            name,
            count,
            harness,
            no_start,
        } => {
            if count == 0 || count > 64 {
                bail!("--count must be between 1 and 64");
            }
            let source = registry.resolve(&source)?;
            let base = name.unwrap_or_else(|| format!("{}-agent", source.name));
            let mut created = Vec::new();
            for index in 1..=count {
                let clone_name = if count == 1 {
                    registry.unique_name(&base)?
                } else {
                    registry.unique_name(&format!("{base}-{index}"))?
                };
                let outcome = registry.clone_from(&source, &clone_name, harness)?;
                for warning in &outcome.warnings {
                    eprintln!("warning: {warning}");
                }
                if !no_start {
                    registry.manager(outcome.instance.clone()).start()?;
                }
                created.push(registry.summary(&outcome.instance));
            }
            print_instances(&created, json)?;
            Ok(0)
        }
        InstanceCommands::Start {
            name,
            harness,
            clone,
        } => {
            let instance = match registry.resolve(&name) {
                Ok(instance) => {
                    if clone {
                        let clone_name =
                            registry.unique_name(&format!("{}-agent", instance.name))?;
                        let outcome = registry.clone_from(&instance, &clone_name, harness)?;
                        for warning in &outcome.warnings {
                            eprintln!("warning: {warning}");
                        }
                        outcome.instance
                    } else {
                        instance
                    }
                }
                Err(_) => {
                    let path = resolve_project(&config, &name)?;
                    let meta = project::read_meta(&path);
                    let harness = harness.or(meta.harness).unwrap_or(config.harness);
                    if clone {
                        let outcome =
                            registry.create_with_warnings(&path, None, Some(harness), true)?;
                        for warning in &outcome.warnings {
                            eprintln!("warning: {warning}");
                        }
                        outcome.instance
                    } else {
                        registry.ensure_for_project(&path, harness)?
                    }
                }
            };
            let manager = registry.manager(instance);
            let report = manager.start()?;
            print_report(&report, json)?;
            Ok(0)
        }
        InstanceCommands::Stop { name } => {
            let instance = match name {
                Some(selector) => registry.resolve(&selector)?,
                None => only_running_instance(&registry)?,
            };
            registry.manager(instance.clone()).stop()?;
            if json {
                println!("{}", serde_json::json!({"stopped": instance.name}));
            } else {
                println!("Instance '{}' stopped.", instance.name);
            }
            Ok(0)
        }
        InstanceCommands::Status { name } => {
            match name {
                Some(selector) => {
                    let instance = registry.resolve(&selector)?;
                    let report = registry.manager(instance).status()?;
                    print_report(&report, json)?;
                }
                None => print_instances(&registry.summaries()?, json)?,
            }
            Ok(0)
        }
        InstanceCommands::Pause { name } => {
            let instance = registry.resolve(&name)?;
            let instance_name = instance.name.clone();
            runtime()?.block_on(registry.manager(instance).pause())?;
            if json {
                println!("{}", serde_json::json!({"paused": instance_name}));
            } else {
                println!("Instance '{instance_name}' paused.");
            }
            Ok(0)
        }
        InstanceCommands::Resume { name } => {
            let instance = registry.resolve(&name)?;
            let instance_name = instance.name.clone();
            runtime()?.block_on(registry.manager(instance).resume())?;
            if json {
                println!("{}", serde_json::json!({"resumed": instance_name}));
            } else {
                println!("Instance '{instance_name}' resumed.");
            }
            Ok(0)
        }
        InstanceCommands::Config { name, edit, path } => {
            let instance = registry.store().load(&name)?;
            let config_path = registry.store().config_path(&name)?;
            if path {
                println!("{}", config_path.display());
            } else if edit {
                run_editor(&config_path)?;
            } else if json {
                println!("{}", serde_json::to_string_pretty(&instance)?);
            } else {
                println!("{}", toml::to_string_pretty(&instance)?);
            }
            Ok(0)
        }
        InstanceCommands::Delete {
            name,
            delete_files,
            keep_files,
            yes,
        } => {
            let instance = registry.store().load(&name)?;
            let workspace_inside = instance
                .workspace
                .canonicalize()
                .map(|workspace| {
                    instance
                        .dir(registry.store().root())
                        .canonicalize()
                        .map(|dir| workspace.starts_with(&dir))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if !yes && !prompt_yes(&format!("Delete instance '{}'?", instance.name))? {
                println!("Cancelled.");
                return Ok(0);
            }
            let delete_files = if workspace_inside {
                if delete_files {
                    true
                } else if keep_files {
                    false
                } else {
                    if !std::io::stdin().is_terminal() {
                        bail!("workspace files require an explicit --delete-files or --keep-files");
                    }
                    prompt_yes("Also delete the cloned workspace files?")?
                }
            } else {
                false
            };
            let outcome = registry.delete(&instance.name, delete_files)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "deleted": instance.name,
                        "delete_files": delete_files,
                        "kept_workspace": outcome.kept_workspace,
                    })
                );
            } else if let Some(kept) = outcome.kept_workspace {
                println!(
                    "Instance '{}' removed. Workspace kept at {}",
                    instance.name,
                    kept.display()
                );
            } else if workspace_inside && delete_files {
                println!("Instance '{}' and its workspace deleted.", instance.name);
            } else {
                println!(
                    "Instance '{}' removed. Source project {} was not touched.",
                    instance.name,
                    instance.project.display()
                );
            }
            Ok(0)
        }
    }
}

pub(crate) fn only_running_instance(
    registry: &InstanceRegistry,
) -> Result<crate::vm::instance_config::InstanceConfig> {
    let mut running: Vec<_> = registry
        .summaries()?
        .into_iter()
        .filter(|instance| instance.state == "running")
        .collect();
    match running.len() {
        0 => bail!("no running instance; start one with `hegn instance start <name>`"),
        1 => registry.store().load(&running.remove(0).name),
        _ => {
            let names: Vec<String> = running.into_iter().map(|instance| instance.name).collect();
            bail!(
                "multiple instances are running ({}); pass an instance name",
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

pub(crate) fn snapshot_instance(
    registry: &InstanceRegistry,
    requested: Option<String>,
) -> Result<crate::vm::instance_config::InstanceConfig> {
    match requested {
        Some(selector) => registry.resolve(&selector),
        None => only_running_instance(registry),
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
    let registry = InstanceRegistry::new(config.clone());
    let instance = registry.ensure_for_project(&path, harness)?;
    let plan = ProvisionPlan::new(instance.harness, &instance.extra_packages).ok();
    let manager = registry.manager(instance.clone());
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
    let console = manager.attach_console()?;

    // Only mutate the workspace repository once a VM is actually available.
    let workspace = instance.workspace.clone();
    let git = project::read_git_summary(&workspace);
    if instance.auto_branch && git.is_repo && !no_branch {
        match project::create_session_branch(&workspace) {
            Ok(branch) => println!("Session branch: {branch}"),
            Err(err) => eprintln!("warning: could not create session branch: {err}"),
        }
    }

    crate::session::console::run_interactive_with_command(console, !no_bar, command.as_deref())?;
    Ok(0)
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
    let registry = InstanceRegistry::new(config.clone());
    let instance = match project {
        Some(selector) => registry.resolve(&selector)?,
        None => only_running_instance(&registry)?,
    };
    // VmConsole::attach re-verifies the recorded pid, VM binary and relay.
    let console = registry.manager(instance).attach_console()?;
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
    let registry = InstanceRegistry::new(config.clone());
    let instance = match selector {
        Some(selector) => match registry.resolve(&selector) {
            Ok(instance) => instance,
            Err(_) => {
                let path = resolve_project(&config, &selector)?;
                let meta = project::read_meta(&path);
                let harness = meta.harness.unwrap_or(config.harness);
                registry.ensure_for_project(&path, harness)?
            }
        },
        None => only_running_instance(&registry)?,
    };

    let manager = registry.manager(instance.clone());
    let started_now = !manager.is_running();
    let report = if started_now {
        Some(manager.start()?)
    } else {
        manager.status().ok()
    };
    if config.cloud_init {
        let plan = ProvisionPlan::new(instance.harness, &instance.extra_packages).ok();
        if let (Some(report), Some(plan)) = (&report, plan) {
            wait_for_provision(report.serial_log.as_deref(), &plan);
        }
    }

    let meta = project::read_meta(&instance.project);
    let mut spec = launch::resolve(
        &config,
        instance.harness,
        command.as_deref().or(meta.launch.as_deref()),
        integration.or(meta.integration),
    )?;
    if no_bar {
        spec.bar = false;
    }
    let hegn = std::env::current_exe().context("failed to locate the hegn executable")?;
    match launch::action(&spec, &instance.name, &instance.workspace, &hegn)? {
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
    fn instance_status_json_works_without_vm() {
        let (_dir, config) = config();
        let code = instance_command(InstanceCommands::Status { name: None }, config, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn instance_create_clone_list_config_and_delete() {
        let (_dir, config) = config();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"demo").unwrap();

        let code = instance_command(
            InstanceCommands::Create {
                project: project.display().to_string(),
                name: Some("base".into()),
                harness: Some(Harness::Codex),
                clone: false,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(config.instances_dir.join("base/hegn.toml").is_file());

        let code = instance_command(
            InstanceCommands::Clone {
                source: "base".into(),
                name: Some("agent1".into()),
                count: 1,
                harness: None,
                no_start: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(config.instances_dir.join("agent1/hegn.toml").is_file());
        assert!(
            config
                .instances_dir
                .join("agent1/workspace/README.md")
                .is_file()
        );

        assert_eq!(
            instance_command(InstanceCommands::List, config.clone(), true).unwrap(),
            0
        );
        assert_eq!(
            instance_command(
                InstanceCommands::Config {
                    name: "base".into(),
                    edit: false,
                    path: true,
                },
                config.clone(),
                true,
            )
            .unwrap(),
            0
        );
        assert_eq!(
            instance_command(
                InstanceCommands::Delete {
                    name: "agent1".into(),
                    delete_files: true,
                    keep_files: false,
                    yes: true,
                },
                config.clone(),
                true,
            )
            .unwrap(),
            0
        );
        assert!(!config.instances_dir.join("agent1").exists());
        assert!(project.join("README.md").is_file());
    }

    #[test]
    fn attach_without_a_session_is_a_hard_error() {
        let (_dir, config) = config();
        let err = attach(None, false, None, config).unwrap_err().to_string();
        assert!(err.contains("no running instance"), "{err}");
    }
}
