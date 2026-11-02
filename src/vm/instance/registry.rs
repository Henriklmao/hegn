//! Multi-instance registry and project resolution.
//!
//! Each instance owns its config, runtime directory and workspace; the global
//! [`Config`] only supplies machine-wide settings.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::config::{Config, Harness};
use crate::network;
use crate::vm::instance_config::{CloneOutcome, DeleteOutcome, InstanceConfig, InstanceStore};

use super::manager::InstanceManager;
use super::process::process_matches;

/// An instance entry shown in the CLI and TUI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InstanceSummary {
    pub name: String,
    pub project: PathBuf,
    pub workspace: PathBuf,
    pub harness: Harness,
    pub ephemeral: bool,
    pub source_instance: Option<String>,
    pub created_at: DateTime<Utc>,
    pub state: String,
    pub vm_pid: Option<u32>,
    pub serial_log: Option<PathBuf>,
    pub config_path: PathBuf,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub error: Option<String>,
}

/// Multi-instance registry. Each instance owns its config, runtime directory and
/// workspace; the global [`Config`] only supplies machine-wide settings.
#[derive(Debug, Clone)]
pub struct InstanceRegistry {
    config: Config,
    store: InstanceStore,
}

impl InstanceRegistry {
    pub fn new(config: Config) -> Self {
        let store = InstanceStore::new(config.clone());
        Self { config, store }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn store(&self) -> &InstanceStore {
        &self.store
    }

    pub fn list(&self) -> Result<Vec<InstanceConfig>> {
        self.store.list()
    }

    pub fn manager(&self, instance: InstanceConfig) -> InstanceManager {
        InstanceManager::new(self.config.clone(), instance)
    }

    pub fn manager_by_name(&self, name: &str) -> Result<InstanceManager> {
        Ok(self.manager(self.store.load(name)?))
    }

    pub fn create(
        &self,
        project: &Path,
        name: Option<&str>,
        harness: Option<Harness>,
        clone: bool,
    ) -> Result<InstanceConfig> {
        self.store
            .create_from_project(project, name, harness, clone)
    }

    /// Like [`Self::create`], but returns non-fatal clone warnings for callers
    /// that can surface them (for example, git's empty-repository notice).
    pub fn create_with_warnings(
        &self,
        project: &Path,
        name: Option<&str>,
        harness: Option<Harness>,
        clone: bool,
    ) -> Result<CloneOutcome> {
        self.store
            .create_from_project_with_warnings(project, name, harness, clone)
    }

    pub fn clone_from(
        &self,
        source: &InstanceConfig,
        name: &str,
        harness: Option<Harness>,
    ) -> Result<CloneOutcome> {
        self.store.clone_from(source, name, harness)
    }

    /// Resolve an instance by name, or by project name/path.
    pub fn resolve(&self, selector: &str) -> Result<InstanceConfig> {
        if let Ok(config) = self.store.load(selector) {
            return Ok(config);
        }
        let project = resolve_project_selector(&self.config, selector)?;
        let mut matches: Vec<InstanceConfig> = self
            .store
            .list()?
            .into_iter()
            .filter(|instance| same_project(&instance.project, &project))
            .collect();
        // Prefer the non-ephemeral base instance when clones share a project.
        matches.sort_by_key(|instance| instance.ephemeral);
        matches.into_iter().next().with_context(|| {
            format!(
                "no instance for {}; create one with `hegn instance create {}`",
                project.display(),
                project.display()
            )
        })
    }

    /// Return the base instance for a project, creating it if necessary.
    pub fn ensure_for_project(&self, project: &Path, harness: Harness) -> Result<InstanceConfig> {
        let project = project
            .canonicalize()
            .with_context(|| format!("project {} does not exist", project.display()))?;
        if let Some(existing) = self
            .store
            .list()?
            .into_iter()
            .find(|instance| !instance.ephemeral && same_project(&instance.project, &project))
        {
            return Ok(existing);
        }
        let base = InstanceConfig::suggested_name(&project);
        let name = self.unique_name(&base)?;
        self.store
            .create_from_project(&project, Some(&name), Some(harness), false)
    }

    /// Find a free instance name starting at `base`.
    pub fn unique_name(&self, base: &str) -> Result<String> {
        let base = crate::session::logs::sanitize_segment(base, 48);
        let mut candidate = base.clone();
        for suffix in 1..1000 {
            if !self.store.dir(&candidate)?.exists() {
                return Ok(candidate);
            }
            candidate = format!("{base}-{suffix}");
        }
        bail!("could not allocate a free instance name for '{base}'")
    }

    pub fn summaries(&self) -> Result<Vec<InstanceSummary>> {
        self.store
            .list()?
            .into_iter()
            .map(|instance| Ok(self.summary(&instance)))
            .collect()
    }

    pub fn summary(&self, instance: &InstanceConfig) -> InstanceSummary {
        let manager = self.manager(instance.clone());
        let paths = manager.paths();
        let session = manager.session();
        let vm_pid = session.as_ref().map(|session| session.vm_pid);
        let binary = session
            .as_ref()
            .and_then(|session| session.vm_binary.clone())
            .unwrap_or_else(|| self.config.vm_binary());
        let alive = vm_pid.is_some_and(|pid| process_matches(pid, &binary));
        let state = if alive { "running" } else { "stopped" }.to_string();
        InstanceSummary {
            name: instance.name.clone(),
            project: instance.project.clone(),
            workspace: instance.workspace.clone(),
            harness: instance.harness,
            ephemeral: instance.ephemeral,
            source_instance: instance.source_instance.clone(),
            created_at: instance.created_at,
            state,
            vm_pid: vm_pid.filter(|pid| process_matches(*pid, &binary)),
            serial_log: session.and_then(|session| session.serial_log),
            config_path: paths
                .run_dir
                .parent()
                .unwrap_or(Path::new(""))
                .join("hegn.toml"),
            vcpus: instance.vcpus,
            memory_mib: instance.memory_mib,
            error: None,
        }
    }

    pub fn stop(&self, name: &str) -> Result<()> {
        self.manager_by_name(name)?.stop()
    }

    /// Delete an instance. The source project of a normal instance is never
    /// deleted; only an in-instance clone can be removed or kept.
    pub fn delete(&self, name: &str, delete_files: bool) -> Result<DeleteOutcome> {
        let manager = self.manager_by_name(name)?;
        if manager.is_running() {
            manager.stop()?;
        }
        network::release(&self.config, name);
        self.store.delete(name, delete_files)
    }
}

fn resolve_project_selector(config: &Config, selector: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(selector);
    if direct.is_dir() {
        return Ok(direct.canonicalize().unwrap_or(direct));
    }
    let candidate = config.projects_root.join(selector);
    if candidate.is_dir() {
        return Ok(candidate.canonicalize().unwrap_or(candidate));
    }
    bail!(
        "no instance named '{selector}' and no project at {} or {}",
        direct.display(),
        candidate.display()
    )
}

/// Whether two project paths refer to the same directory.
///
/// The TUI discovers projects as `<projects_root>/<name>` while the CLI
/// canonicalizes them, so a symlinked projects root would otherwise look like a
/// different project (and trigger a bogus "instance already running" error).
pub fn same_project(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::instance::test_support::config_with_assets;

    #[test]
    fn same_project_compares_canonical_paths() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(same_project(&real, &link));
        assert!(!same_project(&real, &dir.path().join("missing")));
    }

