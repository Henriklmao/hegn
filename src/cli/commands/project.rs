//! `hegn project` handlers: create, import, inspect, assign, unassign, fork,
//! delete and restore projects. Control-plane membership always lives in the
//! private project registry; a VM is never deleted from here.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::{Config, Harness};
use crate::project::{
    self, ForkMode, ProjectEntry, ProjectKind, ProjectRegistry, WorkspaceService, assign_project,
    registered_entry, unassign_project,
};
use crate::vm::config::{VmConfig, VmStore};
use crate::vm::registry::VmRegistry;

use super::vm::{confirm, guest_path_for, prompt_yes, vm_for_entry};
use crate::cli::args::{ForkModeArg, ProjectCommands};
use crate::cli::resolve::{resolve_project, resolve_project_entry as resolve_picker_entry};

pub(crate) fn project_command(command: ProjectCommands, config: Config, json: bool) -> Result<i32> {
    match command {
        ProjectCommands::Create {
            name,
            template,
            vm,
            solo,
        } => {
            let harness = config.harness;
            let template = match template {
                Some(path) => {
                    if !path.is_file() {
                        bail!("template {} does not exist", path.display());
                    }
                    Some(path)
                }
                None => {
                    let path = config.templates_dir.join("AGENTS.md");
                    path.is_file().then_some(path)
                }
            };
            let path = project::create_project(
                &config.projects_root,
                &name,
                template.as_deref(),
                harness,
            )?;
            // Harness, packages and provisioning are VM settings now. Project
            // metadata stays presentation-only and is only written when the
            // user changes launch/integration/auto_start explicitly.
            let entry = register_project(&config, &name, &path, None)?;
            let selected = select_vm_for_new_project(
                &config,
                &entry,
                harness,
                vm,
                solo,
                "hegn project create",
            )?;
            print_new_project(&config, &path, selected.as_ref(), json)?;
            Ok(0)
        }
        ProjectCommands::Import {
            path,
            name,
            vm,
            solo,
        } => {
            let link = config.import_project(&path, name.as_deref())?;
            let target = link.canonicalize().with_context(|| {
                format!("failed to resolve imported project {}", link.display())
            })?;
            let info = project::Project::load(link.clone());
            let entry = register_project(&config, &info.name, &target, None)?;
            let selected = select_vm_for_new_project(
                &config,
                &entry,
                config.harness,
                vm,
                solo,
                "hegn project import",
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": info.name,
                        "path": link,
                        "target": target,
                        "git": info.git,
                        "vm": selected.as_ref().map(|vm| vm.name.clone()),
                    }))?
                );
            } else {
                println!("Imported {}", info.name);
                println!("  link   {}", link.display());
                println!("  target {}", target.display());
                println!("  git    {}", info.git.badge());
                if let Some(vm) = &selected {
                    println!("  vm     {}", vm.name);
                } else {
                    println!("Next: hegn project assign {} --vm <name>", info.name);
                }
            }
            Ok(0)
        }
        ProjectCommands::Info { project } => {
            let (entry, path) = match registered_entry(&config, &project)? {
                Some(entry) => {
                    let path = entry.path.clone();
                    (Some(entry), path)
                }
                None => (None, resolve_project(&config, &project)?),
            };
            let info = project::Project::load(path.clone());
            let (kind, vm_name, read_only, managed, parent, guest_path, harness, packages) =
                match &entry {
                    Some(entry) => {
                        let vm = match entry.vm.as_deref() {
                            Some(_) => Some(vm_for_entry(&config, entry)?),
                            None => None,
                        };
                        let guest = vm
                            .as_ref()
                            .and_then(|vm| guest_path_for(&config, vm, &entry.name).ok());
                        (
                            entry.kind,
                            entry.vm.clone(),
                            entry.read_only,
                            entry.managed,
                            entry.parent.clone(),
                            guest,
                            vm.as_ref().map_or(config.harness, |vm| vm.harness),
                            vm.map(|vm| vm.extra_packages).unwrap_or_default(),
                        )
                    }
                    None => (
                        ProjectKind::External,
                        None,
                        false,
                        false,
                        None,
                        None,
                        config.harness,
                        Vec::new(),
                    ),
                };
            if json {
                #[derive(Serialize)]
                struct Info<'a> {
                    name: &'a str,
                    path: &'a Path,
                    git: &'a project::GitSummary,
                    harness: crate::config::Harness,
                    packages: &'a [String],
                    kind: ProjectKind,
                    vm: Option<String>,
                    read_only: bool,
                    managed: bool,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    guest_path: Option<String>,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    parent: Option<String>,
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Info {
                        name: &info.name,
                        path: &info.path,
                        git: &info.git,
                        harness,
                        packages: &packages,
                        kind,
                        vm: vm_name,
                        read_only,
                        managed,
                        guest_path,
                        parent,
                    })?
                );
            } else {
                println!("{}", info.name);
                println!("  path     {}", info.path.display());
                println!("  git      {}", info.git.badge());
                if let Some(err) = &info.git.error {
                    println!("  git note {err}");
                }
                println!("  harness  {}", harness.label());
                if !packages.is_empty() {
                    println!("  packages {}", packages.join(", "));
                }
                println!("  kind     {}", kind_label(kind));
                println!(
                    "  vm       {}",
                    vm_name.as_deref().unwrap_or("- (unassigned)")
                );
                if read_only {
                    println!("  mode     read-only");
                }
                if managed {
                    println!("  managed  yes");
                }
                if let Some(parent) = &parent {
                    println!("  parent   {parent}");
                }
                if let Some(guest) = &guest_path {
                    println!("  guest    {guest}");
                }
            }
            Ok(0)
        }
        ProjectCommands::Assign {
            project,
            vm,
            read_only,
        } => {
            let (entry, vm) = assign_project(&config, &project, &vm, read_only)?;
            let guest = guest_path_for(&config, &vm, &entry.name).ok();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry.name,
                        "vm": vm.name,
                        "read_only": entry.read_only,
                        "primary": vm.primary,
                        "guest_path": guest,
                    }))?
                );
            } else {
                println!(
                    "Assigned '{}' to VM '{}' ({}).",
                    entry.name,
                    vm.name,
                    if entry.read_only {
                        "read-only"
                    } else {
                        "read-write"
                    }
                );
                if let Some(guest) = guest {
                    println!("  guest {guest}");
                }
                if vm.primary.as_deref() == Some(entry.name.as_str()) {
                    println!("  primary yes");
                }
            }
            Ok(0)
        }
        ProjectCommands::Unassign { project } => {
            let outcome = unassign_project(&config, &project)?;
            match outcome {
                Some(outcome) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "project": outcome.project,
                                "vm": outcome.vm,
                                "previous_primary": outcome.previous_primary,
                                "primary": outcome.primary,
                            }))?
                        );
                    } else {
                        println!("Unassigned '{}' from VM '{}'.", outcome.project, outcome.vm);
                        if outcome.was_primary {
                            let running = VmRegistry::new(config.clone())
                                .manager_by_name(&outcome.vm)
                                .map(|manager| manager.is_running())
                                .unwrap_or(false);
                            match &outcome.primary {
                                Some(next) if running => println!(
                                    "  warning: '{}' was primary; run `hegn vm apply {}` so \
                                     /workspace moves to '{}'",
                                    outcome.project, outcome.vm, next
                                ),
                                Some(next) => println!(
                                    "  warning: '{}' was primary; /workspace will point to '{}' \
                                     on the next start",
                                    outcome.project, next
                                ),
                                None => println!(
                                    "  warning: VM '{}' now has no primary project",
                                    outcome.vm
                                ),
                            }
                        }
                    }
                }
                None => {
                    if json {
                        println!("{}", serde_json::json!({"project": project, "vm": null}));
                    } else {
                        println!("Project '{project}' is already unassigned.");
                    }
                }
            }
            Ok(0)
        }
        ProjectCommands::Fork {
            parent,
            name,
            mode,
            target,
        } => {
            let mut registry = ProjectRegistry::load(&config.state_dir)?;
            let parent_entry = registered_entry(&config, &parent)?
                .with_context(|| format!("project '{parent}' is not registered"))?;
            let mode = match mode {
                ForkModeArg::Clone => ForkMode::Clone,
                ForkModeArg::Worktree => ForkMode::Worktree,
            };
            if mode == ForkMode::Clone && target.is_some() {
                bail!(
                    "a clone always lives at <vms_dir>/<vm>/workspaces/<name>; pass --target only \
                     with --mode worktree"
                );
            }
            let name = match name {
                Some(name) => name,
                None => default_fork_name(&registry, &parent_entry.name, mode)?,
            };
            if registry.get(&name).is_some() {
                bail!("a project named '{name}' already exists in the registry");
            }
            let store = VmStore::new(&config);
            let service = WorkspaceService::new(&config, &store);
            let entry = service.fork(
                &mut registry,
                &parent_entry.name,
                &name,
                mode,
                target.as_deref(),
            )?;
            if let Err(err) = registry.validate(&config, &store) {
                let _ = match entry.kind {
                    ProjectKind::Clone => service.remove_clone(&mut registry, &entry.name),
                    ProjectKind::Worktree => service.remove_worktree(&mut registry, &entry.name),
                    ProjectKind::External => Ok(entry.path.clone()),
                };
                return Err(err);
            }
            let vm = entry.vm.as_deref().and_then(|name| store.load(name).ok());
            let guest = vm
                .as_ref()
                .and_then(|vm| guest_path_for(&config, vm, &entry.name).ok());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry,
                        "kind": entry.kind,
                        "path": entry.path,
                        "vm": entry.vm,
                        "guest_path": guest,
                    }))?
                );
            } else {
                println!(
                    "Forked '{}' into '{}' ({}).",
                    parent_entry.name,
                    entry.name,
                    kind_label(entry.kind)
                );
                println!("  path  {}", entry.path.display());
                if let Some(vm) = &entry.vm {
                    println!("  vm    {vm}");
                }
                if let Some(guest) = guest {
                    println!("  guest {guest}");
                }
            }
            Ok(0)
        }
        ProjectCommands::Delete {
            project: selector,
            delete_files,
            keep_files,
            yes,
        } => delete_project_command(&config, &selector, delete_files, keep_files, yes, json),
        ProjectCommands::Restore { project: selector } => {
            let entry = resolve_picker_entry(&config, &selector)?;
            let mut store =
                crate::session::state::StateStore::load(config.state_dir.join("state.json"));
            store.set_project_hidden(&entry, false)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry,
                        "hidden": false,
                    }))?
                );
            } else {
                println!("Restored {} to the picker", entry.display());
            }
            Ok(0)
        }
    }
}

