//! Workspace forks: isolated clones and nested git worktrees.
//!
//! A fork is a project member, not a new VM, so it inherits the VM's tools and
//! rootfs. Two strategies exist:
//!
//! - **Clone** (default): a full `git clone` below
//!   `<vms_dir>/<vm>/workspaces/<name>`. The clone owns its `.git`, refs, hooks
//!   and object store, so one agent cannot corrupt the source repository, and
//!   it is exported as its own virtio-fs device.
//! - **Worktree** (opt-in): `git worktree add` inside the parent's exported
//!   tree. It shares refs and objects with the parent and is deliberately *not*
//!   exported separately; its gitdir resolves through the parent's share.
//!
//! The registry is the source of truth: the service trusts a parent's registry
//! entry, never an arbitrary host path. Every path is canonicalized and a
//! planted symlink is refused instead of followed.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::config::Config;
use crate::shared::fs::create_private_dir;
use crate::vm::config::VmStore;

use super::meta::validate_project_name;
use super::registry::{ProjectEntry, ProjectKind, ProjectRegistry};

/// Directory, relative to the parent project, that hosts nested worktrees.
const WORKTREE_SUBDIR: &str = ".hegn/worktrees";

/// How a fork relates to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForkMode {
    /// Full, isolated copy below `<vms_dir>/<vm>/workspaces/<name>`.
    #[default]
    Clone,
    /// Nested worktree inside the parent's tree, sharing git state.
    Worktree,
}

/// Default location of a worktree of `parent`: `<parent>/.hegn/worktrees/<name>`.
pub fn default_worktree_path(parent: &Path, name: &str) -> PathBuf {
    parent.join(WORKTREE_SUBDIR).join(name)
}

/// Whether `path` is a worktree registered in `parent`'s repository.
///
/// Uses `git worktree list --porcelain`, comparing canonical paths. Returns
/// `false` for unreachable paths and for repositories git refuses to inspect.
pub fn is_real_worktree(parent: &Path, path: &Path) -> bool {
    let (Ok(parent), Ok(path)) = (parent.canonicalize(), path.canonicalize()) else {
        return false;
    };
    let Ok(listed) = worktree_paths(&parent) else {
        return false;
    };
    listed.iter().any(|candidate| {
        candidate
            .canonicalize()
            .map(|canonical| canonical == path)
            .unwrap_or_else(|_| candidate == &path)
    })
}

/// Clone and worktree operations bound to a configuration and VM store.
pub struct WorkspaceService<'a> {
    config: &'a Config,
    vms: &'a VmStore,
}

impl<'a> WorkspaceService<'a> {
    pub fn new(config: &'a Config, vms: &'a VmStore) -> Self {
        Self { config, vms }
    }

    /// Fork `parent` according to `mode`.
    ///
    /// `target` is only valid for [`ForkMode::Worktree`]; a clone always lives
    /// at `<vms_dir>/<vm>/workspaces/<name>`.
    pub fn fork(
        &self,
        registry: &mut ProjectRegistry,
        parent: &str,
        name: &str,
        mode: ForkMode,
        target: Option<&Path>,
    ) -> Result<ProjectEntry> {
        match mode {
            ForkMode::Clone => {
                if target.is_some() {
                    bail!(
                        "a clone always lives at <vms_dir>/<vm>/workspaces/<name>; \
                         pass a target only with ForkMode::Worktree"
                    );
                }
                self.fork_clone(registry, parent, name)
            }
            ForkMode::Worktree => self.fork_worktree(registry, parent, name, target),
        }
    }

    /// Clone `parent` into the VM's private workspaces directory.
    ///
    /// Uses `git clone --local`, which hardlinks objects when both repositories
    /// share a filesystem, and falls back to a regular clone only when git
    /// reports a cross-device error.
    pub fn fork_clone(
        &self,
        registry: &mut ProjectRegistry,
        parent: &str,
        name: &str,
    ) -> Result<ProjectEntry> {
        let name = validate_project_name(name)?.to_string();
        let parent_entry = self.parent_entry(registry, parent)?;
        match parent_entry.kind {
            ProjectKind::Worktree => {
                let hint = parent_entry
                    .parent
                    .as_deref()
                    .map(|grandparent| format!("; fork '{grandparent}' instead"))
                    .unwrap_or_default();
                bail!(
                    "cannot clone worktree '{}': its git state is shared with its parent{hint}",
                    parent_entry.name
                );
            }
            ProjectKind::External | ProjectKind::Clone => {}
        }

        let vm = self.assigned_vm(&parent_entry)?;
        self.vms.load(&vm).with_context(|| {
            format!(
                "project '{}' is assigned to unknown VM '{vm}'",
                parent_entry.name
            )
        })?;
        if registry.get(&name).is_some() {
            bail!("a project named '{name}' already exists in the registry");
        }
        self.ensure_vm_capacity(registry, &vm)?;

        let target = self.clone_target(&vm, &name)?;
        ensure_absent(&target, "clone target")?;

        let parent_path = parent_entry
            .path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", parent_entry.path.display()))?;
        require_git_working_tree(&parent_path)?;
        let parent_head = require_head_commit(&parent_path)?;

        let workspaces = target
            .parent()
            .with_context(|| format!("{} has no parent directory", target.display()))?;
        create_private_dir(workspaces)
            .with_context(|| format!("failed to create {}", workspaces.display()))?;

        self.clone_repository(&parent_path, &target)?;

        let verified = (|| -> Result<()> {
            if !target.join(".git").is_dir() {
                bail!("clone {} has no .git directory", target.display());
            }
            let canonical = target
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", target.display()))?;
            if canonical != target {
                bail!(
                    "clone target {} resolves to {}; symlinked workspace paths are not allowed",
                    target.display(),
                    canonical.display()
                );
            }
            let head = require_head_commit(&target)?;
            if head != parent_head {
                bail!(
                    "clone {} is at {head} but the parent is at {parent_head}",
                    target.display()
                );
            }
            Ok(())
        })();
        if let Err(err) = verified {
            remove_dir_best_effort(&target);
            return Err(err);
        }

        let entry = ProjectEntry {
            name: name.clone(),
            path: target.clone(),
            vm: Some(vm),
            read_only: false,
            kind: ProjectKind::Clone,
            parent: Some(parent_entry.name),
            managed: true,
            hidden: false,
            created_at: Utc::now(),
        };
        if let Err(err) = registry.upsert(entry.clone()) {
            remove_dir_best_effort(&target);
            return Err(err).with_context(|| {
                format!("failed to register clone '{name}'; the created directory was removed")
            });
        }
        Ok(entry)
    }

