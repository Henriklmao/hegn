//! `hegn snapshot` handler: list, create, restore, delete and show VM
//! snapshots.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::session::snapshot::SnapshotStore;
use crate::vm::instance::InstanceRegistry;

use super::instance::snapshot_instance;
use crate::cli::args::SnapshotCommands;
use crate::cli::output::{print_snapshot, runtime};

pub(crate) fn snapshot_command(
    command: SnapshotCommands,
    config: Config,
    json: bool,
) -> Result<i32> {
    let store = SnapshotStore::new(&config.snapshots_dir);
    let registry = InstanceRegistry::new(config.clone());
    match command {
        SnapshotCommands::List => {
            let snapshots = store.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else if snapshots.is_empty() {
                println!("No snapshots in {}.", store.root().display());
            } else {
                println!(
                    "{:<32} {:<20} {:<18} {:>10}",
                    "NAME", "INSTANCE", "CREATED", "SIZE"
                );
                for snapshot in snapshots {
                    println!(
                        "{:<32} {:<20} {:<18} {:>10}",
                        snapshot.name,
                        snapshot.instance.as_deref().unwrap_or("-"),
                        snapshot.created_at.format("%Y-%m-%d %H:%M:%S"),
                        snapshot.size_label()
                    );
                }
            }
            Ok(0)
        }
        SnapshotCommands::Create { name, instance } => {
            let instance = snapshot_instance(&registry, instance)?;
            let suggested = SnapshotStore::suggested_name(
                instance
                    .project
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("snapshot"),
            );
            let name = name.unwrap_or(suggested);
            let meta = runtime()?.block_on(registry.manager(instance).create_snapshot(&name))?;
            print_snapshot(&meta, json)?;
            Ok(0)
        }
        SnapshotCommands::Restore { name, instance } => {
            let meta = store.load(&name)?;
            let instance = match instance {
                Some(selector) => registry.resolve(&selector)?,
                None => match meta.instance.as_deref() {
                    Some(name) => registry.store().load(name)?,
                    None => {
                        let project = meta
                            .project
                            .as_deref()
                            .context("snapshot has no instance/project metadata")?;
                        registry.resolve(project.to_string_lossy().as_ref())?
                    }
                },
            };
            runtime()?.block_on(registry.manager(instance).restore_snapshot(&meta))?;
            if json {
                println!("{}", serde_json::json!({"restored": meta.name}));
            } else {
                println!("Restored snapshot '{}'.", meta.name);
            }
            Ok(0)
        }
        SnapshotCommands::Delete { name } => {
            store.delete(&name)?;
            if json {
                println!("{}", serde_json::json!({"deleted": name}));
            } else {
                println!("Deleted snapshot '{name}'.");
            }
            Ok(0)
        }
        SnapshotCommands::Show { name } => {
            let meta = store.load(&name)?;
            print_snapshot(&meta, json)?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;

    #[test]
    fn snapshot_list_empty() {
        let (_dir, config) = config();
        assert_eq!(
            snapshot_command(SnapshotCommands::List, config, true).unwrap(),
            0
        );
    }
}