fn print_new_project(
    config: &Config,
    path: &Path,
    vm: Option<&VmConfig>,
    json: bool,
) -> Result<()> {
    let guest = vm.and_then(|vm| {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        guest_path_for(config, vm, &name).ok()
    });
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": path,
                "vm": vm.map(|vm| vm.name.clone()),
                "guest_path": guest,
            }))?
        );
    } else {
        println!("Created {}", path.display());
        match vm {
            Some(vm) => {
                println!("VM: {}", vm.name);
                if let Some(guest) = guest {
                    println!("Guest path: {guest}");
                }
            }
            None => println!("Next: hegn project assign {} --vm <name>", path.display()),
        }
    }
    Ok(())
}

/// Insert a freshly created or imported project into the private registry.
fn register_project(
    config: &Config,
    name: &str,
    path: &Path,
    parent: Option<&str>,
) -> Result<ProjectEntry> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let mut registry = ProjectRegistry::load(&config.state_dir)?;
    if let Some(existing) = registry.get(name)
        && existing.path != canonical
    {
        bail!(
            "a different project named '{}' is already registered at {}",
            name,
            existing.path.display()
        );
    }
    // Re-importing an existing registration must not silently drop its VM
    // membership or read-only mode.
    let entry = registry.get(name).cloned().unwrap_or_else(|| ProjectEntry {
        name: name.to_string(),
        path: canonical,
        vm: None,
        read_only: false,
        kind: ProjectKind::External,
        parent: parent.map(str::to_string),
        managed: false,
        hidden: false,
        created_at: chrono::Utc::now(),
    });
    registry.upsert(entry.clone())?;
    Ok(entry)
}