    /// Add a git worktree of `parent`, nested inside the parent's tree.
    ///
    /// The default target is `<parent>/.hegn/worktrees/<name>`. An explicit
    /// target must resolve strictly inside the parent and must not be the parent
    /// itself. Nested worktrees (a worktree of a worktree) are refused.
    pub fn fork_worktree(
        &self,
        registry: &mut ProjectRegistry,
        parent: &str,
        name: &str,
        target: Option<&Path>,
    ) -> Result<ProjectEntry> {
        let name = validate_project_name(name)?.to_string();
        validate_branch_name(&name)?;
        let parent_entry = self.parent_entry(registry, parent)?;
        if parent_entry.kind == ProjectKind::Worktree {
            let hint = parent_entry
                .parent
                .as_deref()
                .unwrap_or("the parent project");
            bail!(
                "cannot create a worktree inside worktree '{}': nested worktrees are not \
                 supported in v1; fork '{hint}' instead",
                parent_entry.name
            );
        }
        if parent_entry.read_only {
            bail!(
                "project '{}' is read-only; a worktree needs a writable parent",
                parent_entry.name
            );
        }

        let vm = self.assigned_vm(&parent_entry)?;
        self.vms.load(&vm).with_context(|| {
            format!(
                "project '{}' is assigned to unknown VM '{vm}'",
                parent_entry.name
            )
        })?;
        if registry.get(&name).is_some() {
            bail!("a project named '{name}' already exists in the registry");
        }

        let parent_path = parent_entry
            .path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", parent_entry.path.display()))?;
        if !parent_path.is_dir() {
            bail!(
                "parent project {} is not a directory",
                parent_path.display()
            );
        }
        require_git_working_tree(&parent_path)?;
        require_head_commit(&parent_path)?;

        let requested = resolve_worktree_target(&parent_path, &name, target)?;
        ensure_absent(&requested, "worktree target")?;

        add_worktree(&parent_path, &requested, &name)?;

        let verified = (|| -> Result<PathBuf> {
            let canonical = requested
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", requested.display()))?;
            if canonical == parent_path || !canonical.starts_with(&parent_path) {
                bail!(
                    "worktree {} resolves to {}, outside the parent project {}",
                    requested.display(),
                    canonical.display(),
                    parent_path.display()
                );
            }
            if !is_real_worktree(&parent_path, &canonical) {
                bail!(
                    "git did not register {} as a worktree of {}",
                    canonical.display(),
                    parent_path.display()
                );
            }
            Ok(canonical)
        })();
        let worktree_path = match verified {
            Ok(path) => path,
            Err(err) => {
                let _ = git_worktree_remove(&parent_path, &requested, true);
                return Err(err);
            }
        };

        let entry = ProjectEntry {
            name: name.clone(),
            path: worktree_path,
            vm: Some(vm),
            read_only: false,
            kind: ProjectKind::Worktree,
            parent: Some(parent_entry.name),
            managed: false,
            hidden: false,
            created_at: Utc::now(),
        };
        if let Err(err) = registry.upsert(entry.clone()) {
            let _ = git_worktree_remove(&parent_path, &entry.path, true);
            return Err(err).with_context(|| {
                format!("failed to register worktree '{name}'; the git worktree was removed again")
            });
        }
        Ok(entry)
    }

