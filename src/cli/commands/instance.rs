//! Legacy `hegn instance` migration error plus the project-first `open`,
//! `attach` and `launch` handlers.
//!
//! `hegn instance` is kept in the clap tree only so existing scripts fail with
//! a readable mapping to `hegn vm`/`hegn project` instead of an unknown-command
//! usage error. No legacy lifecycle behaviour remains.

use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::agent::launch::{self, Integration, LaunchAction};
use crate::agent::provision::ProvisionPlan;
use crate::config::Config;
use crate::project;
use crate::vm::config::VmConfig;
use crate::vm::registry::VmRegistry;

use super::config::run_editor;
use super::vm::{
    guest_cwd_command, guest_path_for, only_running_vm, primary_guest_path, resolve_project_entry,
    vm_for_entry, wait_for_provision,
};
use crate::cli::args::LegacyMappingCommands;

/// Withdrawn v1 command group. Any invocation exits non-zero after printing
/// the v2 equivalents so a stale script can be fixed mechanically.
pub(crate) fn legacy_mapping_command(
    _command: Option<LegacyMappingCommands>,
    _config: Config,
    _json: bool,
) -> Result<i32> {
    bail!("{}", legacy_mapping_message())
}

pub(crate) fn legacy_mapping_message() -> &'static str {
    "`hegn instance` was replaced by `hegn vm` and `hegn project`:\n\
     \n\
       hegn instance list|status      -> hegn vm list / hegn vm show <vm>\n\
       hegn instance create           -> hegn vm create <vm>; hegn project assign <project> --vm <vm>\n\
       hegn instance clone            -> hegn project fork <project> --name <name>\n\
       hegn instance start            -> hegn vm start <vm>\n\
       hegn instance stop             -> hegn vm stop <vm>\n\
       hegn instance pause|resume     -> hegn vm restart <vm>\n\
       hegn instance config           -> hegn vm config <vm>\n\
       hegn instance delete           -> hegn vm delete <vm>\n\
     \n\
     Membership is now project-first: assign projects to a VM, then start it."
}

pub(crate) fn snapshot_vm(registry: &VmRegistry, requested: Option<String>) -> Result<VmConfig> {
    match requested {
        Some(selector) => registry.resolve(&selector),
        None => only_running_vm(registry),
    }
}

/// Decide whether `hegn open` creates a session branch.
///
/// `auto_branch` comes from the configuration; `--no-branch` is the only
/// force-off flag. There is no `--branch` flag, so a VM configured with
/// `auto_branch = false` never creates one.
fn should_create_session_branch(auto_branch: bool, no_branch: bool) -> bool {
    auto_branch && !no_branch
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
    let entry = resolve_project_entry(&config, &project)?;
    if editor {
        return run_editor(&entry.path);
    }
    if lazygit {
        let status = Command::new("lazygit")
            .current_dir(&entry.path)
            .status()
            .context("failed to launch lazygit (is it installed?)")?;
        return Ok(status.code().unwrap_or(1));
    }

    let vm = vm_for_entry(&config, &entry)?;
    let guest_path = guest_path_for(&config, &vm, &entry.name)?;
    let registry = VmRegistry::new(config.clone());
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

    // Only mutate the workspace repository once a VM is actually available.
    if should_create_session_branch(config.auto_branch, no_branch) {
        let git = project::read_git_summary(&entry.path);
        if git.is_repo {
            match project::create_session_branch(&entry.path) {
                Ok(branch) => println!("Session branch: {branch}"),
                Err(err) => eprintln!("warning: could not create session branch: {err}"),
            }
        }
    }

    println!("Guest path: {guest_path}");
    let typed = command
        .as_deref()
        .map(|command| guest_cwd_command(&guest_path, Some(command)));
    let console = manager.attach_console()?;
    crate::session::console::run_interactive_with_command(console, !no_bar, typed.as_deref())?;
    Ok(0)
}

