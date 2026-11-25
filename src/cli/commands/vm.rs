//! `hegn vm` handlers plus the project-to-VM helpers shared by the
//! project-first CLI (`open`, `launch`, `project assign`, `logs`).
//!
//! A VM is the tool profile and runtime; membership lives in the private
//! project registry. Every command here is non-interactive unless it deletes
//! state, in which case it asks for confirmation or requires `--yes`.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::agent::launch;
use crate::agent::provision::{ProvisionPlan, ProvisionProgress, ProvisionWatcher};
use crate::config::Config;
use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
use crate::shared::text::shell_quote;
use crate::vm::config::{VmConfig, VmStore};
use crate::vm::manager::{BootPlan, VmPaths};
use crate::vm::registry::{VmRegistry, remove_private_rootfs};

use crate::cli::args::VmCommands;
use crate::cli::output::{print_report, print_vms};

/// One member of a VM as shown by `vm show`/`vm projects`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MemberView {
    pub name: String,
    pub path: PathBuf,
    /// Guest mount path, or the inherited path below the parent for a worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_path: Option<String>,
    pub read_only: bool,
    pub kind: ProjectKind,
    pub primary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Serialize)]
struct VmDetail {
    #[serde(flatten)]
    summary: crate::vm::VmSummary,
    /// Project membership list. Named `projects` because `VmSummary` already
    /// has a numeric `members` count.
    projects: Vec<MemberView>,
}

