//! Primary-aware assignment of registered projects to VMs.
//!
//! The registry is the single source of truth for membership, while `vm.toml`
//! records the primary project that drives `/workspace`. These helpers keep the
//! two files consistent: an assignment that fails validation is rolled back,
//! including a primary chosen as part of the same operation.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::config::Config;
use crate::vm::config::{VmConfig, VmStore};

use super::import::expand_tilde;
use super::meta::validate_project_name;
use super::registry::{ProjectEntry, ProjectKind, ProjectRegistry};

/// Result of an assignment, including whether it was the VM's first member.
#[derive(Debug, Clone)]
pub struct AssignOutcome {
    pub entry: ProjectEntry,
    pub vm: VmConfig,
    /// True when the VM had no members before this assignment.
    pub first: bool,
}

/// Assign `selector` to `vm_name`, setting `primary` when the VM has none.
///
/// Any write is rolled back when registry validation fails, so a rejected
/// assignment never leaves a half-applied export.
pub fn assign_project(
    config: &Config,
    selector: &str,
    vm_name: &str,
    read_only: bool,
) -> Result<(ProjectEntry, VmConfig)> {
    let outcome = assign_project_detailed(config, selector, vm_name, read_only)?;
    Ok((outcome.entry, outcome.vm))
}

/// Like [`assign_project`], but also reports whether this was the first
/// project assigned to the VM.
pub fn assign_project_detailed(
    config: &Config,
    selector: &str,
    vm_name: &str,
    read_only: bool,
) -> Result<AssignOutcome> {
    let store = VmStore::new(config);
    let mut vm = store.load(vm_name)?;
    let mut registry = ProjectRegistry::load(&config.state_dir)?;
    let first = registry.members(vm_name).is_empty();
    let (entry, created) = match registered_entry(config, selector)? {
        Some(entry) => (entry, false),
        None => (
            register_external_path(config, &mut registry, selector)?,
            true,
        ),
    };
    let previous = entry.clone();
    let previous_primary = vm.primary.clone();
    let assign = (|| -> Result<()> {
        registry.assign(&entry.name, vm_name, read_only)?;
        if vm.primary.is_none() {
            vm.primary = Some(entry.name.clone());
            store.save(&vm)?;
        }
        registry.validate(config, &store)?;
        Ok(())
    })();
    if let Err(err) = assign {
        let _ = registry.upsert(previous.clone());
        if created {
            let _ = registry.remove(&entry.name);
        }
        // A newly created entry can also have become primary in the same
        // operation. Restore the VM config regardless of `created`.
        if vm.primary != previous_primary {
            vm.primary = previous_primary;
            let _ = store.save(&vm);
        }
        return Err(err);
    }
    let entry = registry.get(&entry.name).cloned().unwrap_or(previous);
    Ok(AssignOutcome { entry, vm, first })
}

/// Clear a project's VM. A departing primary hands `/workspace` to the next
/// exported member, or leaves the VM primary-less when it was the last one.
pub fn unassign_project(config: &Config, selector: &str) -> Result<Option<UnassignOutcome>> {
    let store = VmStore::new(config);
    let mut registry = ProjectRegistry::load(&config.state_dir)?;
    let Some(entry) = registered_entry(config, selector)? else {
        bail!("project '{selector}' is not registered");
    };
    let Some(vm_name) = entry.vm.clone() else {
        return Ok(None);
    };
    let mut vm = store.load(&vm_name)?;
    let previous = entry.clone();
    let previous_primary = vm.primary.clone();
    let was_primary = previous_primary.as_deref() == Some(entry.name.as_str());

    registry.unassign(&entry.name)?;
    if was_primary {
        vm.primary = registry
            .exported_members(&vm_name)
            .first()
            .map(|entry| entry.name.clone());
        if let Err(err) = store.save(&vm) {
            let _ = registry.upsert(previous);
            return Err(err);
        }
    }
    if let Err(err) = registry.validate(config, &store) {
        let _ = registry.upsert(previous);
        if was_primary {
            vm.primary = previous_primary.clone();
            let _ = store.save(&vm);
        }
        return Err(err);
    }
    Ok(Some(UnassignOutcome {
        project: entry.name,
        vm: vm_name,
        was_primary,
        previous_primary,
        primary: vm.primary,
    }))
}

pub struct UnassignOutcome {
    pub project: String,
    pub vm: String,
    pub was_primary: bool,
    pub previous_primary: Option<String>,
    pub primary: Option<String>,
}

/// Assigning a path that is not yet registered adds it as an external project.
fn register_external_path(
    config: &Config,
    registry: &mut ProjectRegistry,
    selector: &str,
) -> Result<ProjectEntry> {
    let path = candidate_path(config, selector)
        .with_context(|| format!("project '{selector}' not found"))?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    let raw = canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .with_context(|| format!("{} has no usable directory name", canonical.display()))?;
    let name = validate_project_name(&raw)
        .with_context(|| format!("cannot derive a project name from {}", canonical.display()))?
        .to_string();
    if registry.get(&name).is_some() {
        bail!(
            "a project named '{name}' is already registered; import the path with \
             `hegn project import {} --name <name>` instead",
            canonical.display()
        );
    }
    let entry = ProjectEntry {
        name,
        path: canonical,
        vm: None,
        read_only: false,
        kind: ProjectKind::External,
        parent: None,
        managed: false,
        hidden: false,
        created_at: Utc::now(),
    };
    registry.upsert(entry.clone())?;
    Ok(entry)
}

