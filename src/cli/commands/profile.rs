//! `hegn profile` handlers for the per-harness guest profile templates.

use anyhow::Result;

use crate::config::{Config, Harness};
use crate::vm::instance::InstanceRegistry;

use super::config::run_editor;
use crate::cli::args::ProfileCommands;

pub(crate) fn profile_command(command: ProfileCommands, config: Config, json: bool) -> Result<i32> {
    let store = crate::agent::profile::ProfileStore::new(&config);
    match command {
        ProfileCommands::List => {
            let mut entries = Vec::new();
            for harness in Harness::ALL {
                if harness == Harness::Shell {
                    continue;
                }
                let path = store.path(harness);
                let exists = path.is_file();
                entries.push(serde_json::json!({
                    "harness": harness.slug(),
                    "path": path,
                    "exists": exists,
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                println!("{:<16} {:<6} PATH", "HARNESS", "STATE");
                for entry in &entries {
                    println!(
                        "{:<16} {:<6} {}",
                        entry["harness"].as_str().unwrap_or(""),
                        if entry["exists"].as_bool().unwrap_or(false) {
                            "ok"
                        } else {
                            "missing"
                        },
                        entry["path"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        ProfileCommands::Show { harness } => {
            let profile = store.load(harness)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&profile)?);
            } else {
                print!("{}", toml::to_string_pretty(&profile)?);
            }
            Ok(0)
        }
        ProfileCommands::Path { harness, edit } => {
            let path = store.path(harness);
            if edit {
                if !path.is_file() {
                    store.write_templates()?;
                }
                run_editor(&path)?;
                return Ok(0);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&path)?);
            } else {
                println!("{}", path.display());
            }
            Ok(0)
        }
        ProfileCommands::Init => {
            let written = store.write_templates()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&written)?);
            } else if written.is_empty() {
                println!(
                    "Profile templates already exist in {}",
                    store.dir().display()
                );
            } else {
                for path in &written {
                    println!("Wrote {}", path.display());
                }
            }
            Ok(0)
        }
        ProfileCommands::Sync { instance } => {
            let registry = InstanceRegistry::new(config);
            let instance = registry.resolve(&instance)?;
            let plan = crate::agent::profile::prepare_for_start(registry.config(), &instance)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "instance": instance.name,
                        "packages": plan.packages,
                        "share": plan.share.as_ref().map(|share| &share.bundle_dir),
                        "warnings": plan.warnings,
                    }))?
                );
            } else {
                match &plan.share {
                    Some(share) => println!(
                        "Profile payload for '{}' refreshed at {}",
                        instance.name,
                        share.bundle_dir.display()
                    ),
                    None => println!(
                        "Profile for '{}' has no files, skills or environment to export",
                        instance.name
                    ),
                }
                for warning in &plan.warnings {
                    eprintln!("hegn: {warning}");
                }
                println!("Restart the instance to apply configuration files.");
            }
            Ok(0)
        }
    }
}