/// Ask how a newly created/imported project joins a VM.
fn select_vm_for_new_project(
    config: &Config,
    entry: &ProjectEntry,
    harness: Harness,
    vm: Option<String>,
    solo: bool,
    command: &str,
) -> Result<Option<VmConfig>> {
    if let Some(vm_name) = vm {
        let (assigned, vm) = assign_project(config, &entry.name, &vm_name, false)?;
        debug_assert_eq!(assigned.name, entry.name);
        return Ok(Some(vm));
    }
    if solo {
        return Ok(Some(
            VmRegistry::new(config.clone()).ensure_for_project(&entry.path, harness)?,
        ));
    }
    if std::io::stdin().is_terminal() {
        if prompt_yes(&format!(
            "Create a one-member VM named '{}' for this project?",
            entry.name
        ))? {
            return Ok(Some(
                VmRegistry::new(config.clone()).ensure_for_project(&entry.path, harness)?,
            ));
        }
        println!(
            "Project left unassigned. Next: hegn project assign {} --vm <name>",
            entry.name
        );
        return Ok(None);
    }
    bail!(
        "{command} needs a VM: pass --vm <name>, or --solo to create a one-member VM named \
         '{}'",
        entry.name
    )
}

fn default_fork_name(registry: &ProjectRegistry, parent: &str, mode: ForkMode) -> Result<String> {
    let suffix = match mode {
        ForkMode::Clone => "clone",
        ForkMode::Worktree => "worktree",
    };
    let base = format!("{parent}-{suffix}");
    if project::validate_project_name(&base).is_err() {
        bail!("cannot derive a project name from '{base}'; pass --name <name>");
    }
    if registry.get(&base).is_none() {
        return Ok(base);
    }
    for index in 2..1000 {
        let candidate = format!("{base}-{index}");
        if registry.get(&candidate).is_none() {
            return Ok(candidate);
        }
    }
    bail!("could not allocate a free fork name for '{base}'")
}