pub(crate) fn vm_command(command: VmCommands, config: Config, json: bool) -> Result<i32> {
    let registry = VmRegistry::new(config.clone());
    match command {
        VmCommands::List => {
            print_vms(&registry.summaries()?, json)?;
            Ok(0)
        }
        VmCommands::Create {
            name,
            harness,
            vcpus,
            memory,
            packages,
        } => {
            let store = VmStore::new(&config);
            if store.dir(&name)?.exists() {
                bail!("VM '{name}' already exists; choose another name");
            }
            let mut vm = VmConfig::from_config(&config, &name, harness);
            if let Some(vcpus) = vcpus {
                vm.vcpus = vcpus;
            }
            if let Some(memory) = memory {
                vm.memory_mib = memory;
            }
            for package in packages {
                if !crate::config::safe_package_name(&package) {
                    bail!("invalid package name: {package:?}");
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
            let path = store.config_path(&vm.name)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "vm": vm.name,
                        "config": path,
                        "harness": vm.harness,
                        "vcpus": vm.vcpus,
                        "memory_mib": vm.memory_mib,
                        "extra_packages": vm.extra_packages,
                    }))?
                );
            } else {
                println!("Created VM '{}'", vm.name);
                println!("  config   {}", path.display());
                println!("  harness  {}", vm.harness.label());
                println!("  resources {} vCPU · {} MiB", vm.vcpus, vm.memory_mib);
                if !vm.extra_packages.is_empty() {
                    println!("  packages {}", vm.extra_packages.join(", "));
                }
                println!("Next: hegn project assign <project> --vm {}", vm.name);
            }
            Ok(0)
        }
        VmCommands::Show { name } => {
            let vm = registry.store().load(&name)?;
            let summary = registry.summary(vm.clone());
            let members = member_views(&config, &vm)?;
            let detail = VmDetail {
                summary,
                projects: members,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&detail)?);
            } else {
                println!("vm           {}", detail.summary.name);
                println!("state        {}", detail.summary.state);
                println!("harness      {}", detail.summary.harness.label());
                println!(
                    "resources    {} vCPU · {} MiB",
                    detail.summary.vcpus, detail.summary.memory_mib
                );
                if let Some(primary) = &detail.summary.primary {
                    println!("primary      {primary}");
                }
                println!("members      {}", detail.summary.members);
                println!("config       {}", detail.summary.config_path.display());
                if let Some(log) = &detail.summary.serial_log {
                    println!("serial log   {}", log.display());
                }
                if !detail.projects.is_empty() {
                    println!();
                    print_member_table(&detail.projects);
                }
            }
            Ok(0)
        }
        VmCommands::Config { name, edit, path } => {
            let vm = registry.store().load(&name)?;
            let config_path = registry.store().config_path(&name)?;
            if path {
                println!("{}", config_path.display());
            } else if edit {
                super::config::run_editor(&config_path)?;
            } else if json {
                println!("{}", serde_json::to_string_pretty(&vm)?);
            } else {
                println!("{}", toml::to_string_pretty(&vm)?);
            }
            Ok(0)
        }
        VmCommands::Start { name } => {
            let vm = registry.store().load(&name)?;
            let report = registry.manager(vm)?.start()?;
            print_report(&report, json)?;
            Ok(0)
        }
        VmCommands::Stop { name } => {
            let vm = registry.store().load(&name)?;
            registry.manager(vm.clone())?.stop()?;
            if json {
                println!("{}", serde_json::json!({"stopped": vm.name}));
            } else {
                println!("VM '{}' stopped.", vm.name);
            }
            Ok(0)
        }
        VmCommands::Restart { name } => {
            let vm = registry.store().load(&name)?;
            let manager = registry.manager(vm)?;
            if manager.is_running() {
                manager.stop()?;
            }
            let report = manager.start()?;
            print_report(&report, json)?;
            Ok(0)
        }
        VmCommands::Apply { name } => {
            let vm = registry.store().load(&name)?;
            let report = registry.manager(vm)?.apply()?;
            print_report(&report, json)?;
            Ok(0)
        }
        VmCommands::Rebuild { name, yes } => {
            let vm = registry.store().load(&name)?;
            if let Some(image) = &vm.rootfs_image {
                bail!(
                    "VM '{name}' uses the explicit rootfs_image {}; `hegn vm rebuild` only \
                     manages a private per-VM copy",
                    image.display()
                );
            }
            if !yes
                && !confirm(
                    json,
                    &format!(
                        "Delete the private rootfs of VM '{}'? Guest-installed tools are lost.",
                        vm.name
                    ),
                    "--yes",
                )?
            {
                println!("Cancelled.");
                return Ok(0);
            }
            let manager = registry.manager(vm.clone())?;
            if manager.is_running() {
                manager.stop()?;
            }
            let removed = remove_private_rootfs(&config, &vm)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "vm": vm.name,
                        "removed_rootfs": removed,
                        "next": format!("hegn vm start {}", vm.name),
                    }))?
                );
            } else if let Some(path) = &removed {
                println!("Removed private rootfs {}.", path.display());
                println!("Next: hegn vm start {}", vm.name);
            } else {
                println!(
                    "VM '{}' has no private rootfs yet. Next: hegn vm start {}",
                    vm.name, vm.name
                );
            }
            Ok(0)
        }
        VmCommands::Primary { name, project } => {
            let mut vm = registry.store().load(&name)?;
            let registry_file = ProjectRegistry::load(&config.state_dir)?;
            let entry = registry_file
                .get(&project)
                .with_context(|| format!("unknown project '{project}'"))?;
            if entry.vm.as_deref() != Some(vm.name.as_str()) {
                bail!(
                    "project '{}' is not assigned to VM '{}'; run `hegn project assign {} --vm {}`",
                    entry.name,
                    vm.name,
                    entry.name,
                    vm.name
                );
            }
            if entry.kind == ProjectKind::Worktree {
                bail!(
                    "project '{}' is a worktree and rides its parent's share; choose an exported \
                     member as primary",
                    entry.name
                );
            }
            vm.primary = Some(entry.name.clone());
            registry.store().save(&vm)?;
            if registry.manager(vm.clone())?.is_running() {
                eprintln!(
                    "warning: VM '{}' is running; run `hegn vm apply {}` before /workspace moves \
                     to '{}'",
                    vm.name, vm.name, entry.name
                );
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({"vm": vm.name, "primary": entry.name})
                );
            } else {
                println!("VM '{}' primary is now '{}'.", vm.name, entry.name);
            }
            Ok(0)
        }
        VmCommands::Projects { name } => {
            let vm = registry.store().load(&name)?;
            let members = member_views(&config, &vm)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&members)?);
            } else if members.is_empty() {
                println!(
                    "VM '{}' has no projects. Assign one with: hegn project assign <project> --vm {}",
                    vm.name, vm.name
                );
            } else {
                print_member_table(&members);
            }
            Ok(0)
        }
        VmCommands::Shell { name, project } => {
            let vm = registry.store().load(&name)?;
            let registry_file = ProjectRegistry::load(&config.state_dir)?;
            let target = match project {
                Some(project) => project,
                None => vm
                    .primary
                    .clone()
                    .or_else(|| {
                        registry_file
                            .exported_members(&vm.name)
                            .first()
                            .map(|entry| entry.name.clone())
                    })
                    .with_context(|| {
                        format!(
                            "VM '{}' has no projects; run `hegn project assign <project> --vm {}`",
                            vm.name, vm.name
                        )
                    })?,
            };
            let guest_path = guest_path_for(&config, &vm, &target)?;
            let spec = launch::resolve(&config, vm.harness, Some(launch::HARNESS), None)?;
            let command = guest_cwd_command(
                &guest_path,
                (!spec.is_shell()).then_some(spec.command.as_str()),
            );
            let manager = registry.manager(vm.clone())?;
            let plan = ProvisionPlan::new(vm.harness, &vm.extra_packages).ok();
            let report = if manager.is_running() {
                None
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
            println!("VM '{}' · project '{}' · {guest_path}", vm.name, target);
            let console = manager.attach_console()?;
            crate::session::console::run_interactive_with_command(
                console,
                spec.bar,
                Some(&command),
            )?;
            Ok(0)
        }
        VmCommands::Delete {
            name,
            yes,
            delete_files,
        } => {
            let vm = registry.store().load(&name)?;
            if !yes
                && !confirm(
                    json,
                    &format!(
                        "Delete VM '{}'? Its projects are unassigned, never deleted.",
                        vm.name
                    ),
                    "--yes",
                )?
            {
                println!("Cancelled.");
                return Ok(0);
            }
            let (worktrees, clones) = vm_workspace_entries(&config, &vm.name)?;
            let outcome = registry.delete(&vm.name, delete_files)?;
            // `VmRegistry::delete` unassigns ordinary members. Clones and
            // worktrees carry extra invariants (a managed path or a parent),
            // so drop their registration explicitly instead of leaving a
            // dangling entry that fails the next validation.
            let removed_workspaces = worktrees.len() + clones.len();
            prune_vm_workspace_entries(&config, &worktrees, &clones)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "deleted": vm.name,
                        "delete_files": delete_files,
                        "kept_workspace": outcome.kept_workspace,
                        "workspace_entries_removed": removed_workspaces,
                    }))?
                );
            } else if !delete_files {
                println!(
                    "VM '{}' removed; its directory and workspaces were kept.",
                    vm.name
                );
                if let Some(kept) = &outcome.kept_workspace {
                    println!("Workspaces kept at {}", kept.display());
                }
            } else {
                println!("VM '{}' and its managed workspaces deleted.", vm.name);
            }
            if removed_workspaces > 0 && !json {
                println!(
                    "  note: removed {removed_workspaces} managed clone/worktree registration(s)"
                );
            }
            Ok(0)
        }
    }
}

