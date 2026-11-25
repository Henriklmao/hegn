//! Private project registry (`<state_dir>/projects.toml`).
//!
//! The registry is the single source of truth for which host directories are
//! exported to which VM, and whether they are writable. It lives outside every
//! exported directory and is private (`0600`, no symlink following, atomic
//! writes, exclusive `flock` for writers), because a guest must never be able to
//! widen its own export.
//!
//! Project membership is deliberately not stored in `vm.toml`; a project has at
//! most one VM and a VM has zero or more projects.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::shared::fs::create_private_dir;
use crate::vm::config::VmStore;

use super::discovery::absolute_or_canonical;
use super::import::validate_export_source;
use super::meta::validate_project_name;

/// Name of the registry file inside `state_dir`.
pub const REGISTRY_FILE: &str = "projects.toml";
/// Writer lock file used by every mutation.
const REGISTRY_LOCK: &str = "projects.lock";
const REGISTRY_VERSION: u32 = 2;

/// How a workspace relates to its project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectKind {
    /// A directory owned by the user, exported on its own.
    #[default]
    External,
    /// A hegn-created clone below `<vms_dir>/<vm>/workspaces`.
    Clone,
    /// A git worktree nested inside its parent; not exported separately.
    Worktree,
}

/// One registered project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectEntry {
    /// Stable picker name.
    pub name: String,
    /// Canonical host directory.
    pub path: PathBuf,
    /// VM this project is assigned to; `None` means unassigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub kind: ProjectKind,
    /// Picker name of the parent project for a clone or worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// True only for hegn-created clone directories.
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub hidden: bool,
    pub created_at: DateTime<Utc>,
}

/// On-disk document. TOML renders the `project` array as `[[project]]`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryDocument {
    version: u32,
    #[serde(default)]
    project: Vec<ProjectEntry>,
}

/// Validate one exported member recorded in snapshot metadata before a restore
/// hands its path to `virtiofsd`.
///
/// Snapshot files are host-private, but a corrupted or edited `snapshot.json`
/// must not be able to redirect an export at `/`, `$HOME`, hegn state, the
/// snapshots or VMs directories, or another VM's workspace. This re-applies the
/// same rules as a boot-time registry validation:
///
/// - a `worktree` is never exported separately and is always rejected,
/// - an `external` path must pass [`validate_export_source`] (the full
///   protected-path check),
/// - a `clone` must be a real directory at exactly
///   `<vms_dir>/<vm>/workspaces/<name>`. Clones intentionally live below
///   `vms_dir`, so they bypass the protected-path check that would reject
///   every managed workspace; their exact location is enforced instead.
///
/// Returns the canonical path.
pub fn validate_recorded_export(
    config: &Config,
    vm: &str,
    name: &str,
    kind: ProjectKind,
    path: &Path,
) -> Result<PathBuf> {
    match kind {
        ProjectKind::Worktree => bail!(
            "recorded member '{name}' at {} is a worktree; worktrees ride their parent's share \
             and are never exported separately",
            path.display()
        ),
        ProjectKind::External => {
            validate_export_source(path, &config.projects_root, &config.protected_paths())
                .with_context(|| format!("recorded external member '{name}'"))
        }
        ProjectKind::Clone => {
            let clean_name = validate_project_name(name)
                .with_context(|| format!("recorded clone member '{name}'"))?;
            let workspaces = VmStore::new(config).workspaces_dir(vm)?;
            let expected = absolute_or_canonical(&workspaces).join(clean_name);
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "recorded clone member '{name}' path {} is a symlink; managed clones must be \
                     real directories",
                    path.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "recorded clone member '{name}' path {} is not a directory",
                    path.display()
                ),
                Ok(_) => {}
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "failed to inspect recorded clone member '{}' at {}",
                            name,
                            path.display()
                        )
                    });
                }
            }
            let canonical = path.canonicalize().with_context(|| {
                format!(
                    "failed to resolve recorded clone member '{name}' at {}",
                    path.display()
                )
            })?;
            if canonical != expected {
                bail!(
                    "recorded clone member '{name}' must live at {} inside VM '{vm}', but its \
                     path {} resolves to {}",
                    expected.display(),
                    path.display(),
                    canonical.display()
                );
            }
            Ok(canonical)
        }
    }
}

/// Snapshot of the private project registry bound to a state directory.
#[derive(Debug, Clone)]
pub struct ProjectRegistry {
    state_dir: PathBuf,
    entries: Vec<ProjectEntry>,
}

