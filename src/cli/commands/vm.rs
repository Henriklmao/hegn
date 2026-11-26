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
use crate::session::ssh::{GuestEnv, SshError, SshIdentity, SshTarget};
use crate::shared::text::shell_quote;
use crate::vm::config::{SshUser, VmConfig, VmStore};
use crate::vm::manager::{BootPlan, SessionRecord, VmManager, VmPaths};
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
        VmCommands::Shell {
            name,
            project,
            serial,
        } => {
            let transport = TransportChoice::from_serial(serial);
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
            let manager = registry.manager(vm.clone())?;
            let plan = ProvisionPlan::new(vm.harness, &vm.extra_packages).ok();
            let started = !manager.is_running();
            let report = if started {
                Some(manager.start()?)
            } else {
                None
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
            match transport {
                TransportChoice::Serial => {
                    let command = guest_cwd_command(
                        &guest_path,
                        (!spec.is_shell()).then_some(spec.command.as_str()),
                    );
                    let console = manager.attach_console()?;
                    crate::session::console::run_interactive_with_command(
                        console,
                        spec.bar,
                        Some(&command),
                    )?;
                    Ok(0)
                }
                TransportChoice::Ssh => {
                    let session = running_session(&manager, &vm)?;
                    let target = ssh_target(&config, &vm, &session)?;
                    if started {
                        ssh_ready_context(&vm.name, target.wait_ready(SSH_READY_TIMEOUT))?;
                    }
                    let command = ssh_session_command(
                        Some(&guest_path),
                        (!spec.is_shell()).then_some(spec.command.as_str()),
                    );
                    run_ssh_session(&target, command.as_deref())
                }
            }
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
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        return Ok(false);
    }
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

/// How long `hegn open`/`hegn vm shell` wait for a booted guest to accept SSH.
pub(crate) const SSH_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Transport chosen by the `--serial` flag. SSH is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportChoice {
    Ssh,
    Serial,
}

impl TransportChoice {
    pub(crate) fn from_serial(serial: bool) -> Self {
        if serial { Self::Serial } else { Self::Ssh }
    }

    pub(crate) fn is_serial(self) -> bool {
        self == Self::Serial
    }
}

/// Validate the SSH connection fields a running VM records at boot.
///
/// The record is only trusted after it matches the VM's current config: the
/// control address must belong to the VM's TAP slot and the account must be
/// the one the config selects. A mismatch means the record is stale, so using
/// it would connect to the wrong guest or as the wrong user.
pub(crate) fn session_ssh_endpoint(
    session: &SessionRecord,
    vm: &VmConfig,
    config: &Config,
) -> Result<(std::net::Ipv4Addr, String)> {
    let guest_ip = session
        .guest_ip
        .with_context(|| missing_ssh_fields(&vm.name))?;
    let ssh_user = session
        .ssh_user
        .as_deref()
        .map(str::trim)
        .filter(|user| !user.is_empty())
        .with_context(|| missing_ssh_fields(&vm.name))?;

    let recorded_user = SshUser::ALL
        .into_iter()
        .find(|user| user.as_str() == ssh_user)
        .with_context(|| {
            format!(
                "VM '{}' records SSH user '{ssh_user}', which is not a hegn guest account; {}.",
                vm.name,
                ssh_record_hint(&vm.name)
            )
        })?;
    let expected_user = vm
        .effective_ssh_user(config.guest_credential_hardening)
        .with_context(|| {
            format!(
                "cannot resolve the SSH user for VM '{}'; {}.",
                vm.name,
                ssh_record_hint(&vm.name)
            )
        })?;
    if recorded_user != expected_user {
        bail!(
            "VM '{}' records SSH user '{ssh_user}', but its config selects '{}'; {}.",
            vm.name,
            expected_user.as_str(),
            ssh_record_hint(&vm.name)
        );
    }

    let index = vm.network_index.with_context(|| {
        format!(
            "VM '{}' has no control network slot in its config (network_index is unset); {}.",
            vm.name,
            ssh_record_hint(&vm.name)
        )
    })?;
    if index >= crate::network::TAP_POOL {
        bail!(
            "VM '{}' has control network slot {index}, outside the {} slot pool; {}.",
            vm.name,
            crate::network::TAP_POOL,
            ssh_record_hint(&vm.name)
        );
    }
    let expected_ip = crate::network::control_guest_ip(index);
    if guest_ip != expected_ip {
        bail!(
            "VM '{}' records SSH address {guest_ip}, but control slot {index} owns \
             {expected_ip}; {}.",
            vm.name,
            ssh_record_hint(&vm.name)
        );
    }
    Ok((guest_ip, ssh_user.to_string()))
}

fn missing_ssh_fields(vm: &str) -> String {
    format!(
        "VM '{vm}' has no SSH address or user in its session record; it was probably started \
         before SSH support. {}.",
        ssh_record_hint(vm)
    )
}

fn ssh_record_hint(vm: &str) -> String {
    format!("run 'hegn vm apply {vm}' and restart the VM, or use 'hegn attach {vm} --serial'")
}

/// Guest session context forwarded over SSH.
pub(crate) fn session_guest_env(vm: &str, session: &SessionRecord) -> GuestEnv {
    let project = session
        .project
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_default();
    let workspace = if session.workspace.as_os_str().is_empty() {
        &session.project
    } else {
        &session.workspace
    };
    GuestEnv {
        vm: vm.to_string(),
        project,
        harness: session.harness.label().to_string(),
        workspace: workspace.display().to_string(),
    }
}

/// Load the session record of a running VM, or fail with a start hint.
pub(crate) fn running_session(manager: &VmManager, vm: &VmConfig) -> Result<SessionRecord> {
    if !manager.is_running() {
        bail!(
            "VM '{}' is not running; start it with `hegn vm start {}`",
            vm.name,
            vm.name
        );
    }
    manager.session().with_context(|| {
        format!(
            "VM '{}' has no readable session record; stop it and start it again with \
             `hegn vm start {}`",
            vm.name, vm.name
        )
    })
}

/// Build the SSH target of a running VM from its recorded session.
pub(crate) fn ssh_target(
    config: &Config,
    vm: &VmConfig,
    session: &SessionRecord,
) -> Result<SshTarget> {
    let (guest_ip, ssh_user) = session_ssh_endpoint(session, vm, config)?;
    let identity = SshIdentity::new(&config.vms_dir, &vm.name);
    // `load` never regenerates a key: a running guest would not know it.
    identity.load()?;
    let target = SshTarget::new(
        vm.name.clone(),
        guest_ip,
        ssh_user,
        identity.key().to_path_buf(),
        identity.known_hosts().to_path_buf(),
    )
    .with_context(|| format!("invalid SSH target for VM '{}'", vm.name))?;
    Ok(target.with_env(session_guest_env(&vm.name, session)))
}

/// The remote command for an SSH session, anchored in the guest project path.
///
/// With a project path and no command the remote shell is replaced by an
/// interactive login shell that starts in that directory. Without a path the
/// raw command is passed through unchanged.
pub(crate) fn ssh_session_command(
    guest_path: Option<&str>,
    command: Option<&str>,
) -> Option<String> {
    let command = command.map(str::trim).filter(|command| !command.is_empty());
    match (guest_path, command) {
        (Some(path), Some(command)) => Some(guest_cwd_command(path, Some(command))),
        (Some(path), None) => Some(format!(
            "cd {} && exec \"${{SHELL:-/bin/sh}}\" -l",
            shell_quote(path)
        )),
        (None, Some(command)) => Some(command.to_string()),
        (None, None) => None,
    }
}

/// Run an SSH session and propagate the remote exit code.
///
/// Transport failures become a hegn error that names the VM and destination.
pub(crate) fn run_ssh_session(target: &SshTarget, command: Option<&str>) -> Result<i32> {
    ssh_failure_context(target, target.interactive(command))
}

/// Attach the VM/destination context to an SSH result, for callers and tests.
pub(crate) fn ssh_failure_context(
    target: &SshTarget,
    result: std::result::Result<i32, SshError>,
) -> Result<i32> {
    result.with_context(|| {
        format!(
            "SSH to VM '{}' failed ({}@{})",
            target.vm, target.user, target.host
        )
    })
}

/// Turn a failed readiness probe into an actionable `hegn open` error.
pub(crate) fn ssh_ready_context(vm: &str, result: std::result::Result<(), SshError>) -> Result<()> {
    result.with_context(|| {
        format!(
            "VM '{vm}' is running, but SSH is not ready. Try 'hegn attach {vm} --serial', or run \
             'hegn vm apply {vm}' and restart the VM."
        )
    })
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
    fn vm_rebuild_removes_the_private_rootfs_and_ssh_identity() {
        use crate::session::ssh::SshIdentity;

        let (_dir, config) = config();
        let store = VmStore::new(&config);
        let vm = VmConfig::from_config(&config, "rust", Some(Harness::Shell));
        store.save(&vm).unwrap();
        let rootfs = vm.rootfs_path(&config);
        std::fs::create_dir_all(rootfs.parent().unwrap()).unwrap();
        std::fs::write(&rootfs, b"disk").unwrap();
        let identity = SshIdentity::new(&config.vms_dir, "rust");
        std::fs::create_dir_all(identity.dir()).unwrap();
        std::fs::write(identity.key(), b"client-key").unwrap();
        std::fs::write(identity.known_hosts(), b"host-key").unwrap();

        let code = vm_command(
            VmCommands::Rebuild {
                name: "rust".into(),
                yes: true,
            },
            config.clone(),
            true,
        )
        .unwrap();

        assert_eq!(code, 0);
        assert!(!rootfs.exists());
        assert!(
            !identity.dir().exists(),
            "rebuild must reset the SSH identity"
        );
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

    fn session_record() -> SessionRecord {
        SessionRecord {
            project: PathBuf::from("/projects/parser"),
            workspace: PathBuf::from("/projects/parser"),
            vm: "rust".into(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid: 0,
            vm_binary: None,
            relay_pid: 0,
            virtiofs_pids: Vec::new(),
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
            membership: Vec::new(),
            guest_ip: Some(std::net::Ipv4Addr::new(10, 214, 0, 2)),
            ssh_user: Some("root".into()),
        }
    }

    #[test]
    fn serial_flag_maps_to_the_transport_choice() {
        assert_eq!(TransportChoice::from_serial(false), TransportChoice::Ssh);
        assert_eq!(TransportChoice::from_serial(true), TransportChoice::Serial);
        assert!(!TransportChoice::from_serial(false).is_serial());
        assert!(TransportChoice::from_serial(true).is_serial());
    }

    fn ssh_vm(config: &Config, index: Option<u8>) -> VmConfig {
        let mut vm = VmConfig::from_config(config, "rust", Some(Harness::Shell));
        vm.network_index = index;
        vm
    }

    fn endpoint_error(record: &SessionRecord, vm: &VmConfig, config: &Config) -> String {
        format!(
            "{:#}",
            session_ssh_endpoint(record, vm, config).unwrap_err()
        )
    }

    #[test]
    fn session_endpoint_requires_both_recorded_fields() {
        let (_dir, config) = config();
        let vm = ssh_vm(&config, Some(0));
        let mut record = session_record();
        let (ip, user) = session_ssh_endpoint(&record, &vm, &config).unwrap();
        assert_eq!(ip, std::net::Ipv4Addr::new(10, 214, 0, 2));
        assert_eq!(user, "root");

        record.ssh_user = None;
        let err = endpoint_error(&record, &vm, &config);
        assert!(err.contains("--serial"), "{err}");
        assert!(err.contains("hegn vm apply rust"), "{err}");

        record.ssh_user = Some("  ".into());
        assert!(session_ssh_endpoint(&record, &vm, &config).is_err());

        record.ssh_user = Some("root".into());
        record.guest_ip = None;
        let err = endpoint_error(&record, &vm, &config);
        assert!(err.contains("--serial"), "{err}");
    }

    #[test]
    fn session_endpoint_must_match_the_vm_control_slot_and_user() {
        let (_dir, config) = config();
        let vm = ssh_vm(&config, Some(0));
        let mut record = session_record();

        record.guest_ip = Some(std::net::Ipv4Addr::new(10, 214, 0, 6));
        let err = endpoint_error(&record, &vm, &config);
        assert!(err.contains("control slot 0"), "{err}");
        assert!(err.contains("10.214.0.2"), "{err}");
        assert!(err.contains("hegn attach rust --serial"), "{err}");
        assert!(err.contains("hegn vm apply rust"), "{err}");

        record.guest_ip = Some(std::net::Ipv4Addr::new(10, 214, 0, 2));
        record.ssh_user = Some("ubuntu".into());
        let err = endpoint_error(&record, &vm, &config);
        assert!(err.contains("selects 'root'"), "{err}");
        assert!(err.contains("hegn attach rust --serial"), "{err}");
        assert!(err.contains("hegn vm apply rust"), "{err}");

        record.ssh_user = Some("nobody".into());
        let err = endpoint_error(&record, &vm, &config);
        assert!(err.contains("not a hegn guest account"), "{err}");

        let mut vm_hegn = ssh_vm(&config, Some(0));
        vm_hegn.ssh_user = Some(SshUser::Hegn);
        record.ssh_user = Some("hegn".into());
        record.guest_ip = Some(crate::network::control_guest_ip(0));
        assert_eq!(
            session_ssh_endpoint(&record, &vm_hegn, &config).unwrap(),
            (crate::network::control_guest_ip(0), "hegn".to_string())
        );

        let vm_without_slot = ssh_vm(&config, None);
        record.ssh_user = Some("root".into());
        let err = endpoint_error(&record, &vm_without_slot, &config);
        assert!(err.contains("network_index is unset"), "{err}");
        assert!(err.contains("hegn attach rust --serial"), "{err}");
        assert!(err.contains("hegn vm apply rust"), "{err}");
    }

    #[test]
    fn guest_env_uses_the_recorded_project_and_workspace() {
        let record = session_record();
        let env = session_guest_env("rust", &record);
        assert_eq!(env.vm, "rust");
        assert_eq!(env.project, "parser");
        assert_eq!(env.harness, "shell");
        assert_eq!(env.workspace, "/projects/parser");
    }

    #[test]
    fn ssh_session_command_anchors_the_guest_path_and_quotes_it() {
        assert_eq!(
            ssh_session_command(Some("/projects/a b"), Some("opencode")),
            Some("cd '/projects/a b' && opencode".into())
        );
        assert_eq!(
            ssh_session_command(Some("/projects/demo"), Some("  ")),
            Some("cd /projects/demo && exec \"${SHELL:-/bin/sh}\" -l".into())
        );
        assert_eq!(
            ssh_session_command(None, Some("opencode")),
            Some("opencode".into())
        );
        assert_eq!(ssh_session_command(None, None), None);
    }

    #[test]
    fn ssh_target_uses_the_recorded_endpoint_and_identity() {
        let (_dir, config) = config();
        let mut vm = VmConfig::from_config(&config, "rust", Some(Harness::Shell));
        vm.network_index = Some(0);
        vm.ssh_user = Some(SshUser::Hegn);
        let identity = SshIdentity::new(&config.vms_dir, "rust");
        if identity.ensure().is_err() {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        }
        let mut record = session_record();
        record.ssh_user = Some("hegn".into());

        let target = ssh_target(&config, &vm, &record).unwrap();
        assert_eq!(target.vm, "rust");
        assert_eq!(target.host, std::net::Ipv4Addr::new(10, 214, 0, 2));
        assert_eq!(target.user, "hegn");
        assert_eq!(target.key, identity.key());
        assert_eq!(target.known_hosts, identity.known_hosts());
        assert_eq!(target.env.project, "parser");
    }

    #[test]
    fn ssh_exit_codes_are_propagated_and_transport_errors_gain_context() {
        let target = SshTarget::new(
            "rust",
            std::net::Ipv4Addr::new(10, 214, 0, 2),
            "root",
            PathBuf::from("/tmp/key"),
            PathBuf::from("/tmp/known_hosts"),
        )
        .unwrap();

        assert_eq!(ssh_failure_context(&target, Ok(7)).unwrap(), 7);

        let err = ssh_failure_context(
            &target,
            Err(SshError::TransportAfterConnect {
                vm: "rust".into(),
                user: "root".into(),
                host: std::net::Ipv4Addr::new(10, 214, 0, 2),
            }),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("SSH to VM 'rust' failed (root@10.214.0.2)"),
            "{err}"
        );
        assert!(err.contains("--serial"), "{err}");
    }

    #[test]
    fn ssh_ready_errors_name_the_serial_recovery_path() {
        let err = ssh_ready_context(
            "rust",
            Err(SshError::ConnectionRefused {
                vm: "rust".into(),
                user: "root".into(),
                host: std::net::Ipv4Addr::new(10, 214, 0, 2),
            }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("SSH is not ready"), "{err}");
        assert!(err.contains("hegn attach rust --serial"), "{err}");
        assert!(err.contains("hegn vm apply rust"), "{err}");
    }
}