fn candidate_path(config: &Config, selector: &str) -> Option<PathBuf> {
    let direct = expand_tilde(Path::new(selector));
    let candidates = if direct.is_absolute() {
        vec![direct]
    } else {
        vec![config.projects_root.join(selector), direct]
    };
    candidates.into_iter().find(|path| path.is_dir())
}

/// Resolve a registry entry by picker name, canonical path, or a path inside
/// the projects root. Returns `None` when the path is not registered.
pub fn registered_entry(config: &Config, selector: &str) -> Result<Option<ProjectEntry>> {
    let registry = ProjectRegistry::load(&config.state_dir)?;
    if let Some(entry) = registry.get(selector) {
        return Ok(Some(entry.clone()));
    }
    let direct = expand_tilde(Path::new(selector));
    let candidates = if direct.is_absolute() {
        vec![direct]
    } else {
        vec![config.projects_root.join(selector), direct]
    };
    for candidate in candidates {
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if let Some(entry) = registry.find_by_path(&canonical) {
            return Ok(Some(entry.clone()));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::Harness;

    fn test_config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.projects_root = dir.join("projects");
        config.state_dir = dir.join("state");
        config.snapshots_dir = dir.join("snapshots");
        config.vms_dir = dir.join("vms");
        config.templates_dir = dir.join("templates");
        config.profiles_dir = dir.join("profiles");
        std::fs::create_dir_all(&config.projects_root).unwrap();
        config
    }

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

    fn save_vm(config: &Config, name: &str) {
        VmStore::new(config)
            .save(&VmConfig::from_config(config, name, Some(Harness::Shell)))
            .unwrap();
    }

    #[test]
    fn assign_sets_the_first_primary_and_unassign_hands_it_to_the_next_member() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        save_vm(&config, "rust");
        for name in ["parser", "cli"] {
            let path = config.projects_root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .upsert(entry(name, &path))
                .unwrap();
        }

        let (_, vm) = assign_project(&config, "parser", "rust", false).unwrap();
        assert_eq!(vm.primary.as_deref(), Some("parser"));
        let (_, vm) = assign_project(&config, "cli", "rust", true).unwrap();
        assert_eq!(vm.primary.as_deref(), Some("parser"));
        let registry = ProjectRegistry::load(&config.state_dir).unwrap();
        assert!(registry.get("cli").unwrap().read_only);

        let outcome = unassign_project(&config, "parser").unwrap().unwrap();
        assert!(outcome.was_primary);
        assert_eq!(outcome.primary.as_deref(), Some("cli"));
        let vm = VmStore::new(&config).load("rust").unwrap();
        assert_eq!(vm.primary.as_deref(), Some("cli"));
        assert!(
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .get("parser")
                .unwrap()
                .vm
                .is_none()
        );
    }

    #[test]
    fn the_first_member_is_reported_only_for_an_empty_vm() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        save_vm(&config, "rust");
        for name in ["parser", "cli"] {
            let path = config.projects_root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            ProjectRegistry::load(&config.state_dir)
                .unwrap()
                .upsert(entry(name, &path))
                .unwrap();
        }

        let first = assign_project_detailed(&config, "parser", "rust", false).unwrap();
        assert!(first.first);
        let second = assign_project_detailed(&config, "cli", "rust", false).unwrap();
        assert!(!second.first, "the VM already has a member");
    }

    #[test]
    fn unassigning_the_last_member_clears_the_primary() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        save_vm(&config, "rust");
        let path = config.projects_root.join("parser");
        std::fs::create_dir_all(&path).unwrap();
        ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .upsert(entry("parser", &path))
            .unwrap();
        assign_project(&config, "parser", "rust", false).unwrap();

        let outcome = unassign_project(&config, "parser").unwrap().unwrap();
        assert!(outcome.was_primary);
        assert!(outcome.primary.is_none());
        assert!(
            VmStore::new(&config)
                .load("rust")
                .unwrap()
                .primary
                .is_none()
        );
        assert!(unassign_project(&config, "parser").unwrap().is_none());
    }

    #[test]
    fn a_failed_assignment_restores_a_newly_created_primary() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.max_projects_per_vm = 1;
        let existing = config.projects_root.join("one");
        std::fs::create_dir_all(&existing).unwrap();
        save_vm(&config, "rust");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("one", &existing)).unwrap();
        registry.assign("one", "rust", false).unwrap();
        assert!(
            VmStore::new(&config)
                .load("rust")
                .unwrap()
                .primary
                .is_none()
        );

        let created = dir.path().join("code/two");
        std::fs::create_dir_all(&created).unwrap();
        let err = assign_project(&config, &created.display().to_string(), "rust", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_projects_per_vm"), "{err}");

        let registry = ProjectRegistry::load(&config.state_dir).unwrap();
        assert!(
            registry.get("two").is_none(),
            "the new entry was rolled back"
        );
        assert_eq!(registry.get("one").unwrap().vm.as_deref(), Some("rust"));
        assert!(
            VmStore::new(&config)
                .load("rust")
                .unwrap()
                .primary
                .is_none(),
            "the temporary primary must be restored"
        );
    }
}