fn delete_project_command(
    config: &Config,
    selector: &str,
    delete_files: bool,
    keep_files: bool,
    yes: bool,
    json: bool,
) -> Result<i32> {
    let Some(entry) = registered_entry(config, selector)? else {
        return delete_external_picker_project(
            config,
            selector,
            delete_files,
            keep_files,
            yes,
            json,
        );
    };

    match entry.kind {
        ProjectKind::Clone => {
            if keep_files {
                bail!(
                    "clone '{}' is a hegn-managed workspace; omit --keep-files to remove it",
                    entry.name
                );
            }
            let info = project::Project::load(entry.path.clone());
            print_project_git(&entry.path, &info, json);
            if !yes
                && !confirm(
                    &format!(
                        "Delete managed clone '{}' at {}?",
                        entry.name,
                        entry.path.display()
                    ),
                    "--yes",
                )?
            {
                println!("Cancelled.");
                return Ok(0);
            }
            let mut registry = ProjectRegistry::load(&config.state_dir)?;
            let store = VmStore::new(config);
            let service = WorkspaceService::new(config, &store);
            let removed = service.remove_clone(&mut registry, &entry.name)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry.name,
                        "kind": "clone",
                        "removed": removed,
                    }))?
                );
            } else {
                println!("Removed clone '{}' at {}.", entry.name, removed.display());
            }
            Ok(0)
        }
        ProjectKind::Worktree => {
            if keep_files {
                bail!(
                    "worktree '{}' is removed with `git worktree remove`; omit --keep-files",
                    entry.name
                );
            }
            let info = project::Project::load(entry.path.clone());
            print_project_git(&entry.path, &info, json);
            if !yes
                && !confirm(
                    &format!(
                        "Remove git worktree '{}' at {}? The parent project stays.",
                        entry.name,
                        entry.path.display()
                    ),
                    "--yes",
                )?
            {
                println!("Cancelled.");
                return Ok(0);
            }
            let mut registry = ProjectRegistry::load(&config.state_dir)?;
            let store = VmStore::new(config);
            let service = WorkspaceService::new(config, &store);
            let removed = service.remove_worktree(&mut registry, &entry.name)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry.name,
                        "kind": "worktree",
                        "removed": removed,
                        "parent": entry.parent,
                    }))?
                );
            } else {
                println!(
                    "Removed worktree '{}' at {}; the parent project is untouched.",
                    entry.name,
                    removed.display()
                );
            }
            Ok(0)
        }
        ProjectKind::External => {
            delete_registered_external(config, &entry, delete_files, keep_files, yes, json)
        }
    }
}