/// Managed clone and worktree entries of a VM, in removal order.
fn vm_workspace_entries(config: &Config, vm: &str) -> Result<(Vec<String>, Vec<String>)> {
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

/// Drop clone/worktree registrations after their VM directory is gone or
/// renamed. Worktrees go first so removing a clone parent is never blocked.
fn prune_vm_workspace_entries(
    config: &Config,
    worktrees: &[String],
    clones: &[String],
) -> Result<()> {
    if worktrees.is_empty() && clones.is_empty() {
        return Ok(());
    }
    let mut registry = ProjectRegistry::load(&config.state_dir)?;
    for name in worktrees.iter().chain(clones.iter()) {
        registry.remove(name)?;
    }
    Ok(())
}

fn print_member_table(members: &[MemberView]) {
    println!("{:<24} {:<10} {:<8} GUEST", "PROJECT", "KIND", "MODE");
    for member in members {
        let mode = if member.read_only { "ro" } else { "rw" };
        let guest = member.guest_path.as_deref().unwrap_or("-");
        let marker = if member.primary { "*" } else { " " };
        println!(
            "{:<24} {:<10} {:<8} {}{}",
            member.name,
            member_kind(member.kind),
            mode,
            guest,
            if member.primary {
                format!(" {marker}")
            } else {
                String::new()
            }
        );
    }
}

fn member_kind(kind: ProjectKind) -> &'static str {
    match kind {
        ProjectKind::External => "external",
        ProjectKind::Clone => "clone",
        ProjectKind::Worktree => "worktree",
    }
}