pub(crate) fn attach(
    selector: Option<String>,
    no_bar: bool,
    command: Option<String>,
    config: Config,
) -> Result<i32> {
    let registry = VmRegistry::new(config.clone());
    let vm = match selector {
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
    let (vm, entry) = match selector {
        Some(selector) => match registry.store().load(&selector) {
            Ok(vm) => (vm, None),
            Err(_) => {
                let entry = resolve_project_entry(&config, &selector)?;
                let vm = vm_for_entry(&config, &entry)?;
                (vm, Some(entry))
            }
        },
        None => (only_running_vm(&registry)?, None),
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
    if let Some(warning) = manager.membership_warning() {
        eprintln!("warning: {warning}");
    }

    let guest_path = match &entry {
        Some(entry) => Some(guest_path_for(&config, &vm, &entry.name)?),
        None => primary_guest_path(&config, &vm)?,
    };
    let host_path = entry
        .as_ref()
        .map(|entry| entry.path.clone())
        .or_else(|| {
            let summary = registry.summary(vm.clone());
            (!summary.project.as_os_str().is_empty()).then_some(summary.project)
        })
        .unwrap_or_default();
    let meta = project::read_meta(&host_path);
    let mut spec = launch::resolve(
        &config,
        vm.harness,
        command.as_deref().or(meta.launch.as_deref()),
        integration.or(meta.integration),
    )?;
    if no_bar {
        spec.bar = false;
    }
    if let Some(guest_path) = &guest_path {
        let original = (!spec.is_shell()).then_some(spec.command.as_str());
        spec.command = guest_cwd_command(guest_path, original);
    }
    let hegn = std::env::current_exe().context("failed to locate the hegn executable")?;
    match launch::action(&spec, &vm.name, &host_path, &hegn)? {
        LaunchAction::Embedded { command } => {
            let console = manager.attach_console()?;
            crate::session::console::run_interactive_with_command(
                console,
                spec.bar,
                Some(&command),
            )?;
        }
        LaunchAction::External { vm, command, bar } => {
            let console = registry.manager_by_name(&vm)?.attach_console()?;
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
    use crate::cli::args::LegacyMappingCommands;
    use crate::cli::test_support::config;

    #[test]
    fn every_legacy_mapping_invocation_is_a_migration_error() {
        let (_dir, config) = config();
        for command in [
            None,
            Some(LegacyMappingCommands::List),
            Some(LegacyMappingCommands::Create {
                project: "demo".into(),
                name: None,
                harness: None,
                clone: false,
            }),
            Some(LegacyMappingCommands::Clone {
                source: "demo".into(),
                name: None,
                count: 1,
                harness: None,
                no_start: true,
            }),
        ] {
            let err = legacy_mapping_command(command, config.clone(), true)
                .unwrap_err()
                .to_string();
            assert!(err.contains("hegn vm"), "{err}");
            assert!(err.contains("hegn project"), "{err}");
        }
    }

    #[test]
    fn migration_message_names_the_project_first_equivalents() {
        let message = legacy_mapping_message();
        assert!(message.contains("hegn vm start"), "{message}");
        assert!(message.contains("hegn project assign"), "{message}");
        assert!(message.contains("hegn project fork"), "{message}");
    }

    #[test]
    fn attach_without_a_session_is_a_hard_error() {
        let (_dir, config) = config();
        let err = attach(None, false, None, config).unwrap_err().to_string();
        assert!(err.contains("no running VM"), "{err}");
    }

    #[test]
    fn auto_branch_and_no_branch_decide_the_session_branch() {
        assert!(should_create_session_branch(true, false));
        assert!(
            !should_create_session_branch(false, false),
            "auto_branch = false disables the branch"
        );
        assert!(
            !should_create_session_branch(true, true),
            "--no-branch always forces off"
        );
        assert!(!should_create_session_branch(false, true));
    }
}