fn delete_registered_external(
    config: &Config,
    entry: &ProjectEntry,
    delete_files: bool,
    keep_files: bool,
    yes: bool,
    json: bool,
) -> Result<i32> {
    let info = project::Project::load(entry.path.clone());
    print_project_git(&entry.path, &info, json);
    let registry = ProjectRegistry::load(&config.state_dir)?;
    let clones: Vec<&str> = registry
        .entries()
        .iter()
        .filter(|candidate| {
            candidate.kind == ProjectKind::Clone
                && candidate.parent.as_deref() == Some(entry.name.as_str())
        })
        .map(|candidate| candidate.name.as_str())
        .collect();
    if !clones.is_empty() && !json {
        eprintln!(
            "note: clone(s) {} keep their parent name as display-only lineage; they are not \
             deleted",
            clones.join(", ")
        );
    }
    let is_import_link = registered_import_link(config, entry);
    let is_direct_child = registered_direct_child(config, entry);
    if !yes
        && !prompt_yes(&format!(
            "Remove project '{}' from hegn? Its VM is never deleted.",
            entry.name
        ))?
    {
        println!("Cancelled.");
        return Ok(0);
    }
    let delete_files = if delete_files {
        true
    } else if keep_files {
        false
    } else if std::io::stdin().is_terminal() {
        prompt_yes(&format!(
            "Also delete the project files at {}?",
            entry.path.display()
        ))?
    } else {
        bail!("choose --delete-files or --keep-files in non-interactive mode");
    };
    let outcome = project::delete_registered_project(config, entry, delete_files)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "project": entry.path,
                "name": entry.name,
                "kind": "external",
                "files_deleted": outcome.files_deleted,
                "instances": outcome.instances,
            }))?
        );
    } else if outcome.files_deleted {
        println!(
            "Removed '{}' and deleted files at {}.",
            entry.name,
            entry.path.display()
        );
    } else if is_import_link {
        println!(
            "Removed import link for '{}'; target {} kept.",
            entry.name,
            entry.path.display()
        );
    } else if is_direct_child {
        println!(
            "Removed '{}' from the picker; project files kept.",
            entry.name
        );
    } else {
        println!(
            "Removed registry entry '{}'; files kept at {}.",
            entry.name,
            entry.path.display()
        );
    }
    Ok(0)
}

fn registered_import_link(config: &Config, entry: &ProjectEntry) -> bool {
    let picker = config.projects_root.join(&entry.name);
    if !std::fs::symlink_metadata(&picker)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return false;
    }
    picker
        .canonicalize()
        .ok()
        .is_some_and(|target| target == entry.path)
}

fn registered_direct_child(config: &Config, entry: &ProjectEntry) -> bool {
    config
        .projects_root
        .canonicalize()
        .ok()
        .is_some_and(|root| entry.path.parent() == Some(root.as_path()))
}

fn delete_external_picker_project(
    config: &Config,
    selector: &str,
    delete_files: bool,
    keep_files: bool,
    yes: bool,
    json: bool,
) -> Result<i32> {
    let picker = resolve_picker_entry(config, selector)?;
    let info = project::Project::load(picker.clone());
    print_project_git(&picker, &info, json);
    let entry = ProjectEntry {
        name: info.name.clone(),
        path: picker.canonicalize().unwrap_or_else(|_| picker.clone()),
        vm: None,
        read_only: false,
        kind: ProjectKind::External,
        parent: None,
        managed: false,
        hidden: false,
        created_at: chrono::Utc::now(),
    };
    delete_external_picker_entry(
        config,
        &entry,
        &picker,
        &info,
        delete_files,
        keep_files,
        yes,
        json,
    )
}