/// Every project assigned to `vm`, including worktrees.
///
/// Exported members get their boot-plan guest path; a worktree is not exported
/// separately, so its path is the parent's guest path plus the relative path
/// inside the parent share.
pub(crate) fn member_views(config: &Config, vm: &VmConfig) -> Result<Vec<MemberView>> {
    let registry = ProjectRegistry::load(&config.state_dir)?;
    let store = VmStore::new(config);
    let exported = registry.exported_members(&vm.name);
    let mut guest_paths: HashMap<String, String> = HashMap::new();
    if !exported.is_empty() {
        let plan = BootPlan::build(config, vm, &store, &registry, &VmPaths::from_vm(config, vm))?;
        for member in plan.members {
            guest_paths.insert(member.name, member.guest_path);
        }
    }
    let effective_primary = vm
        .primary
        .clone()
        .or_else(|| exported.first().map(|entry| entry.name.clone()));
    let mut views = Vec::new();
    for entry in registry.members(&vm.name) {
        let guest_path = match entry.kind {
            ProjectKind::Worktree => entry.parent.as_deref().and_then(|parent_name| {
                let parent = registry.get(parent_name)?;
                let parent_guest = guest_paths.get(parent_name)?;
                let relative = entry.path.strip_prefix(&parent.path).ok()?;
                Some(format!("{parent_guest}/{}", relative.display()))
            }),
            ProjectKind::External | ProjectKind::Clone => guest_paths.get(&entry.name).cloned(),
        };
        views.push(MemberView {
            name: entry.name.clone(),
            path: entry.path.clone(),
            guest_path,
            read_only: entry.read_only,
            kind: entry.kind,
            primary: effective_primary.as_deref() == Some(entry.name.as_str()),
            parent: entry.parent.clone(),
        });
    }
    Ok(views)
}

/// Guest path of one member of `vm`.
pub(crate) fn guest_path_for(config: &Config, vm: &VmConfig, project: &str) -> Result<String> {
    member_views(config, vm)?
        .into_iter()
        .find(|member| member.name == project)
        .and_then(|member| member.guest_path)
        .with_context(|| {
            format!(
                "project '{project}' is not a member of VM '{}'; run `hegn project assign {project} --vm {}`",
                vm.name, vm.name
            )
        })
}

/// Guest path of the VM primary (or first exported member).
pub(crate) fn primary_guest_path(config: &Config, vm: &VmConfig) -> Result<Option<String>> {
    Ok(member_views(config, vm)?
        .into_iter()
        .find(|member| member.primary)
        .and_then(|member| member.guest_path))
}

/// Resolve a registered project by picker name, canonical path, or
/// `<projects_root>/<name>`.
///
/// An on-disk directory that is not in the registry is a hard error: v2 never
/// guesses a VM assignment from a path.
pub(crate) fn resolve_project_entry(config: &Config, selector: &str) -> Result<ProjectEntry> {
    let registry = ProjectRegistry::load(&config.state_dir)?;
    if let Some(entry) = registry.get(selector) {
        return Ok(entry.clone());
    }
    let mut candidates = vec![PathBuf::from(selector)];
    if !Path::new(selector).is_absolute() {
        candidates.push(config.projects_root.join(selector));
    }
    for candidate in candidates {
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if !canonical.is_dir() {
            continue;
        }
        if let Some(entry) = registry.find_by_path(&canonical) {
            return Ok(entry.clone());
        }
        bail!(
            "{} is not registered; run `hegn project assign {} --vm <name>` or \
             `hegn project import {}`",
            canonical.display(),
            canonical.display(),
            canonical.display()
        );
    }
    bail!(
        "project '{selector}' is not registered; run `hegn project assign <path> --vm <name>` \
         or `hegn project import <path>`"
    )
}