impl ProjectRegistry {
    /// Load the registry. A missing file yields an empty version-2 registry.
    pub fn load(state_dir: impl Into<PathBuf>) -> Result<Self> {
        let state_dir = state_dir.into();
        let entries = read_entries(&state_dir)?;
        Ok(Self { state_dir, entries })
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn path(&self) -> PathBuf {
        self.state_dir.join(REGISTRY_FILE)
    }

    pub fn entries(&self) -> &[ProjectEntry] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&ProjectEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Look up an entry by its canonical path.
    pub fn find_by_path(&self, canonical: &Path) -> Option<&ProjectEntry> {
        self.entries.iter().find(|entry| entry.path == canonical)
    }

    /// Projects assigned to `vm`, ordered by canonical path.
    ///
    /// Includes worktrees. A worktree lives inside its parent's share and is
    /// not exported separately; use [`Self::exported_members`] for the paths
    /// that get their own `virtiofsd` and `--fs` device.
    pub fn members(&self, vm: &str) -> Vec<&ProjectEntry> {
        let mut members: Vec<&ProjectEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.vm.as_deref() == Some(vm))
            .collect();
        members.sort_by(|a, b| a.path.cmp(&b.path));
        members
    }

    /// Projects assigned to `vm` that are exported separately, ordered by
    /// canonical path (boot order).
    pub fn exported_members(&self, vm: &str) -> Vec<&ProjectEntry> {
        let mut members: Vec<&ProjectEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.vm.as_deref() == Some(vm) && entry.kind != ProjectKind::Worktree)
            .collect();
        members.sort_by(|a, b| a.path.cmp(&b.path));
        members
    }

    /// Insert or replace an entry by name.
    ///
    /// The path must be an existing directory; it is canonicalized before it is
    /// stored. A second entry may not use the same canonical path.
    pub fn upsert(&mut self, mut entry: ProjectEntry) -> Result<()> {
        validate_entry_name(&entry.name)?;
        let canonical = entry.path.canonicalize().with_context(|| {
            format!(
                "project '{}' path {} does not exist or is inaccessible",
                entry.name,
                entry.path.display()
            )
        })?;
        if !canonical.is_dir() {
            bail!("project '{}' path is not a directory", entry.name);
        }
        entry.path = canonical;
        self.mutate(move |entries| {
            if entries
                .iter()
                .any(|existing| existing.name != entry.name && existing.path == entry.path)
            {
                bail!(
                    "project path {} is already registered",
                    entry.path.display()
                );
            }
            match entries
                .iter_mut()
                .find(|existing| existing.name == entry.name)
            {
                Some(existing) => *existing = entry,
                None => entries.push(entry),
            }
            Ok(())
        })
    }

    /// Assign a project to a VM. Also moves it from its previous VM.
    ///
    /// Worktrees inherit their parent's membership and cannot be assigned
    /// independently.
    pub fn assign(&mut self, name: &str, vm: &str, read_only: bool) -> Result<()> {
        self.mutate(|entries| {
            let index = entries
                .iter()
                .position(|entry| entry.name == name)
                .with_context(|| format!("unknown project '{name}'"))?;
            if entries[index].kind == ProjectKind::Worktree {
                bail!(
                    "worktree '{name}' inherits its parent's VM and cannot be assigned independently"
                );
            }
            let has_worktrees = entries
                .iter()
                .any(|entry| entry.kind == ProjectKind::Worktree && entry.parent.as_deref() == Some(name));
            if read_only && has_worktrees {
                bail!("cannot make '{name}' read-only while it has worktrees");
            }
            entries[index].vm = Some(vm.to_string());
            entries[index].read_only = read_only;
            for child in entries.iter_mut().filter(|entry| {
                entry.kind == ProjectKind::Worktree && entry.parent.as_deref() == Some(name)
            }) {
                child.vm = Some(vm.to_string());
            }
            Ok(())
        })
    }

    /// Clear a project's VM assignment.
    pub fn unassign(&mut self, name: &str) -> Result<()> {
        self.mutate(|entries| {
            let index = entries
                .iter()
                .position(|entry| entry.name == name)
                .with_context(|| format!("unknown project '{name}'"))?;
            if entries[index].kind == ProjectKind::Worktree {
                bail!(
                    "worktree '{name}' inherits its parent's VM and cannot be unassigned independently"
                );
            }
            entries[index].vm = None;
            entries[index].read_only = false;
            for child in entries.iter_mut().filter(|entry| {
                entry.kind == ProjectKind::Worktree && entry.parent.as_deref() == Some(name)
            }) {
                child.vm = None;
            }
            Ok(())
        })
    }

    /// Remove an entry. Returns whether it existed.
    ///
    /// Removing a parent while worktrees still name it is refused; remove the
    /// worktrees first so no dangling parent reference can be written.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        self.mutate(|entries| {
            if entries.iter().any(|entry| {
                entry.kind == ProjectKind::Worktree
                    && entry.parent.as_deref() == Some(name)
                    && entry.name != name
            }) {
                bail!("cannot remove '{name}': worktrees still name it as their parent");
            }
            let before = entries.len();
            entries.retain(|entry| entry.name != name);
            Ok(before != entries.len())
        })
    }

    /// Validate every registry invariant that depends on global config and the
    /// VM store.
    pub fn validate(&self, config: &Config, vms: &VmStore) -> Result<()> {
        self.check_unique_names_and_paths()?;
        self.check_paths(config)?;
        self.check_worktrees(vms)?;
        self.check_nesting()?;
        self.check_vms(config, vms)
    }

    fn check_unique_names_and_paths(&self) -> Result<()> {
        let mut names = HashSet::new();
        let mut paths = HashSet::new();
        for entry in &self.entries {
            if !names.insert(entry.name.as_str()) {
                bail!("duplicate project name '{}'", entry.name);
            }
            if !paths.insert(entry.path.clone()) {
                bail!(
                    "duplicate project path {} (projects must have unique paths)",
                    entry.path.display()
                );
            }
        }
        Ok(())
    }

    fn check_paths(&self, config: &Config) -> Result<()> {
        for entry in &self.entries {
            validate_entry_name(&entry.name)?;
            if entry.path.as_os_str().is_empty() || !entry.path.is_absolute() {
                bail!("project '{}' path must be absolute", entry.name);
            }
            let canonical = entry.path.canonicalize().with_context(|| {
                format!(
                    "project '{}' path {} does not exist or is inaccessible",
                    entry.name,
                    entry.path.display()
                )
            })?;
            if canonical != entry.path {
                bail!(
                    "project '{}' path {} is not canonical (expected {})",
                    entry.name,
                    entry.path.display(),
                    canonical.display()
                );
            }
            if !canonical.is_dir() {
                bail!("project '{}' path is not a directory", entry.name);
            }
            if entry.kind == ProjectKind::Worktree {
                // A worktree is not exported separately; its parent carries the
                // safety check and the path must live below it.
                continue;
            }
            let protected = if entry.kind == ProjectKind::Clone {
                // Managed clones intentionally live below `vms_dir`; their exact
                // location is enforced separately. Everything else hegn owns
                // stays protected.
                config
                    .protected_paths()
                    .into_iter()
                    .filter(|path| path != &config.vms_dir)
                    .collect()
            } else {
                config.protected_paths()
            };
            validate_export_source(&entry.path, &config.projects_root, &protected)
                .with_context(|| format!("project '{}'", entry.name))?;
        }
        Ok(())
    }

    fn check_worktrees(&self, vms: &VmStore) -> Result<()> {
        for entry in &self.entries {
            match entry.kind {
                ProjectKind::Worktree => {
                    let parent_name = entry.parent.as_deref().with_context(|| {
                        format!("worktree '{}' must name its parent project", entry.name)
                    })?;
                    let parent = self.get(parent_name).with_context(|| {
                        format!(
                            "worktree '{}' names unknown parent '{parent_name}'",
                            entry.name
                        )
                    })?;
                    if entry.vm != parent.vm {
                        bail!(
                            "worktree '{}' must inherit its parent's VM ('{}' has {:?}, worktree has {:?})",
                            entry.name,
                            parent.name,
                            parent.vm,
                            entry.vm
                        );
                    }
                    if parent.read_only {
                        bail!(
                            "worktree '{}' cannot live in read-only parent '{}'",
                            entry.name,
                            parent.name
                        );
                    }
                    if !entry.path.starts_with(&parent.path) || entry.path == parent.path {
                        bail!(
                            "worktree '{}' must live inside its parent {}",
                            entry.name,
                            parent.path.display()
                        );
                    }
                    if entry.managed {
                        bail!("worktree '{}' cannot be a managed clone", entry.name);
                    }
                }
                ProjectKind::Clone => {
                    let vm = entry.vm.as_deref().with_context(|| {
                        format!("managed clone '{}' must be assigned to a VM", entry.name)
                    })?;
                    if !entry.managed {
                        bail!("clone '{}' must be managed", entry.name);
                    }
                    let expected = vms
                        .dir(vm)?
                        .canonicalize()
                        .with_context(|| format!("VM '{vm}' directory does not exist"))?
                        .join("workspaces")
                        .join(&entry.name);
                    if entry.path != expected {
                        bail!(
                            "managed clone '{}' must live at {} (got {})",
                            entry.name,
                            expected.display(),
                            entry.path.display()
                        );
                    }
                }
                ProjectKind::External => {
                    if entry.managed {
                        bail!("only clones may be managed ('{}')", entry.name);
                    }
                }
            }
            // A clone records the project it was forked from for lineage and
            // display; it is not a worktree and is never nested in it. Only an
            // external project has no parent to name.
            if entry.kind == ProjectKind::External && entry.parent.is_some() {
                bail!("external project '{}' must not name a parent", entry.name);
            }
        }
        Ok(())
    }

    fn check_nesting(&self) -> Result<()> {
        for (index, first) in self.entries.iter().enumerate() {
            for second in self.entries.iter().skip(index + 1) {
                if !first.path.starts_with(&second.path) && !second.path.starts_with(&first.path) {
                    continue;
                }
                // Unassigned projects are not VM members yet, so nesting is not
                // an export conflict. Validation runs again once assigned.
                if first.vm.is_none() || second.vm.is_none() {
                    continue;
                }
                let (nested, ancestor) = if first.path.starts_with(&second.path) {
                    (first, second)
                } else {
                    (second, first)
                };
                let allowed = nested.kind == ProjectKind::Worktree
                    && nested.parent.as_deref() == Some(ancestor.name.as_str())
                    && nested.vm == ancestor.vm;
                if !allowed {
                    bail!(
                        "project '{}' is nested inside '{}'; only a worktree may be nested in its named parent",
                        nested.name,
                        ancestor.name
                    );
                }
            }
        }
        Ok(())
    }

    fn check_vms(&self, config: &Config, vms: &VmStore) -> Result<()> {
        let mut exported: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in &self.entries {
            let Some(vm) = entry.vm.as_deref() else {
                continue;
            };
            vms.load(vm).with_context(|| {
                format!("project '{}' is assigned to unknown VM '{vm}'", entry.name)
            })?;
            if entry.kind != ProjectKind::Worktree {
                *exported.entry(vm).or_insert(0) += 1;
            }
        }
        for (vm, count) in exported {
            if count > config.max_projects_per_vm as usize {
                bail!(
                    "VM '{vm}' exports {count} projects, above max_projects_per_vm = {}",
                    config.max_projects_per_vm
                );
            }
        }
        Ok(())
    }

    /// Read-modify-write under the exclusive writer lock.
    fn mutate<T>(&mut self, change: impl FnOnce(&mut Vec<ProjectEntry>) -> Result<T>) -> Result<T> {
        let _lock = RegistryLock::acquire(&self.state_dir)?;
        let mut entries = read_entries(&self.state_dir)?;
        let value = change(&mut entries)?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        write_entries(&self.state_dir, &entries)?;
        self.entries = entries;
        Ok(value)
    }
}