    /// Delete a hegn-managed clone and its registry entry.
    ///
    /// The registry entry is removed first, so a failed directory deletion can
    /// never leave a registered-but-half-deleted workspace that a VM would try
    /// to boot. Returns the workspace path that was released.
    pub fn remove_clone(&self, registry: &mut ProjectRegistry, name: &str) -> Result<PathBuf> {
        let entry = registry
            .get(name)
            .with_context(|| format!("unknown project '{name}'"))?
            .clone();
        if entry.kind != ProjectKind::Clone {
            bail!(
                "project '{}' is not a clone; only hegn-managed clones are deleted here",
                entry.name
            );
        }
        if !entry.managed {
            bail!(
                "clone '{}' is not managed by hegn and was left untouched",
                entry.name
            );
        }
        let vm = entry
            .vm
            .as_deref()
            .with_context(|| format!("clone '{}' has no VM assignment", entry.name))?;
        let target = self.checked_clone_path(&entry, vm)?;

        if !registry.remove(name)? {
            bail!("project '{name}' disappeared from the registry");
        }

        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing to delete symlinked clone path {}",
                    target.display()
                );
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!("clone path {} is not a directory", target.display());
            }
            Ok(_) => {
                let canonical = target
                    .canonicalize()
                    .with_context(|| format!("failed to resolve {}", target.display()))?;
                if canonical != target {
                    bail!(
                        "refusing to delete {}: it resolves to {}",
                        target.display(),
                        canonical.display()
                    );
                }
                fs::remove_dir_all(&canonical).with_context(|| {
                    format!("failed to delete clone directory {}", canonical.display())
                })?;
            }
            // The workspace is already gone; dropping the stale entry is enough.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("failed to inspect {}", target.display()));
            }
        }
        Ok(target)
    }

    /// Remove a registered worktree through git, then drop its registry entry.
    ///
    /// `git worktree remove` runs first, so a worktree that git still considers
    /// attached (for example because it has uncommitted changes) stays
    /// registered. Only the worktree directory is ever touched, never the
    /// parent tree.
    pub fn remove_worktree(&self, registry: &mut ProjectRegistry, name: &str) -> Result<PathBuf> {
        let entry = registry
            .get(name)
            .with_context(|| format!("unknown project '{name}'"))?
            .clone();
        if entry.kind != ProjectKind::Worktree {
            bail!("project '{}' is not a git worktree", entry.name);
        }
        let parent_name = entry
            .parent
            .as_deref()
            .with_context(|| format!("worktree '{}' has no parent project", entry.name))?;
        let parent = registry
            .get(parent_name)
            .with_context(|| {
                format!(
                    "worktree '{}' names unknown parent '{parent_name}'",
                    entry.name
                )
            })?
            .clone();
        let parent_path = parent.path.canonicalize().with_context(|| {
            format!("failed to resolve parent project {}", parent.path.display())
        })?;
        if !parent_path.is_dir() {
            bail!(
                "parent project {} is not a directory",
                parent_path.display()
            );
        }
        let target = entry
            .path
            .canonicalize()
            .with_context(|| format!("failed to resolve worktree {}", entry.path.display()))?;
        if target == parent_path || !target.starts_with(&parent_path) {
            bail!(
                "refusing to remove worktree '{}': {} is not inside its parent {}",
                entry.name,
                target.display(),
                parent_path.display()
            );
        }
        if !is_real_worktree(&parent_path, &target) {
            bail!(
                "refusing to remove '{}': {} is not a git worktree of {}",
                entry.name,
                target.display(),
                parent_path.display()
            );
        }

        git_worktree_remove(&parent_path, &target, false)
            .with_context(|| format!("failed to remove worktree '{}'", entry.name))?;

        if !registry.remove(name)? {
            bail!("project '{name}' disappeared from the registry");
        }
        Ok(target)
    }

    fn parent_entry(&self, registry: &ProjectRegistry, parent: &str) -> Result<ProjectEntry> {
        let entry = registry
            .get(parent)
            .with_context(|| format!("unknown project '{parent}'"))?;
        if entry.vm.is_none() {
            bail!(
                "project '{}' is not assigned to a VM; assign it before forking",
                entry.name
            );
        }
        Ok(entry.clone())
    }

    fn assigned_vm(&self, entry: &ProjectEntry) -> Result<String> {
        entry.vm.clone().with_context(|| {
            format!(
                "project '{}' is not assigned to a VM; assign it before forking",
                entry.name
            )
        })
    }

    /// Refuse a fork that would push the VM above `max_projects_per_vm`.
    ///
    /// Worktrees do not count: they are not exported separately.
    fn ensure_vm_capacity(&self, registry: &ProjectRegistry, vm: &str) -> Result<()> {
        let exported = registry.exported_members(vm).len();
        if exported >= self.config.max_projects_per_vm as usize {
            bail!(
                "VM '{vm}' already exports {exported} projects (max_projects_per_vm = {})",
                self.config.max_projects_per_vm
            );
        }
        Ok(())
    }

    /// Canonical `<vms_dir>/<vm>/workspaces/<name>` for a managed clone.
    fn clone_target(&self, vm: &str, name: &str) -> Result<PathBuf> {
        let name = validate_project_name(name)?;
        let vm_dir = self
            .vms
            .dir(vm)?
            .canonicalize()
            .with_context(|| format!("VM '{vm}' directory does not exist"))?;
        let target = vm_dir.join("workspaces").join(name);
        let root = self
            .config
            .vms_dir
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", self.config.vms_dir.display()))?;
        if !target.starts_with(&root) {
            bail!(
                "internal error: clone target {} escapes the VMs directory {}",
                target.display(),
                root.display()
            );
        }
        Ok(target)
    }

    /// Validate that `entry` points exactly at its managed workspace path.
    fn checked_clone_path(&self, entry: &ProjectEntry, vm: &str) -> Result<PathBuf> {
        if validate_project_name(&entry.name)? != entry.name {
            bail!(
                "refusing to delete clone '{}': its registered name is not a clean path component",
                entry.name
            );
        }
        let expected = self.clone_target(vm, &entry.name)?;
        let canonical = match fs::symlink_metadata(&entry.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing to delete symlinked clone path {}",
                    entry.path.display()
                );
            }
            Ok(metadata) if !metadata.is_dir() => {
                bail!("clone path {} is not a directory", entry.path.display());
            }
            Ok(_) => entry
                .path
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", entry.path.display()))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => entry.path.clone(),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to inspect {}", entry.path.display()));
            }
        };
        if canonical != expected {
            bail!(
                "refusing to delete clone '{}' at {}: the expected workspace is {}",
                entry.name,
                canonical.display(),
                expected.display()
            );
        }
        Ok(expected)
    }

    fn clone_repository(&self, source: &Path, target: &Path) -> Result<()> {
        let mut command = git(None);
        command
            .args(["clone", "--local", "--"])
            .arg(source)
            .arg(target);
        let output = command.output().with_context(|| {
            format!(
                "failed to run git clone from {} (is git installed?)",
                source.display()
            )
        })?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !is_cross_device_failure(&stderr) {
            bail!(
                "git clone --local from {} to {} failed: {stderr}",
                source.display(),
                target.display()
            );
        }

        // `--local` hardlinks objects; on a different filesystem git fails with
        // EXDEV. Remove whatever the failed attempt left behind and retry
        // without hardlinks.
        if fs::symlink_metadata(target)
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            remove_dir_best_effort(target);
        }
        let mut command = git(None);
        command.args(["clone", "--"]).arg(source).arg(target);
        let output = command.output().with_context(|| {
            format!(
                "failed to run git clone from {} after a cross-device local clone",
                source.display()
            )
        })?;
        if !output.status.success() {
            bail!(
                "git clone from {} to {} failed after the local clone reported a cross-device \
                 error: {}",
                source.display(),
                target.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Use the same hardened git invocation as the rest of hegn.
///
/// The repository being forked is untrusted input: hooks, `core.fsmonitor`, and
/// clean/process filters could otherwise run host code when git opens it.
fn git(current_dir: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(dir) = current_dir {
        command.arg("-C").arg(dir);
    }
    command
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "core.fsmonitor=false"])
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
}