    #[test]
    fn registry_allows_multiple_instances_and_resolves_projects() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let registry = InstanceRegistry::new(config.clone());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let base = registry
            .ensure_for_project(&project, Harness::Shell)
            .unwrap();
        assert_eq!(base.name, "demo");
        assert!(!base.ephemeral);
        // A second call reuses the base instance.
        assert_eq!(
            registry
                .ensure_for_project(&project, Harness::Shell)
                .unwrap()
                .name,
            "demo"
        );
        assert_eq!(registry.resolve("demo").unwrap().name, "demo");
        assert_eq!(
            registry.resolve(project.to_str().unwrap()).unwrap().name,
            "demo"
        );
        let clone = registry
            .clone_from(&base, "demo-agent-1", Some(Harness::Codex))
            .unwrap()
            .instance;
        assert!(clone.ephemeral);
        assert_eq!(registry.list().unwrap().len(), 2);
        let summaries = registry.summaries().unwrap();
        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().all(|summary| summary.state == "stopped"));
        assert_eq!(registry.unique_name("demo").unwrap(), "demo-1");
    }

    #[test]
    fn summaries_are_empty_when_no_instances_exist() {
        let dir = tempfile::tempdir().unwrap();
        let registry = InstanceRegistry::new(config_with_assets(dir.path()));
        assert!(!registry.config().instances_dir.exists());
        assert!(registry.list().unwrap().is_empty());
        assert!(registry.summaries().unwrap().is_empty());
    }

    #[test]
    fn resolve_names_both_lookup_locations_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let registry = InstanceRegistry::new(config_with_assets(dir.path()));
        let err = registry.resolve("missing").unwrap_err().to_string();
        assert!(err.contains("no instance named 'missing'"), "{err}");
        assert!(err.contains("projects"), "{err}");
    }

    #[test]
    fn ensure_for_project_requires_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let registry = InstanceRegistry::new(config_with_assets(dir.path()));
        let missing = dir.path().join("missing-project");
        let err = registry
            .ensure_for_project(&missing, Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn resolve_prefers_the_base_instance_over_an_ephemeral_clone() {
        let dir = tempfile::tempdir().unwrap();
        let registry = InstanceRegistry::new(config_with_assets(dir.path()));
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let base = registry
            .ensure_for_project(&project, Harness::Shell)
            .unwrap();
        // The clone sorts before the base alphabetically; resolve must still
        // return the non-ephemeral instance.
        let clone = registry
            .clone_from(&base, "aaa-clone", Some(Harness::Codex))
            .unwrap()
            .instance;
        assert!(clone.ephemeral);

        let resolved = registry.resolve(project.to_str().unwrap()).unwrap();
        assert_eq!(resolved.name, "demo");
        assert!(!resolved.ephemeral);
    }

    #[test]
    fn summary_hides_a_recycled_vm_pid() {
        use chrono::Utc;

        use crate::vm::instance::SessionRecord;

        let dir = tempfile::tempdir().unwrap();
        let registry = InstanceRegistry::new(config_with_assets(dir.path()));
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let instance = registry
            .ensure_for_project(&project, Harness::Shell)
            .unwrap();
        let manager = registry.manager(instance.clone());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: project.clone(),
            workspace: project.clone(),
            instance: instance.name.clone(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid: std::process::id(),
            // The recorded binary is gone, so a recycled pid must not be
            // reported as a running VM.
            vm_binary: Some(dir.path().join("missing-cloud-hypervisor")),
            relay_pid: 0,
            virtiofs_pid: 0,
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();

        let summary = registry.summary(&instance);
        assert_eq!(summary.state, "stopped");
        assert_eq!(summary.vm_pid, None);
        assert_eq!(summary.name, "demo");
        assert_eq!(summary.vcpus, instance.vcpus);
    }

    #[test]
    fn same_project_handles_identical_and_unresolvable_paths() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(same_project(&missing, &missing));
        assert!(!same_project(&missing, &dir.path().join("other-missing")));
        assert!(!same_project(dir.path(), &missing));
    }
}