#[allow(clippy::too_many_arguments)]
fn delete_external_picker_entry(
    config: &Config,
    entry: &ProjectEntry,
    picker: &Path,
    info: &project::Project,
    delete_files: bool,
    keep_files: bool,
    yes: bool,
    json: bool,
) -> Result<i32> {
    if !yes
        && !prompt_yes(&format!(
            "Remove project '{}' from the picker? Its VM is never deleted.",
            info.name
        ))?
    {
        return Ok(0);
    }
    let delete_files = if delete_files {
        true
    } else if keep_files {
        false
    } else if std::io::stdin().is_terminal() {
        prompt_yes(&format!(
            "Also delete the project files at {}?",
            picker.display()
        ))?
    } else {
        bail!("choose --delete-files or --keep-files in non-interactive mode");
    };
    let outcome = project::delete_project(config, picker, delete_files)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "project": picker,
                "kind": entry.kind,
                "files_deleted": outcome.files_deleted,
                "instances": outcome.instances,
            }))?
        );
    } else {
        println!(
            "Removed {}; {}",
            entry.name,
            if outcome.files_deleted {
                "project files deleted"
            } else {
                "project files kept"
            }
        );
    }
    Ok(0)
}

fn kind_label(kind: ProjectKind) -> &'static str {
    match kind {
        ProjectKind::External => "external",
        ProjectKind::Clone => "clone",
        ProjectKind::Worktree => "worktree",
    }
}

