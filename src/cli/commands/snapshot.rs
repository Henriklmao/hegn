//! `hegn snapshot` handler: list, create, restore, delete and show VM
//! snapshots.

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::session::snapshot::{SnapshotMeta, SnapshotStore};
use crate::vm::registry::VmRegistry;

use super::instance::snapshot_vm;
use crate::cli::args::SnapshotCommands;
use crate::cli::output::{print_snapshot, runtime};

/// Resolve the primary `--vm` flag, accepting the deprecated `--instance`
/// spelling with a single warning.
fn vm_selector(vm: Option<String>, instance: Option<String>) -> Option<String> {
    match instance {
        Some(instance) => {
            eprintln!("warning: `--instance` is deprecated for snapshots; use `--vm <vm>` instead");
            Some(instance)
        }
        None => vm,
    }
}

fn selected_snapshots(store: &SnapshotStore, vm: Option<&str>) -> Result<Vec<SnapshotMeta>> {
    match vm {
        Some(vm) => store.list_for_vm(vm),
        None => store.list(),
    }
}

/// VM that owns a snapshot, or an error when the metadata is legacy or has no
/// owner. The historical messages are kept so scripts and users see the same
/// remediation.
fn snapshot_owner(meta: &SnapshotMeta, name: &str) -> Result<String> {
    if let Some(label) = meta.legacy_label() {
        bail!("snapshot '{name}' is {label}; take a new snapshot with `hegn snapshot create`");
    }
    Ok(meta
        .vm()
        .context("snapshot has no VM/project metadata")?
        .to_owned())
}

/// Reject a `--vm` that names a different VM than the snapshot records.
fn verify_owner(
    registry: &VmRegistry,
    requested: Option<String>,
    owner: &str,
    name: &str,
) -> Result<()> {
    if let Some(requested) = requested {
        let vm = registry.resolve(&requested)?;
        if vm.name != owner {
            bail!(
                "snapshot '{name}' belongs to VM '{owner}', not '{}'",
                vm.name
            );
        }
    }
    Ok(())
}

