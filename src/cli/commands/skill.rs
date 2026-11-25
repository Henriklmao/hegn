//! `hegn skills` handlers: push host skills into an instance and copy guest
//! changes back to the host.

use anyhow::{Result, bail};

use crate::config::Config;
use crate::vm::registry::VmRegistry;

use crate::cli::args::SkillCommands;

pub(crate) fn skill_command(command: SkillCommands, config: Config, json: bool) -> Result<i32> {
    let registry = VmRegistry::new(config);
    match command {
        SkillCommands::Sync { instance: selector } => {
            let vm = registry.resolve(&selector)?;
            let report = crate::agent::profile::sync_skills(registry.config(), &vm)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Copied {} file(s) from {} host source(s) into instance '{}'",
                    report.copied_files, report.sources, report.vm
                );
                for warning in &report.warnings {
                    eprintln!("hegn: {warning}");
                }
            }
            Ok(0)
        }
        SkillCommands::SyncBack {
            instance: selector,
            dry_run,
            force,
        } => {
            let vm = registry.resolve(&selector)?;
            let manager = registry.manager(vm.clone())?;
            if manager.is_running() && !force && !dry_run {
                bail!(
                    "instance '{}' is running; stop it first or pass --force to copy live files",
                    vm.name
                );
            }
            let report = crate::agent::profile::sync_skills_back(registry.config(), &vm, dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let action = if dry_run { "Would copy" } else { "Copied" };
                println!(
                    "{} {} file(s) from instance '{}' back to the host ({} unchanged)",
                    action, report.copied_files, report.vm, report.skipped_files
                );
                for warning in &report.warnings {
                    eprintln!("hegn: {warning}");
                }
            }
            Ok(0)
        }
    }
}