/// Exclusive advisory lock; dropped when the mutation finishes.
struct RegistryLock {
    _file: fs::File,
}

impl RegistryLock {
    fn acquire(state_dir: &Path) -> Result<Self> {
        create_private_dir(state_dir)
            .with_context(|| format!("failed to secure {}", state_dir.display()))?;
        let path = state_dir.join(REGISTRY_LOCK);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        // Safety: `file` is a valid open descriptor for a private lock file.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if locked != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

/// Validate a picker name without silently trimming it.
///
/// `validate_project_name` trims before validating, which is right for user
/// input but would let a registry entry keep surrounding whitespace while
/// lookups by the clean name fail.
fn validate_entry_name(name: &str) -> Result<()> {
    let trimmed = validate_project_name(name)?;
    if trimmed != name {
        bail!("project name must not start or end with whitespace");
    }
    Ok(())
}

fn read_entries(state_dir: &Path) -> Result<Vec<ProjectEntry>> {
    let path = state_dir.join(REGISTRY_FILE);
    match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", path.display()));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing symlinked project registry {}", path.display());
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("project registry {} is not a regular file", path.display());
        }
        Ok(_) => {}
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read project registry {}", path.display()))?;
    let document: RegistryDocument = toml::from_str(&raw)
        .with_context(|| format!("failed to parse project registry {}", path.display()))?;
    if document.version != REGISTRY_VERSION {
        bail!(
            "unsupported project registry version {} in {} (expected {REGISTRY_VERSION})",
            document.version,
            path.display()
        );
    }
    Ok(document.project)
}