fn print_project_git(path: &Path, info: &project::Project, json: bool) {
    if json {
        return;
    }
    println!("{}", info.name);
    println!("  path     {}", path.display());
    println!("  git      {}", info.git.badge());
    if let Some(error) = &info.git.error {
        println!("  git note {error}");
    }
    if info.git.is_dirty() {
        println!(
            "  changes  {} staged, {} modified, {} untracked",
            info.git.staged, info.git.modified, info.git.untracked
        );
        println!("  warning  uncommitted changes are lost if the files are deleted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;
    use crate::cli::{Cli, Commands};
    use chrono::Utc;
    use clap::Parser;
    use std::path::PathBuf;
    use std::process::Command;

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

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo(config: &Config, name: &str) -> PathBuf {
        let path = config.projects_root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        git_ok(&path, &["init", "--quiet"]);
        git_ok(&path, &["config", "user.email", "test@example.com"]);
        git_ok(&path, &["config", "user.name", "Test"]);
        std::fs::write(path.join("README.md"), format!("# {name}\n")).unwrap();
        git_ok(&path, &["add", "."]);
        git_ok(&path, &["commit", "--quiet", "-m", "initial"]);
        path
    }

    #[test]
    fn project_create_can_create_a_solo_vm_without_writing_profile_metadata() {
        let (_dir, config) = config();
        let code = project_command(
            ProjectCommands::Create {
                name: "demo".into(),
                template: None,
                vm: None,
                solo: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let project = config.projects_root.join("demo");
        assert!(!project.join(project::PROJECT_META_FILE).exists());
        let meta = project::read_meta(&project);
        assert!(meta.harness.is_none());
        assert!(meta.packages.is_empty());
        assert!(meta.template.is_none());
        let registry = ProjectRegistry::load(&config.state_dir).unwrap();
        assert_eq!(registry.get("demo").unwrap().vm.as_deref(), Some("demo"));
        assert_eq!(
            VmStore::new(&config).load("demo").unwrap().harness,
            config.harness
        );
    }

    #[test]
    fn project_create_without_a_vm_fails_closed_when_non_interactive() {
        let (_dir, config) = config();
        let err = project_command(
            ProjectCommands::Create {
                name: "demo".into(),
                template: None,
                vm: None,
                solo: false,
            },
            config,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--solo"), "{err}");
    }

    #[test]
    fn project_import_creates_a_picker_symlink_and_assigns_to_a_vm() {
        let (dir, config) = config();
        VmStore::new(&config)
            .save(&VmConfig::from_config(&config, "rust", None))
            .unwrap();
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();
        let code = project_command(
            ProjectCommands::Import {
                path: source.clone(),
                name: None,
                vm: Some("rust".into()),
                solo: false,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let link = config.projects_root.join("demo");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(link.canonicalize().unwrap(), source.canonicalize().unwrap());
        let registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let entry = registry.get("demo").unwrap();
        assert_eq!(entry.vm.as_deref(), Some("rust"));
    }

    #[test]
    fn project_import_parses_and_rejects_unsafe_sources() {
        let (_dir, config) = config();
        let cli = Cli::try_parse_from([
            "hegn",
            "project",
            "import",
            "/tmp/example",
            "--name",
            "x",
            "--solo",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Project {
                command:
                    ProjectCommands::Import {
                        path,
                        name,
                        vm,
                        solo,
                    },
            }) => {
                assert_eq!(path, PathBuf::from("/tmp/example"));
                assert_eq!(name.as_deref(), Some("x"));
                assert!(vm.is_none());
                assert!(solo);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(config.import_project(Path::new("/"), None).is_err());
    }

    #[test]
    fn project_delete_keeps_or_removes_files_and_can_be_restored() {
        let (_dir, config) = config();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"demo").unwrap();

        let code = project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: false,
                keep_files: true,
                yes: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(project.join("README.md").is_file(), "files must be kept");
        let store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(store.is_hidden(&project));

        let code = project_command(
            ProjectCommands::Restore {
                project: "demo".into(),
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let store = crate::session::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(!store.is_hidden(&project));

        let code = project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: true,
                keep_files: false,
                yes: true,
            },
            config,
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(!project.exists());
    }

    #[test]
    fn project_delete_never_follows_an_import_link() {
        let (dir, config) = config();
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), b"demo").unwrap();
        project_command(
            ProjectCommands::Import {
                path: source.clone(),
                name: None,
                vm: None,
                solo: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        let link = config.projects_root.join("demo");

        assert!(
            resolve_picker_entry(&config, &source.display().to_string()).is_err(),
            "deletion must only target picker entries"
        );
        project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: true,
                keep_files: false,
                yes: true,
            },
            config,
            true,
        )
        .unwrap();
        assert!(!link.exists());
        assert!(source.join("README.md").is_file());
    }

    #[test]
    fn project_create_rejects_a_missing_template() {
        let (_dir, config) = config();
        let err = project_command(
            ProjectCommands::Create {
                name: "demo".into(),
                template: Some(PathBuf::from("/definitely/missing/AGENTS.md")),
                vm: None,
                solo: true,
            },
            config,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn project_fork_dispatches_a_clone_in_a_real_repository() {
        let (_dir, config) = config();
        VmStore::new(&config)
            .save(&VmConfig::from_config(&config, "rust", None))
            .unwrap();
        let parent = repo(&config, "parser");
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(entry("parser", &parent))
            .unwrap();
        assign_project(&config, "parser", "rust", false).unwrap();

        let code = project_command(
            ProjectCommands::Fork {
                parent: "parser".into(),
                name: Some("parser-fix".into()),
                mode: ForkModeArg::Clone,
                target: None,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let clone = registry.get("parser-fix").unwrap();
        assert_eq!(clone.kind, ProjectKind::Clone);
        assert!(clone.managed);
        assert_eq!(clone.vm.as_deref(), Some("rust"));
        assert_eq!(clone.parent.as_deref(), Some("parser"));
        assert!(clone.path.join(".git").is_dir());
        assert!(parent.join("README.md").is_file());
    }

    #[test]
    fn project_delete_removes_a_managed_clone_without_touching_the_parent() {
        let (_dir, config) = config();
        VmStore::new(&config)
            .save(&VmConfig::from_config(&config, "rust", None))
            .unwrap();
        let parent = repo(&config, "parser");
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(entry("parser", &parent))
            .unwrap();
        assign_project(&config, "parser", "rust", false).unwrap();
        project_command(
            ProjectCommands::Fork {
                parent: "parser".into(),
                name: Some("parser-fix".into()),
                mode: ForkModeArg::Clone,
                target: None,
            },
            config.clone(),
            true,
        )
        .unwrap();

        let code = project_command(
            ProjectCommands::Delete {
                project: "parser-fix".into(),
                delete_files: true,
                keep_files: false,
                yes: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(parent.join("README.md").is_file());
        assert!(config.vms_dir.join("rust").is_dir(), "the VM must survive");
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("parser-fix")
                .is_none()
        );
    }
}