fn run_git(current_dir: Option<&Path>, args: &[&str], what: &str) -> Result<String> {
    let mut command = git(current_dir);
    command.args(args);
    let output = command
        .output()
        .with_context(|| format!("failed to run {what}: is git installed?"))?;
    if !output.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn require_git_working_tree(path: &Path) -> Result<()> {
    let inside = run_git(
        Some(path),
        &["rev-parse", "--is-inside-work-tree"],
        &format!("git rev-parse in {}", path.display()),
    )?;
    if inside != "true" {
        bail!("{} is not a git working tree", path.display());
    }
    Ok(())
}

fn require_head_commit(path: &Path) -> Result<String> {
    let mut command = git(Some(path));
    command.args(["rev-parse", "--verify", "HEAD"]);
    let output = command
        .output()
        .with_context(|| format!("failed to run git in {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "{} has no git commit to fork; create an initial commit first",
            path.display()
        );
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head.is_empty() {
        bail!("{} has no git commit to fork", path.display());
    }
    Ok(head)
}

fn branch_exists(parent: &Path, branch: &str) -> Result<bool> {
    let mut command = git(Some(parent));
    command
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"));
    let output = command.output().context("failed to run git show-ref")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to check whether branch '{branch}' exists: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn add_worktree(parent: &Path, target: &Path, name: &str) -> Result<()> {
    if branch_exists(parent, name)? {
        bail!(
            "branch '{name}' already exists in {}; choose a different worktree name",
            parent.display()
        );
    }
    let target_dir = target
        .parent()
        .with_context(|| format!("{} has no parent directory", target.display()))?;
    fs::create_dir_all(target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;

    let mut command = git(Some(parent));
    command
        .args(["worktree", "add", "-b", name, "--"])
        .arg(target)
        .arg("HEAD");
    let output = command
        .output()
        .with_context(|| format!("failed to run git worktree add for {}", target.display()))?;
    if !output.status.success() {
        bail!(
            "git worktree add {} failed: {}",
            target.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn git_worktree_remove(parent: &Path, target: &Path, force: bool) -> Result<()> {
    let mut command = git(Some(parent));
    command.args(["worktree", "remove"]);
    if force {
        command.arg("--force");
    }
    command.arg("--").arg(target);
    let output = command
        .output()
        .with_context(|| format!("failed to run git worktree remove for {}", target.display()))?;
    if !output.status.success() {
        bail!(
            "git worktree remove {} failed: {}",
            target.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn worktree_paths(parent: &Path) -> Result<Vec<PathBuf>> {
    let raw = run_git(
        Some(parent),
        &["worktree", "list", "--porcelain"],
        "git worktree list",
    )?;
    Ok(parse_worktree_porcelain(&raw))
}

/// Extract the `worktree <path>` lines from `git worktree list --porcelain`.
fn parse_worktree_porcelain(raw: &str) -> Vec<PathBuf> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

/// Resolve an explicit or default worktree target below `parent`.
fn resolve_worktree_target(parent: &Path, name: &str, target: Option<&Path>) -> Result<PathBuf> {
    let requested = match target {
        Some(target) => {
            if !target.is_absolute() {
                bail!(
                    "worktree target {} must be an absolute path",
                    target.display()
                );
            }
            target.to_path_buf()
        }
        None => default_worktree_path(parent, name),
    };
    let resolved = canonicalize_allow_missing(&requested)?;
    if resolved == parent {
        bail!(
            "worktree target {} must not be the parent project itself",
            requested.display()
        );
    }
    if !resolved.starts_with(parent) {
        bail!(
            "worktree target {} must live inside the parent project {}",
            resolved.display(),
            parent.display()
        );
    }
    Ok(resolved)
}

/// Canonicalize `path`, allowing a non-existent tail but never following a
/// symlink in the missing part.
///
/// `Path::canonicalize` only works for existing paths; `git worktree add` needs
/// a destination that does not exist yet. Walking up to the deepest existing
/// ancestor keeps the same no-symlink guarantee the rest of hegn relies on.
fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let mut missing: Vec<OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match current.canonicalize() {
            Ok(canonical) => {
                let mut resolved = canonical;
                for component in missing.iter().rev() {
                    if component == ".." || component == "." {
                        bail!(
                            "path {} must not contain '.' or '..' in a missing component",
                            path.display()
                        );
                    }
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // `canonicalize` already fails for a dangling symlink; report it
                // instead of treating the link itself as a normal component.
                if fs::symlink_metadata(&current).is_ok() {
                    bail!(
                        "cannot resolve {}: {} is not a resolvable directory",
                        path.display(),
                        current.display()
                    );
                }
                let name = current
                    .file_name()
                    .with_context(|| format!("cannot resolve {}", path.display()))?;
                missing.push(name.to_os_string());
                current = current
                    .parent()
                    .with_context(|| format!("cannot resolve {}", path.display()))?
                    .to_path_buf();
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to resolve {}", current.display()));
            }
        }
    }
}

/// Refuse to fork over an existing path, without following a planted link.
fn ensure_absent(path: &Path, what: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing {what} {}: it is a symlink", path.display());
        }
        Ok(metadata) if metadata.is_dir() => {
            bail!("{what} {} already exists", path.display());
        }
        Ok(_) => bail!("{what} {} exists and is not a directory", path.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("failed to inspect {what} {}", path.display()))
        }
    }
}

/// A project name may legally contain spaces, but a git branch may not.
fn validate_branch_name(name: &str) -> Result<()> {
    let invalid = name.is_empty()
        || name.starts_with('-')
        || name.starts_with('.')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("@{")
        || name
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'));
    if invalid {
        bail!(
            "worktree name '{name}' is not a valid git branch name; choose a name without \
             spaces, '..' or a trailing '.lock'"
        );
    }
    Ok(())
}

/// Only a cross-device error justifies retrying a clone without `--local`.
fn is_cross_device_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("cross-device") || lower.contains("exdev")
}

fn remove_dir_best_effort(path: &Path) {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        let _ = fs::remove_dir_all(path);
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::vm::config::VmConfig;

    struct Fixture {
        dir: tempfile::TempDir,
        config: Config,
        vms: VmStore,
        registry: ProjectRegistry,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().canonicalize().unwrap();
            let mut config = Config::default();
            config.projects_root = base.join("projects");
            config.state_dir = base.join("state");
            config.snapshots_dir = base.join("snapshots");
            config.vms_dir = base.join("vms");
            config.templates_dir = base.join("templates");
            config.profiles_dir = base.join("profiles");
            fs::create_dir_all(&config.projects_root).unwrap();
            let vms = VmStore::new(&config);
            vms.save(&VmConfig::from_config(&config, "rust", None))
                .unwrap();
            let registry = ProjectRegistry::load(&config.state_dir).unwrap();
            Self {
                dir,
                config,
                vms,
                registry,
            }
        }

        fn repo(&mut self, name: &str) -> PathBuf {
            let path = self.config.projects_root.join(name);
            fs::create_dir_all(&path).unwrap();
            git_ok(&path, &["init", "--quiet"]);
            git_ok(&path, &["config", "user.email", "test@example.com"]);
            git_ok(&path, &["config", "user.name", "Test"]);
            fs::write(path.join("README.md"), format!("# {name}\n")).unwrap();
            git_ok(&path, &["add", "."]);
            git_ok(&path, &["commit", "--quiet", "-m", "initial"]);
            path
        }

        fn register(&mut self, name: &str, path: &Path, read_only: bool) {
            self.registry.upsert(external_entry(name, path)).unwrap();
            self.registry.assign(name, "rust", read_only).unwrap();
        }

        fn parent(&mut self) -> PathBuf {
            let path = self.repo("parser");
            self.register("parser", &path, false);
            path
        }

        fn assert_valid(&self) {
            self.registry.validate(&self.config, &self.vms).unwrap();
        }
    }

    fn external_entry(name: &str, path: &Path) -> ProjectEntry {
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

    #[test]
    fn clone_has_independent_git_state_and_can_commit() {
        let mut fixture = Fixture::new();
        let parent = fixture.parent();
        let parent_head = git_ok(&parent, &["rev-parse", "HEAD"]);

        let entry = WorkspaceService::new(&fixture.config, &fixture.vms)
            .fork_clone(&mut fixture.registry, "parser", "parser-fix")
            .unwrap();

        assert_eq!(entry.kind, ProjectKind::Clone);
        assert!(entry.managed);
        assert!(!entry.read_only);
        assert_eq!(entry.parent.as_deref(), Some("parser"));
        assert_eq!(entry.vm.as_deref(), Some("rust"));
        assert_eq!(entry.path, entry.path.canonicalize().unwrap());
        assert!(
            entry.path.join(".git").is_dir(),
            "a clone owns its git directory"
        );
        assert_eq!(git_ok(&entry.path, &["rev-parse", "HEAD"]), parent_head);
        assert_eq!(
            fs::read_to_string(entry.path.join("README.md")).unwrap(),
            "# parser\n"
        );

        let vms_root = fixture.config.vms_dir.canonicalize().unwrap();
        assert_eq!(entry.path, vms_root.join("rust/workspaces/parser-fix"));
        assert!(entry.path.starts_with(&vms_root));
        assert!(
            !entry
                .path
                .starts_with(fixture.config.projects_root.canonicalize().unwrap())
        );

        // The clone commits independently and the parent is untouched.
        fs::write(entry.path.join("fix.txt"), "fix\n").unwrap();
        git_ok(&entry.path, &["add", "."]);
        git_ok(&entry.path, &["commit", "--quiet", "-m", "fix"]);
        assert_ne!(git_ok(&entry.path, &["rev-parse", "HEAD"]), parent_head);
        assert!(!parent.join("fix.txt").exists());
        assert_eq!(git_ok(&parent, &["rev-parse", "HEAD"]), parent_head);

        fixture.assert_valid();
    }

    #[test]
    fn fork_rejects_an_unknown_or_unassigned_parent() {
        let mut fixture = Fixture::new();
        let path = fixture.repo("parser");
        fixture
            .registry
            .upsert(external_entry("parser", &path))
            .unwrap();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);

        let err = service
            .fork_clone(&mut fixture.registry, "parser", "clone")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not assigned"), "{err}");
        let err = service
            .fork_worktree(&mut fixture.registry, "parser", "wt", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not assigned"), "{err}");
        let err = service
            .fork_clone(&mut fixture.registry, "missing", "clone")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown project"), "{err}");

        assert!(fixture.registry.get("clone").is_none());
        assert!(fixture.registry.get("wt").is_none());
        assert!(!path.join(".hegn").exists());
    }

    #[test]
    fn worktree_lives_inside_the_parent_and_is_not_exported() {
        let mut fixture = Fixture::new();
        let parent = fixture.parent();

        let entry = WorkspaceService::new(&fixture.config, &fixture.vms)
            .fork_worktree(&mut fixture.registry, "parser", "parser-experiment", None)
            .unwrap();

        assert_eq!(entry.kind, ProjectKind::Worktree);
        assert!(!entry.managed);
        assert_eq!(entry.parent.as_deref(), Some("parser"));
        assert_eq!(entry.vm.as_deref(), Some("rust"));
        assert_eq!(entry.path, parent.join(".hegn/worktrees/parser-experiment"));
        assert!(entry.path.starts_with(&parent));
        assert!(
            entry.path.join(".git").is_file(),
            "a worktree .git is a file pointing back at the parent"
        );

        assert!(is_real_worktree(&parent, &entry.path));
        let listed = git_ok(&parent, &["worktree", "list", "--porcelain"]);
        assert!(
            listed.contains(entry.path.to_str().unwrap()),
            "porcelain output must list the worktree: {listed}"
        );

        let exported: Vec<&str> = fixture
            .registry
            .exported_members("rust")
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(exported, vec!["parser"]);
        let members: Vec<&str> = fixture
            .registry
            .members("rust")
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(members, vec!["parser", "parser-experiment"]);

        // A commit in the worktree lands in the shared repository, not in the
        // parent's working tree.
        fs::write(entry.path.join("experiment.txt"), "x\n").unwrap();
        git_ok(&entry.path, &["add", "."]);
        git_ok(&entry.path, &["commit", "--quiet", "-m", "experiment"]);
        assert!(!parent.join("experiment.txt").exists());
        assert!(
            git_ok(&parent, &["branch", "--list", "parser-experiment"])
                .contains("parser-experiment")
        );

        fixture.assert_valid();
    }

    #[test]
    fn worktree_rejects_targets_outside_the_parent() {
        let mut fixture = Fixture::new();
        let parent = fixture.parent();
        let outside = fixture.config.projects_root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);

        let err = service
            .fork_worktree(&mut fixture.registry, "parser", "wt", Some(&outside))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must live inside the parent"), "{err}");

        let escaped = parent.join("../escape");
        let err = service
            .fork_worktree(&mut fixture.registry, "parser", "wt", Some(&escaped))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must live inside the parent"), "{err}");
        assert!(!fixture.config.projects_root.join("escape").exists());

        let err = service
            .fork_worktree(&mut fixture.registry, "parser", "wt", Some(&parent))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be the parent"), "{err}");

        let err = service
            .fork_worktree(
                &mut fixture.registry,
                "parser",
                "wt",
                Some(Path::new("relative")),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"), "{err}");

        assert!(fixture.registry.get("wt").is_none());
        assert!(!parent.join(".hegn").exists());
        assert!(parent.join("README.md").is_file());
    }

    #[test]
    fn worktree_rejects_a_read_only_parent_but_clones_are_allowed() {
        let mut fixture = Fixture::new();
        let path = fixture.repo("parser");
        fixture.register("parser", &path, true);
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);

        let err = service
            .fork_worktree(&mut fixture.registry, "parser", "wt", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("read-only"), "{err}");

        let clone = service
            .fork_clone(&mut fixture.registry, "parser", "parser-clone")
            .unwrap();
        assert_eq!(clone.kind, ProjectKind::Clone);
        assert!(!clone.read_only);
        assert!(fixture.registry.get("wt").is_none());
    }

    #[test]
    fn worktrees_and_worktree_parents_are_not_nested() {
        let mut fixture = Fixture::new();
        fixture.parent();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);

        service
            .fork_worktree(&mut fixture.registry, "parser", "wt", None)
            .unwrap();
        let err = service
            .fork_worktree(&mut fixture.registry, "wt", "nested", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nested worktrees are not supported"), "{err}");
        let err = service
            .fork_clone(&mut fixture.registry, "wt", "from-worktree")
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot clone worktree"), "{err}");

        assert!(fixture.registry.get("nested").is_none());
        assert!(fixture.registry.get("from-worktree").is_none());
    }

    #[test]
    fn duplicate_names_branches_and_targets_are_rejected() {
        let mut fixture = Fixture::new();
        let parent = fixture.parent();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);

        let first = service
            .fork_clone(&mut fixture.registry, "parser", "fix")
            .unwrap();
        let err = service
            .fork_clone(&mut fixture.registry, "parser", "fix")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(first.path.is_dir());

        git_ok(&parent, &["branch", "experiment"]);
        let err = service
            .fork_worktree(&mut fixture.registry, "parser", "experiment", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("branch 'experiment' already exists"), "{err}");
        assert!(!parent.join(".hegn/worktrees/experiment").exists());

        let first_worktree = service
            .fork_worktree(&mut fixture.registry, "parser", "wt-one", None)
            .unwrap();
        let err = service
            .fork_worktree(
                &mut fixture.registry,
                "parser",
                "wt-two",
                Some(&first_worktree.path),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(fixture.registry.get("wt-two").is_none());

        let err = service
            .fork_worktree(&mut fixture.registry, "parser", "with space", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("valid git branch name"), "{err}");
        assert!(!parent.join(".hegn/worktrees/with space").exists());
    }

    #[test]
    fn fork_dispatch_only_accepts_targets_for_worktrees() {
        let mut fixture = Fixture::new();
        fixture.parent();
        let err = WorkspaceService::new(&fixture.config, &fixture.vms)
            .fork(
                &mut fixture.registry,
                "parser",
                "copy",
                ForkMode::Clone,
                Some(Path::new("/tmp/elsewhere")),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("ForkMode::Worktree"), "{err}");
        assert!(fixture.registry.get("copy").is_none());
    }

    #[test]
    fn removing_a_clone_deletes_only_the_managed_workspace() {
        let mut fixture = Fixture::new();
        let parent = fixture.parent();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);
        let clone = service
            .fork_clone(&mut fixture.registry, "parser", "parser-fix")
            .unwrap();

        let removed = service
            .remove_clone(&mut fixture.registry, "parser-fix")
            .unwrap();
        assert_eq!(removed, clone.path);
        assert!(!clone.path.exists());
        assert!(fixture.registry.get("parser-fix").is_none());
        assert!(fixture.registry.get("parser").is_some());
        assert!(parent.join("README.md").is_file());
        assert!(!fixture.registry.path().is_dir());

        let reloaded = ProjectRegistry::load(&fixture.config.state_dir).unwrap();
        assert!(reloaded.get("parser-fix").is_none());
        assert!(reloaded.get("parser").is_some());
    }

    #[test]
    fn removing_a_clone_with_a_missing_directory_only_clears_the_entry() {
        let mut fixture = Fixture::new();
        fixture.parent();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);
        let clone = service
            .fork_clone(&mut fixture.registry, "parser", "parser-fix")
            .unwrap();
        fs::remove_dir_all(&clone.path).unwrap();

        let removed = service
            .remove_clone(&mut fixture.registry, "parser-fix")
            .unwrap();
        assert_eq!(removed, clone.path);
        assert!(fixture.registry.get("parser-fix").is_none());
    }

    #[test]
    fn clone_removal_refuses_unmanaged_or_misplaced_directories() {
        let mut fixture = Fixture::new();
        let parent = fixture.parent();
        let elsewhere = fixture.config.projects_root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();

        fixture
            .registry
            .upsert(ProjectEntry {
                name: "tampered".into(),
                path: elsewhere.clone(),
                vm: Some("rust".into()),
                read_only: false,
                kind: ProjectKind::Clone,
                parent: Some("parser".into()),
                managed: true,
                hidden: false,
                created_at: Utc::now(),
            })
            .unwrap();
        let unmanaged = fixture.config.vms_dir.join("rust/workspaces/unmanaged");
        fs::create_dir_all(&unmanaged).unwrap();
        fixture
            .registry
            .upsert(ProjectEntry {
                name: "unmanaged".into(),
                path: unmanaged.clone(),
                vm: Some("rust".into()),
                read_only: false,
                kind: ProjectKind::Clone,
                parent: Some("parser".into()),
                managed: false,
                hidden: false,
                created_at: Utc::now(),
            })
            .unwrap();

        let service = WorkspaceService::new(&fixture.config, &fixture.vms);
        let err = service
            .remove_clone(&mut fixture.registry, "tampered")
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected workspace"), "{err}");
        assert!(elsewhere.is_dir());
        assert!(fixture.registry.get("tampered").is_some());

        let err = service
            .remove_clone(&mut fixture.registry, "unmanaged")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not managed"), "{err}");
        assert!(unmanaged.is_dir());
        assert!(fixture.registry.get("unmanaged").is_some());

        let err = service
            .remove_clone(&mut fixture.registry, "parser")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a clone"), "{err}");
        assert!(parent.is_dir());
        assert!(fixture.registry.get("parser").is_some());
    }

    #[test]
    fn removing_a_worktree_keeps_the_parent_intact() {
        let mut fixture = Fixture::new();
        let parent = fixture.parent();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);
        let worktree = service
            .fork_worktree(&mut fixture.registry, "parser", "parser-experiment", None)
            .unwrap();

        fs::write(worktree.path.join("experiment.txt"), "x\n").unwrap();
        git_ok(&worktree.path, &["add", "."]);
        git_ok(&worktree.path, &["commit", "--quiet", "-m", "experiment"]);

        let removed = service
            .remove_worktree(&mut fixture.registry, "parser-experiment")
            .unwrap();
        assert_eq!(removed, worktree.path);
        assert!(!worktree.path.exists());
        assert!(!is_real_worktree(&parent, &worktree.path));
        assert!(parent.join(".git").is_dir());
        assert!(parent.join("README.md").is_file());
        assert!(!parent.join("experiment.txt").exists());
        assert!(fixture.registry.get("parser-experiment").is_none());
        assert!(fixture.registry.get("parser").is_some());

        fixture.assert_valid();
    }

    #[test]
    fn worktree_removal_refuses_a_dirty_worktree() {
        let mut fixture = Fixture::new();
        fixture.parent();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);
        let worktree = service
            .fork_worktree(&mut fixture.registry, "parser", "parser-experiment", None)
            .unwrap();
        fs::write(worktree.path.join("scratch.txt"), "untracked\n").unwrap();

        let err = service
            .remove_worktree(&mut fixture.registry, "parser-experiment")
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to remove worktree"), "{err}");
        assert!(worktree.path.join("scratch.txt").is_file());
        assert!(fixture.registry.get("parser-experiment").is_some());
    }

    #[test]
    fn worktree_removal_refuses_entries_that_are_not_real_worktrees() {
        let mut fixture = Fixture::new();
        let parent = fixture.parent();
        let fake = parent.join("not-a-worktree");
        fs::create_dir_all(&fake).unwrap();
        fixture
            .registry
            .upsert(ProjectEntry {
                name: "fake".into(),
                path: fake.clone(),
                vm: Some("rust".into()),
                read_only: false,
                kind: ProjectKind::Worktree,
                parent: Some("parser".into()),
                managed: false,
                hidden: false,
                created_at: Utc::now(),
            })
            .unwrap();
        let service = WorkspaceService::new(&fixture.config, &fixture.vms);

        let err = service
            .remove_worktree(&mut fixture.registry, "fake")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a git worktree"), "{err}");
        assert!(fake.is_dir());
        assert!(fixture.registry.get("fake").is_some());

        let err = service
            .remove_worktree(&mut fixture.registry, "parser")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a git worktree"), "{err}");
        assert!(parent.is_dir());
        assert!(fixture.registry.get("parser").is_some());
    }

    #[test]
    fn a_failed_registry_write_removes_the_new_clone_again() {
        let mut fixture = Fixture::new();
        fixture.parent();

        // Keep the in-memory registry (so the parent resolves) but make the
        // writer fail: a symlinked registry file is refused.
        let registry_path = fixture.registry.path();
        fs::remove_file(&registry_path).unwrap();
        let victim = fixture.dir.path().join("victim.toml");
        fs::write(&victim, "not a registry\n").unwrap();
        std::os::unix::fs::symlink(&victim, &registry_path).unwrap();

        let err = WorkspaceService::new(&fixture.config, &fixture.vms)
            .fork_clone(&mut fixture.registry, "parser", "parser-fix")
            .unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");

        let target = fixture
            .config
            .vms_dir
            .canonicalize()
            .unwrap()
            .join("rust/workspaces/parser-fix");
        assert!(
            !target.exists(),
            "a clone that could not be registered must be removed again"
        );
        assert_eq!(fs::read_to_string(&victim).unwrap(), "not a registry\n");
    }

    #[test]
    fn cross_device_detection_matches_git_errors_only() {
        assert!(is_cross_device_failure(
            "fatal: failed to create link 'x': Invalid cross-device link"
        ));
        assert!(is_cross_device_failure(
            "EXDEV: cross-device link not permitted"
        ));
        assert!(!is_cross_device_failure(
            "fatal: destination path 'x' already exists"
        ));
        assert!(!is_cross_device_failure(
            "fatal: repository 'x' does not exist"
        ));
    }

    #[test]
    fn worktree_porcelain_parsing_ignores_non_worktree_lines() {
        let raw = "worktree /a/one\nHEAD abc\nbranch refs/heads/main\n\n\
                   worktree /a/two\nHEAD def\ndetached\n";
        assert_eq!(
            parse_worktree_porcelain(raw),
            vec![PathBuf::from("/a/one"), PathBuf::from("/a/two")]
        );
        assert!(parse_worktree_porcelain("").is_empty());
    }
}