fn write_entries(state_dir: &Path, entries: &[ProjectEntry]) -> Result<()> {
    let document = RegistryDocument {
        version: REGISTRY_VERSION,
        project: entries.to_vec(),
    };
    let raw = toml::to_string_pretty(&document)?;
    create_private_dir(state_dir)
        .with_context(|| format!("failed to secure {}", state_dir.display()))?;
    let target = state_dir.join(REGISTRY_FILE);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp = state_dir.join(format!(
        ".{REGISTRY_FILE}.tmp-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_file(&tmp);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, &target)
            .with_context(|| format!("failed to replace {}", target.display()))?;
        sync_directory(state_dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|handle| handle.sync_all())
        .with_context(|| format!("failed to sync {}", path.display()))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::vm::config::VmConfig;
    use std::os::unix::fs::PermissionsExt;

    fn test_config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.projects_root = dir.join("projects");
        config.state_dir = dir.join("state");
        config.snapshots_dir = dir.join("snapshots");
        config.vms_dir = dir.join("vms");
        config.templates_dir = dir.join("templates");
        config.profiles_dir = dir.join("profiles");
        config
    }

    fn directory(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn entry(name: &str, path: &Path) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            path: path.to_path_buf(),
            vm: None,
            read_only: false,
            kind: ProjectKind::External,
            parent: None,
            managed: false,
            hidden: false,
            created_at: Utc::now(),
        }
    }

    fn vm_store(config: &Config, name: &str) -> VmStore {
        let store = VmStore::new(config);
        store
            .save(&VmConfig::from_config(config, name, None))
            .unwrap();
        store
    }

    #[test]
    fn missing_file_loads_an_empty_version_two_registry() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let registry = ProjectRegistry::load(&state).unwrap();
        assert!(registry.entries().is_empty());
        assert!(!state.join(REGISTRY_FILE).exists());
    }

    #[test]
    fn round_trips_entries_and_omits_absent_options() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let path = directory(dir.path(), "projects/demo");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let mut project = entry("demo", &path);
        project.read_only = true;
        project.hidden = true;
        registry.upsert(project.clone()).unwrap();

        let raw = fs::read_to_string(registry.path()).unwrap();
        assert!(raw.contains("version = 2"), "{raw}");
        assert!(raw.contains("[[project]]"), "{raw}");
        assert!(!raw.contains("vm ="), "absent vm is compact: {raw}");
        assert!(!raw.contains("parent ="), "absent parent is compact: {raw}");

        let reloaded = ProjectRegistry::load(&config.state_dir).unwrap();
        assert_eq!(reloaded.get("demo").unwrap(), &project);
    }

    #[test]
    fn names_with_surrounding_whitespace_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let path = directory(dir.path(), "projects/demo");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let entry = entry(" demo ", &path);
        let err = registry.upsert(entry).unwrap_err().to_string();
        assert!(err.contains("whitespace"), "{err}");
    }

    #[test]
    fn registry_file_is_private_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let path = directory(dir.path(), "projects/demo");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("demo", &path)).unwrap();

        assert_eq!(
            fs::metadata(registry.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&config.state_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let leftovers: Vec<String> = fs::read_dir(&config.state_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn failed_atomic_write_cleans_up_its_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        create_private_dir(&state).unwrap();
        fs::create_dir(state.join(REGISTRY_FILE)).unwrap();

        assert!(write_entries(&state, &[]).is_err());
        let leftovers: Vec<String> = fs::read_dir(&state)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn symlinked_registry_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        create_private_dir(&state).unwrap();
        let target = dir.path().join("target.toml");
        fs::write(&target, "version = 2\n").unwrap();
        std::os::unix::fs::symlink(&target, state.join(REGISTRY_FILE)).unwrap();

        let err = ProjectRegistry::load(&state).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn duplicate_names_and_paths_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let vms = VmStore::new(&config);

        let duplicate_name = r#"
version = 2

[[project]]
name = "demo"
path = "/a"
created_at = "2026-01-01T00:00:00Z"

[[project]]
name = "demo"
path = "/b"
created_at = "2026-01-01T00:00:00Z"
"#;
        fs::create_dir_all(&config.state_dir).unwrap();
        fs::write(config.state_dir.join(REGISTRY_FILE), duplicate_name).unwrap();
        let err = ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .validate(&config, &vms)
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate project name"), "{err}");

        let duplicate_path = r#"
version = 2

[[project]]
name = "one"
path = "/a"
created_at = "2026-01-01T00:00:00Z"

[[project]]
name = "two"
path = "/a"
created_at = "2026-01-01T00:00:00Z"
"#;
        fs::write(config.state_dir.join(REGISTRY_FILE), duplicate_path).unwrap();
        let err = ProjectRegistry::load(&config.state_dir)
            .unwrap()
            .validate(&config, &vms)
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate project path"), "{err}");
    }

    #[test]
    fn malformed_registry_and_wrong_versions_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();

        fs::write(state.join(REGISTRY_FILE), "version = 1\n").unwrap();
        let err = ProjectRegistry::load(&state).unwrap_err().to_string();
        assert!(
            err.contains("unsupported project registry version"),
            "{err}"
        );

        fs::write(state.join(REGISTRY_FILE), "not = [valid").unwrap();
        assert!(ProjectRegistry::load(&state).is_err());
    }

    #[test]
    fn nested_external_members_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let parent = directory(dir.path(), "projects/parser");
        let nested = parent.join("vendor/nested");
        fs::create_dir_all(&nested).unwrap();

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("parser", &parent)).unwrap();
        registry.upsert(entry("nested", &nested)).unwrap();
        registry
            .upsert(entry("other", &directory(dir.path(), "projects/other")))
            .unwrap();
        registry.assign("parser", "rust", false).unwrap();
        registry.assign("nested", "rust", false).unwrap();

        let vms = vm_store(&config, "rust");
        let err = format!("{:#}", registry.validate(&config, &vms).unwrap_err());
        assert!(err.contains("nested inside"), "{err}");
    }

    #[test]
    fn unassigned_nesting_is_allowed_until_the_projects_are_assigned() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let parent = directory(dir.path(), "projects/parser");
        let nested = parent.join("vendor/nested");
        fs::create_dir_all(&nested).unwrap();

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("parser", &parent)).unwrap();
        registry.upsert(entry("nested", &nested)).unwrap();
        let vms = vm_store(&config, "rust");
        registry.validate(&config, &vms).unwrap();

        registry.assign("parser", "rust", false).unwrap();
        registry.assign("nested", "rust", false).unwrap();
        let err = format!("{:#}", registry.validate(&config, &vms).unwrap_err());
        assert!(err.contains("nested inside"), "{err}");
    }

    #[test]
    fn one_assigned_nested_project_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let parent = directory(dir.path(), "projects/parser");
        let nested = parent.join("vendor/nested");
        fs::create_dir_all(&nested).unwrap();

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("parser", &parent)).unwrap();
        registry.upsert(entry("nested", &nested)).unwrap();
        let vms = vm_store(&config, "rust");
        registry.assign("parser", "rust", false).unwrap();
        registry.validate(&config, &vms).unwrap();

        registry.unassign("parser").unwrap();
        registry.assign("nested", "rust", false).unwrap();
        registry.validate(&config, &vms).unwrap();
    }

    fn worktree_pair(
        dir: &Path,
        config: &Config,
        parent_vm: Option<&str>,
        worktree_vm: Option<&str>,
        read_only: bool,
    ) -> ProjectRegistry {
        let parent = directory(dir, "projects/parser");
        let worktree_path = parent.join(".worktrees/fix");
        fs::create_dir_all(&worktree_path).unwrap();

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let mut parent_entry = entry("parser", &parent);
        parent_entry.vm = parent_vm.map(str::to_string);
        parent_entry.read_only = read_only;
        registry.upsert(parent_entry).unwrap();

        let mut worktree = entry("parser-fix", &worktree_path);
        worktree.kind = ProjectKind::Worktree;
        worktree.parent = Some("parser".into());
        worktree.vm = worktree_vm.map(str::to_string);
        registry.upsert(worktree).unwrap();
        registry
    }

    #[test]
    fn worktree_inside_its_parent_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let vms = vm_store(&config, "rust");
        let registry = worktree_pair(dir.path(), &config, Some("rust"), Some("rust"), false);
        registry.validate(&config, &vms).unwrap();
    }

    #[test]
    fn exported_members_exclude_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let _vms = vm_store(&config, "rust");
        let registry = worktree_pair(dir.path(), &config, Some("rust"), Some("rust"), false);

        let all: Vec<&str> = registry
            .members("rust")
            .into_iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(all, vec!["parser", "parser-fix"]);

        let exported: Vec<&str> = registry
            .exported_members("rust")
            .into_iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(exported, vec!["parser"]);
    }

    #[test]
    fn worktree_with_a_different_vm_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let vms = vm_store(&config, "a");
        vm_store(&config, "b");
        let registry = worktree_pair(dir.path(), &config, Some("a"), Some("b"), false);
        let err = registry.validate(&config, &vms).unwrap_err().to_string();
        assert!(err.contains("inherit"), "{err}");
    }

    #[test]
    fn worktree_in_a_read_only_parent_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let vms = vm_store(&config, "rust");
        let registry = worktree_pair(dir.path(), &config, Some("rust"), Some("rust"), true);
        let err = registry.validate(&config, &vms).unwrap_err().to_string();
        assert!(err.contains("read-only"), "{err}");
    }

    #[test]
    fn worktree_requires_an_existing_parent_and_parent_path() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let vms = VmStore::new(&config);
        let parent = directory(dir.path(), "projects/parser");

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let nested = parent.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let mut missing = entry("missing-parent", &nested);
        missing.kind = ProjectKind::Worktree;
        registry.upsert(missing).unwrap();
        let err = registry.validate(&config, &vms).unwrap_err().to_string();
        assert!(err.contains("must name its parent"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let parent = directory(dir.path(), "projects/parser");
        let nested = parent.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let mut unknown = entry("unknown-parent", &nested);
        unknown.kind = ProjectKind::Worktree;
        unknown.parent = Some("nope".into());
        registry.upsert(unknown).unwrap();
        let err = format!(
            "{:#}",
            registry
                .validate(&config, &VmStore::new(&config))
                .unwrap_err()
        );
        assert!(err.contains("unknown parent"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let parent = directory(dir.path(), "projects/parser");
        let elsewhere = directory(dir.path(), "projects/elsewhere");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("parser", &parent)).unwrap();
        let mut outside = entry("outside", &elsewhere);
        outside.kind = ProjectKind::Worktree;
        outside.parent = Some("parser".into());
        registry.upsert(outside).unwrap();
        let err = format!(
            "{:#}",
            registry
                .validate(&config, &VmStore::new(&config))
                .unwrap_err()
        );
        assert!(err.contains("must live inside"), "{err}");
    }

    #[test]
    fn worktree_cannot_be_assigned_independently() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        vm_store(&config, "rust");
        let mut registry = worktree_pair(dir.path(), &config, None, None, false);
        let err = registry
            .assign("parser-fix", "rust", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("inherits"), "{err}");
        assert!(registry.get("parser-fix").unwrap().vm.is_none());
    }

    #[test]
    fn managed_clone_paths_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let vms = vm_store(&config, "rust");
        let wrong = directory(dir.path(), "projects/clone-a");

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let mut clone = entry("clone-a", &wrong);
        clone.kind = ProjectKind::Clone;
        clone.managed = true;
        clone.vm = Some("rust".into());
        registry.upsert(clone.clone()).unwrap();
        let err = registry.validate(&config, &vms).unwrap_err().to_string();
        assert!(err.contains("must live at"), "{err}");

        let expected = directory(dir.path(), "vms/rust/workspaces/clone-a");
        clone.path = expected;
        registry.upsert(clone).unwrap();
        registry.validate(&config, &vms).unwrap();

        let mut unmanaged = entry("clone-b", &wrong);
        unmanaged.kind = ProjectKind::Clone;
        unmanaged.vm = Some("rust".into());
        registry.upsert(unmanaged).unwrap();
        let err = registry.validate(&config, &vms).unwrap_err().to_string();
        assert!(err.contains("must be managed"), "{err}");
    }

    #[test]
    fn max_projects_per_vm_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.max_projects_per_vm = 2;
        let vms = vm_store(&config, "rust");

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        for name in ["one", "two", "three"] {
            let path = directory(dir.path(), &format!("projects/{name}"));
            let mut project = entry(name, &path);
            project.vm = Some("rust".into());
            registry.upsert(project).unwrap();
        }
        let err = registry.validate(&config, &vms).unwrap_err().to_string();
        assert!(err.contains("max_projects_per_vm"), "{err}");
    }

    #[test]
    fn assign_moves_between_vms_and_unassign_clears_the_assignment() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let path = directory(dir.path(), "projects/demo");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("demo", &path)).unwrap();

        registry.assign("demo", "a", true).unwrap();
        let assigned = registry.get("demo").unwrap();
        assert_eq!(assigned.vm.as_deref(), Some("a"));
        assert!(assigned.read_only);

        registry.assign("demo", "b", false).unwrap();
        let assigned = registry.get("demo").unwrap();
        assert_eq!(assigned.vm.as_deref(), Some("b"));
        assert!(!assigned.read_only);

        registry.unassign("demo").unwrap();
        assert!(registry.get("demo").unwrap().vm.is_none());
        assert!(!registry.get("demo").unwrap().read_only);

        let reloaded = ProjectRegistry::load(&config.state_dir).unwrap();
        assert!(reloaded.get("demo").unwrap().vm.is_none());
        assert!(registry.assign("missing", "a", false).is_err());
    }

    #[test]
    fn assigning_an_unknown_vm_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let vms = VmStore::new(&config);
        let path = directory(dir.path(), "projects/demo");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("demo", &path)).unwrap();
        registry.assign("demo", "missing", false).unwrap();

        let err = registry.validate(&config, &vms).unwrap_err().to_string();
        assert!(err.contains("unknown VM"), "{err}");
    }

    #[test]
    fn members_are_filtered_and_sorted_by_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        for (name, relative, vm) in [
            ("alpha", "projects/alpha", Some("rust")),
            ("beta", "projects/beta", Some("rust")),
            ("other", "projects/other", Some("go")),
            ("free", "projects/free", None),
        ] {
            let path = directory(dir.path(), relative);
            let mut project = entry(name, &path);
            project.vm = vm.map(str::to_string);
            registry.upsert(project).unwrap();
        }
        let names: Vec<&str> = registry
            .members("rust")
            .into_iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(registry.members("missing").is_empty());

        let alpha = dir.path().join("projects/alpha").canonicalize().unwrap();
        assert_eq!(
            registry
                .find_by_path(&alpha)
                .map(|entry| entry.name.as_str()),
            Some("alpha")
        );
        assert!(registry.find_by_path(Path::new("/nowhere")).is_none());
    }

    #[test]
    fn remove_refuses_parents_with_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let mut registry = worktree_pair(dir.path(), &config, None, None, false);
        let err = registry.remove("parser").unwrap_err().to_string();
        assert!(err.contains("worktrees still name it"), "{err}");
        assert!(registry.remove("parser-fix").unwrap());
        assert!(registry.remove("parser").unwrap());
        assert!(!registry.remove("parser").unwrap());
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        // hegn state
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let state = directory(dir.path(), "state");
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("state", &state)).unwrap();
        let err = format!(
            "{:#}",
            registry
                .validate(&config, &VmStore::new(&config))
                .unwrap_err()
        );
        assert!(err.contains("overlaps hegn state"), "{err}");

        // The filesystem root
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("root", Path::new("/"))).unwrap();
        let err = format!(
            "{:#}",
            registry
                .validate(&config, &VmStore::new(&config))
                .unwrap_err()
        );
        assert!(err.contains("filesystem root"), "{err}");

        // The projects root itself
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        fs::create_dir_all(&config.projects_root).unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry
            .upsert(entry("projects-root", &config.projects_root))
            .unwrap();
        let err = format!(
            "{:#}",
            registry
                .validate(&config, &VmStore::new(&config))
                .unwrap_err()
        );
        assert!(err.contains("projects root"), "{err}");

        // An ancestor of the projects root
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        fs::create_dir_all(&config.projects_root).unwrap();
        let ancestor = config.projects_root.parent().unwrap().to_path_buf();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("ancestor", &ancestor)).unwrap();
        let err = format!(
            "{:#}",
            registry
                .validate(&config, &VmStore::new(&config))
                .unwrap_err()
        );
        assert!(err.contains("ancestor of the projects root"), "{err}");
    }

    #[test]
    fn home_directory_is_rejected_when_it_exists() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        if !home.is_dir() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        registry.upsert(entry("home", &home)).unwrap();
        let err = format!(
            "{:#}",
            registry
                .validate(&config, &VmStore::new(&config))
                .unwrap_err()
        );
        assert!(err.contains("home directory"), "{err}");
    }
}