/// VM of a registered project; an unassigned project is a hard error.
pub(crate) fn vm_for_entry(config: &Config, entry: &ProjectEntry) -> Result<VmConfig> {
    let vm = entry.vm.as_deref().with_context(|| {
        format!(
            "project '{}' is not assigned to a VM; run `hegn project assign {} --vm <name>`",
            entry.name, entry.name
        )
    })?;
    VmStore::new(config)
        .load(vm)
        .with_context(|| format!("project '{}' names unknown VM '{vm}'", entry.name))
}

/// Pick the only running VM, or fail with the list of candidates.
pub(crate) fn only_running_vm(registry: &VmRegistry) -> Result<VmConfig> {
    let mut running: Vec<_> = registry
        .summaries()?
        .into_iter()
        .filter(|vm| vm.state == "running")
        .collect();
    match running.len() {
        0 => bail!("no running VM; start one with `hegn vm start <name>`"),
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

/// Ask a yes/no question on a terminal. Non-interactive callers must pass the
/// command's explicit confirmation flag; `--json` is always non-interactive.
pub(crate) fn confirm(json: bool, question: &str, hint: &str) -> Result<bool> {
    use std::io::BufRead;

    if json {
        bail!("confirmation required; pass {hint} in non-interactive mode");
    }
    if !std::io::stdin().is_terminal() {
        bail!("confirmation required; pass {hint} in non-interactive mode");
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

/// Shared by `project delete`, which also needs a file policy flag.
pub(crate) fn prompt_yes(json: bool, question: &str) -> Result<bool> {
    confirm(json, question, "--yes and a file policy flag")
}

/// Prefix a guest command with a `cd` into an exported project directory.
///
/// The launch API has no dedicated cwd field yet: the typed command is the
/// only channel the serial console offers. This helper keeps that wiring in one
/// place instead of teaching each caller the shell quoting.
pub(crate) fn guest_cwd_command(guest_path: &str, command: Option<&str>) -> String {
    let quoted = shell_quote(guest_path);
    match command.map(str::trim).filter(|command| !command.is_empty()) {
        Some(command) => format!("cd {quoted} && {command}"),
        None => format!("cd {quoted}"),
    }
}

/// Wait for the guest provisioner while showing a rough progress bar.
///
/// Pre-provisioned images never emit progress, so the wait is bounded: after a
/// short grace period the console is attached normally.
pub(crate) fn wait_for_provision(log: Option<&Path>, plan: &ProvisionPlan) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;
    use crate::config::Harness;
    use chrono::Utc;

    fn entry(name: &str, path: &Path) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            path: path.canonicalize().unwrap(),
            vm: None,
            read_only: false,
            kind: ProjectKind::External,
            parent: None,
            managed: false,
            hidden: false,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn guest_cwd_command_quotes_paths_and_keeps_the_command_verbatim() {
        assert_eq!(
            guest_cwd_command("/projects/demo", Some("opencode")),
            "cd /projects/demo && opencode"
        );
        assert_eq!(
            guest_cwd_command("/projects/demo", Some("  ")),
            "cd /projects/demo"
        );
        assert_eq!(
            guest_cwd_command("/projects/a b", Some("printf '%s' \"x\"")),
            "cd '/projects/a b' && printf '%s' \"x\""
        );
    }

    #[test]
    fn vm_delete_without_yes_fails_closed_in_json_mode() {
        let (_dir, config) = config();
        VmStore::new(&config)
            .save(&VmConfig::from_config(
                &config,
                "rust",
                Some(Harness::Shell),
            ))
            .unwrap();
        let err = vm_command(
            VmCommands::Delete {
                name: "rust".into(),
                yes: false,
                delete_files: false,
            },
            config,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--yes"), "{err}");
    }

    #[test]
    fn member_views_resolve_exported_guest_paths_and_primary_marker() {
        let (dir, config) = config();
        let store = VmStore::new(&config);
        let mut vm = VmConfig::from_config(&config, "rust", Some(Harness::Shell));
        vm.primary = Some("parser".into());
        store.save(&vm).unwrap();

        let parser = dir.path().join("projects/parser");
        let cli = dir.path().join("projects/cli");
        std::fs::create_dir_all(&parser).unwrap();
        std::fs::create_dir_all(&cli).unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("parser", &parser)).unwrap();
        registry.upsert(entry("cli", &cli)).unwrap();
        registry.assign("parser", "rust", false).unwrap();
        registry.assign("cli", "rust", true).unwrap();

        let views = member_views(&config, &vm).unwrap();
        assert_eq!(views.len(), 2);
        let parser_view = views.iter().find(|view| view.name == "parser").unwrap();
        assert_eq!(parser_view.guest_path.as_deref(), Some("/projects/parser"));
        assert!(parser_view.primary);
        assert!(!parser_view.read_only);
        let cli_view = views.iter().find(|view| view.name == "cli").unwrap();
        assert_eq!(cli_view.guest_path.as_deref(), Some("/projects/cli"));
        assert!(cli_view.read_only);
        assert!(!cli_view.primary);
        assert_eq!(
            guest_path_for(&config, &vm, "cli").unwrap(),
            "/projects/cli"
        );
        assert!(guest_path_for(&config, &vm, "missing").is_err());
    }

    #[test]
    fn vm_delete_drops_clone_and_worktree_registrations() {
        let (dir, config) = config();
        let store = VmStore::new(&config);
        store
            .save(&VmConfig::from_config(
                &config,
                "rust",
                Some(Harness::Shell),
            ))
            .unwrap();

        let parser = dir.path().join("projects/parser");
        std::fs::create_dir_all(&parser).unwrap();
        let clone_path = config.vms_dir.join("rust/workspaces/parser-fix");
        std::fs::create_dir_all(&clone_path).unwrap();
        let worktree_path = parser.join(".hegn/worktrees/experiment");
        std::fs::create_dir_all(&worktree_path).unwrap();

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("parser", &parser)).unwrap();
        registry.assign("parser", "rust", false).unwrap();
        let mut clone = entry("parser-fix", &clone_path);
        clone.kind = ProjectKind::Clone;
        clone.managed = true;
        clone.vm = Some("rust".into());
        clone.parent = Some("parser".into());
        registry.upsert(clone).unwrap();
        let mut worktree = entry("experiment", &worktree_path);
        worktree.kind = ProjectKind::Worktree;
        worktree.vm = Some("rust".into());
        worktree.parent = Some("parser".into());
        registry.upsert(worktree).unwrap();

        let code = vm_command(
            VmCommands::Delete {
                name: "rust".into(),
                yes: true,
                delete_files: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(!config.vms_dir.join("rust").exists());
        let registry = ProjectRegistry::load(&config.state_dir).unwrap();
        assert!(registry.get("parser-fix").is_none());
        assert!(registry.get("experiment").is_none());
        assert!(registry.get("parser").unwrap().vm.is_none());
        assert!(parser.is_dir(), "external projects must survive");
        registry
            .validate(&config, &VmStore::new(&config))
            .expect("registry must stay valid after VM deletion");
    }

    #[test]
    fn resolve_project_entry_requires_registration() {
        let (dir, config) = config();
        let path = dir.path().join("projects/demo");
        std::fs::create_dir_all(&path).unwrap();
        let err = resolve_project_entry(&config, &path.display().to_string())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not registered"), "{err}");

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("demo", &path)).unwrap();
        let resolved = resolve_project_entry(&config, "demo").unwrap();
        assert_eq!(resolved.name, "demo");
        assert_eq!(
            resolve_project_entry(&config, &path.display().to_string())
                .unwrap()
                .name,
            "demo"
        );
    }
}