pub(crate) fn snapshot_command(
    command: SnapshotCommands,
    config: Config,
    json: bool,
) -> Result<i32> {
    let store = SnapshotStore::new(&config.snapshots_dir);
    let registry = VmRegistry::new(config.clone());
    match command {
        SnapshotCommands::List { vm, instance } => {
            let vm = vm_selector(vm, instance);
            let snapshots = selected_snapshots(&store, vm.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else if snapshots.is_empty() {
                println!("No snapshots in {}.", store.root().display());
            } else {
                println!(
                    "{:<32} {:<20} {:<18} {:>10} {:<10}",
                    "NAME", "VM", "CREATED", "SIZE", "FORMAT"
                );
                for snapshot in snapshots {
                    println!(
                        "{:<32} {:<20} {:<18} {:>10} {:<10}",
                        snapshot.name,
                        snapshot.vm().unwrap_or("-"),
                        snapshot.created_at.format("%Y-%m-%d %H:%M:%S"),
                        snapshot.size_label(),
                        if snapshot.is_legacy() { "legacy" } else { "v2" }
                    );
                }
            }
            Ok(0)
        }
        SnapshotCommands::Create { name, vm, instance } => {
            let Some(selector) = vm_selector(vm, instance) else {
                bail!("snapshot create needs a VM: pass --vm <vm> (or a project assigned to a VM)");
            };
            let vm = snapshot_vm(&registry, Some(selector))?;
            let suggested = SnapshotStore::suggested_name(&vm.name);
            let name = name.unwrap_or(suggested);
            let meta = runtime()?.block_on(registry.manager(vm)?.create_snapshot(&name))?;
            print_snapshot(&meta, json)?;
            Ok(0)
        }
        SnapshotCommands::Restore { name, vm, instance } => {
            let meta = store.load(&name)?;
            let owner = snapshot_owner(&meta, &name)?;
            let vm = match vm_selector(vm, instance) {
                Some(selector) => {
                    let vm = registry.resolve(&selector)?;
                    if vm.name != owner {
                        bail!(
                            "snapshot '{name}' belongs to VM '{owner}', not '{}'",
                            vm.name
                        );
                    }
                    vm
                }
                None => registry.store().load(&owner)?,
            };
            runtime()?.block_on(registry.manager(vm)?.restore_snapshot(&meta))?;
            if json {
                println!("{}", serde_json::json!({"restored": meta.name}));
            } else {
                println!("Restored snapshot '{}'.", meta.name);
            }
            Ok(0)
        }
        SnapshotCommands::Delete { name, vm, instance } => {
            if let Some(requested) = vm_selector(vm, instance) {
                let meta = store.load(&name)?;
                let owner = match (meta.legacy_label(), meta.vm()) {
                    (None, Some(owner)) => owner.to_owned(),
                    _ => bail!("snapshot '{name}' has no VM owner; omit --vm to delete it"),
                };
                verify_owner(&registry, Some(requested), &owner, &name)?;
            }
            // Without `--vm`, delete what the user named. Legacy, ownerless
            // and corrupt snapshots are still removable.
            store.delete(&name)?;
            if json {
                println!("{}", serde_json::json!({"deleted": name}));
            } else {
                println!("Deleted snapshot '{name}'.");
            }
            Ok(0)
        }
        SnapshotCommands::Show { name, vm, instance } => {
            let meta = store.load(&name)?;
            if let Some(requested) = vm_selector(vm, instance) {
                match (meta.legacy_label(), meta.vm()) {
                    (None, Some(owner)) => {
                        verify_owner(&registry, Some(requested), owner, &name)?;
                    }
                    _ => eprintln!(
                        "warning: snapshot '{name}' has no verifiable VM owner; showing it anyway"
                    ),
                }
            }
            print_snapshot(&meta, json)?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;
    use crate::session::snapshot::{SNAPSHOT_VERSION, SnapshotBoot, compute_fingerprint};
    use chrono::Utc;
    use std::fs;

    fn write_snapshot(store: &SnapshotStore, name: &str, vm: Option<&str>) -> SnapshotMeta {
        let paths = store.prepare(name).unwrap();
        fs::write(paths.dir.join("config.json"), b"{}").unwrap();
        fs::write(paths.dir.join("state.json"), b"{}").unwrap();
        fs::write(paths.dir.join("memory-ranges"), b"{}").unwrap();
        let boot = SnapshotBoot::default();
        let meta = SnapshotMeta {
            name: name.into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: paths.final_dir.clone(),
            vm: vm.map(str::to_owned),
            project: None,
            size_bytes: 0,
            note: None,
            version: SNAPSHOT_VERSION,
            boot_fingerprint: Some(compute_fingerprint(&[], &boot)),
            membership: Vec::new(),
            boot: Some(boot),
        };
        store.save_meta(&meta).unwrap();
        meta
    }

    fn write_legacy_snapshot(store: &SnapshotStore, name: &str, vm: Option<&str>) {
        let paths = store.prepare(name).unwrap();
        let meta = SnapshotMeta {
            name: name.into(),
            dir: paths.final_dir.clone(),
            vm: vm.map(str::to_owned),
            version: 1,
            ..SnapshotMeta::default()
        };
        store.save_meta(&meta).unwrap();
    }

    fn save_vm(config: &Config, name: &str) {
        use crate::config::Harness;
        use crate::vm::config::{VmConfig, VmStore};

        let vm = VmConfig::from_config(config, name, Some(Harness::Shell));
        VmStore::new(config).save(&vm).unwrap();
    }

    #[test]
    fn snapshot_list_empty() {
        let (_dir, config) = config();
        assert_eq!(
            snapshot_command(
                SnapshotCommands::List {
                    vm: None,
                    instance: None,
                },
                config,
                true,
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn selected_snapshots_filters_by_vm_and_lists_everything_without_one() {
        let (_dir, config) = config();
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_snapshot(&store, "rust-one", Some("rust"));
        write_snapshot(&store, "rust-two", Some("rust"));
        write_snapshot(&store, "python-one", Some("python"));
        write_snapshot(&store, "orphan", None);

        assert_eq!(selected_snapshots(&store, None).unwrap().len(), 4);
        let rust = selected_snapshots(&store, Some("rust")).unwrap();
        assert_eq!(rust.len(), 2);
        assert!(rust.iter().all(|snapshot| snapshot.vm() == Some("rust")));
        assert!(
            selected_snapshots(&store, Some("golang"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn create_without_vm_fails_with_a_clear_hint() {
        let (_dir, config) = config();
        let err = snapshot_command(
            SnapshotCommands::Create {
                name: None,
                vm: None,
                instance: None,
            },
            config,
            true,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("--vm <vm>"), "{message}");
        assert!(message.contains("project assigned to a VM"), "{message}");
    }

    #[test]
    fn deprecated_instance_flag_maps_to_the_same_vm() {
        assert_eq!(
            vm_selector(None, Some("rust".into())),
            Some("rust".to_owned())
        );
        assert_eq!(
            vm_selector(Some("rust".into()), None),
            Some("rust".to_owned())
        );
    }

    #[test]
    fn restore_refuses_legacy_snapshots() {
        let (_dir, config) = config();
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_legacy_snapshot(&store, "legacy", Some("rust"));

        let err = snapshot_command(
            SnapshotCommands::Restore {
                name: "legacy".into(),
                vm: None,
                instance: None,
            },
            config,
            true,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("legacy"), "{err:#}");
    }

    #[test]
    fn restore_refuses_an_owner_mismatch() {
        let (_dir, config) = config();
        save_vm(&config, "rust");
        save_vm(&config, "python");
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_snapshot(&store, "demo", Some("rust"));

        let err = snapshot_command(
            SnapshotCommands::Restore {
                name: "demo".into(),
                vm: Some("python".into()),
                instance: None,
            },
            config,
            true,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("belongs to VM 'rust'"), "{message}");
        assert!(message.contains("not 'python'"), "{message}");
    }

    #[test]
    fn delete_without_vm_removes_legacy_ownerless_and_corrupt_snapshots() {
        let (_dir, config) = config();
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_legacy_snapshot(&store, "legacy", Some("rust"));
        write_snapshot(&store, "orphan", None);
        let corrupt = config.snapshots_dir.join("corrupt");
        fs::create_dir_all(&corrupt).unwrap();
        fs::write(corrupt.join("snapshot.json"), b"not json").unwrap();

        for name in ["legacy", "orphan", "corrupt"] {
            assert_eq!(
                snapshot_command(
                    SnapshotCommands::Delete {
                        name: name.into(),
                        vm: None,
                        instance: None,
                    },
                    config.clone(),
                    true,
                )
                .unwrap(),
                0
            );
            assert!(
                !config.snapshots_dir.join(name).exists(),
                "{name} must be deleted"
            );
        }
    }

    #[test]
    fn delete_with_vm_refuses_snapshots_without_a_verifiable_owner() {
        let (_dir, config) = config();
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_legacy_snapshot(&store, "legacy", Some("rust"));
        write_snapshot(&store, "orphan", None);

        for name in ["legacy", "orphan"] {
            let err = snapshot_command(
                SnapshotCommands::Delete {
                    name: name.into(),
                    vm: Some("rust".into()),
                    instance: None,
                },
                config.clone(),
                true,
            )
            .unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                format!("snapshot '{name}' has no VM owner; omit --vm to delete it")
            );
            assert!(
                config.snapshots_dir.join(name).exists(),
                "{name} must be preserved"
            );
        }
    }

    #[test]
    fn delete_with_vm_verifies_ownership() {
        let (_dir, config) = config();
        save_vm(&config, "rust");
        save_vm(&config, "python");
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_snapshot(&store, "demo", Some("rust"));

        let err = snapshot_command(
            SnapshotCommands::Delete {
                name: "demo".into(),
                vm: Some("python".into()),
                instance: None,
            },
            config.clone(),
            true,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("belongs to VM 'rust'"),
            "{err:#}"
        );
        assert!(config.snapshots_dir.join("demo").exists());

        assert_eq!(
            snapshot_command(
                SnapshotCommands::Delete {
                    name: "demo".into(),
                    vm: Some("rust".into()),
                    instance: None,
                },
                config,
                true,
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn show_without_vm_prints_legacy_and_ownerless_snapshots() {
        let (_dir, config) = config();
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_legacy_snapshot(&store, "legacy", Some("rust"));
        write_snapshot(&store, "orphan", None);

        for name in ["legacy", "orphan"] {
            assert_eq!(
                snapshot_command(
                    SnapshotCommands::Show {
                        name: name.into(),
                        vm: None,
                        instance: None,
                    },
                    config.clone(),
                    true,
                )
                .unwrap(),
                0
            );
        }
    }

    #[test]
    fn show_with_vm_warns_but_still_prints_unverifiable_snapshots() {
        let (_dir, config) = config();
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_legacy_snapshot(&store, "legacy", Some("rust"));
        write_snapshot(&store, "orphan", None);

        for name in ["legacy", "orphan"] {
            assert_eq!(
                snapshot_command(
                    SnapshotCommands::Show {
                        name: name.into(),
                        vm: Some("rust".into()),
                        instance: None,
                    },
                    config.clone(),
                    true,
                )
                .unwrap(),
                0
            );
        }
    }

    #[test]
    fn show_with_vm_refuses_an_owner_mismatch() {
        let (_dir, config) = config();
        save_vm(&config, "rust");
        save_vm(&config, "python");
        let store = SnapshotStore::new(&config.snapshots_dir);
        write_snapshot(&store, "demo", Some("rust"));

        let err = snapshot_command(
            SnapshotCommands::Show {
                name: "demo".into(),
                vm: Some("python".into()),
                instance: None,
            },
            config,
            true,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("belongs to VM 'rust'"),
            "{err:#}"
        );
    }
}
