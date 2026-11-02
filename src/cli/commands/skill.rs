//! `hegn skills` handlers: push host skills into an instance and copy guest
//! changes back to the host.

use anyhow::{Result, bail};

use crate::config::Config;
use crate::vm::instance::InstanceRegistry;

use crate::cli::args::SkillCommands;

pub(crate) fn skill_command(command: SkillCommands, config: Config, json: bool) -> Result<i32> {
    let registry = InstanceRegistry::new(config);
    match command {
        SkillCommands::Sync { instance } => {
            let instance = registry.resolve(&instance)?;
            let report = crate::agent::profile::sync_skills(registry.config(), &instance)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Copied {} file(s) from {} host source(s) into instance '{}'",
                    report.copied_files, report.sources, report.instance
                );
                for warning in &report.warnings {
                    eprintln!("hegn: {warning}");
                }
            }
            Ok(0)
        }
        SkillCommands::SyncBack {
            instance,
            dry_run,
            force,
        } => {
            let instance = registry.resolve(&instance)?;
            let manager = registry.manager(instance.clone());
            if manager.is_running() && !force && !dry_run {
                bail!(
                    "instance '{}' is running; stop it first or pass --force to copy live files",
                    instance.name
                );
            }
            let report =
                crate::agent::profile::sync_skills_back(registry.config(), &instance, dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let action = if dry_run { "Would copy" } else { "Copied" };
                println!(
                    "{} {} file(s) from instance '{}' back to the host ({} unchanged)",
                    action, report.copied_files, report.instance, report.skipped_files
                );
                for warning in &report.warnings {
                    eprintln!("hegn: {warning}");
                }
            }
            Ok(0)
        }
    }
}
