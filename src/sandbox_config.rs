//! Per-sandbox configuration and workspace cloning.
//!
//! Every sandbox lives in its own directory under `sandboxes_dir`:
//!
//! ```text
//! <sandboxes_dir>/<name>/
//!   hegn.toml     per-sandbox settings (private, 0600)
//!   workspace/    clone for multi-agent sandboxes
//!   run/          runtime sockets/pids/lock (created on start)
//! ```
//!
//! A normal sandbox points `workspace` at the original project; a multi-agent
//! clone gets a private workspace inside the sandbox directory. Deleting a
//! sandbox never deletes a workspace outside its own directory.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Harness, SecurityMode};

pub const CONFIG_FILE: &str = "hegn.toml";
pub const WORKSPACE_DIR: &str = "workspace";
const CONFIG_VERSION: u32 = 1;
const MAX_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub version: u32,
    pub name: String,
    /// Original project the sandbox belongs to.
    pub project: PathBuf,
    /// Directory exported to the guest. For normal sandboxes this is the
    /// project itself; for clones it is `<sandbox>/workspace`.
    pub workspace: PathBuf,
    /// Optional per-sandbox root filesystem. When absent, hegn copies the
    /// global image into the sandbox on first start so concurrent VMs never
    /// share a writable disk.
    pub rootfs_image: Option<PathBuf>,
    pub harness: Harness,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub shared_tag: String,
    pub guest_mount: String,
    /// Clone created for multi-agent work.
    pub ephemeral: bool,
    pub source_sandbox: Option<String>,
    pub created_at: DateTime<Utc>,
    pub auto_branch: bool,
    pub security: SecurityMode,
    pub extra_packages: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            name: String::new(),
            project: PathBuf::new(),
            workspace: PathBuf::new(),
            rootfs_image: None,
            harness: Harness::default(),
            vcpus: 2,
            memory_mib: 2048,
            shared_tag: "workspace".to_string(),
            guest_mount: "/workspace".to_string(),
            ephemeral: false,
            source_sandbox: None,
            created_at: Utc::now(),
            auto_branch: true,
            security: SecurityMode::Enforce,
            extra_packages: Vec::new(),
        }
    }
}

impl SandboxConfig {
    /// Stable sandbox name derived from a project directory.
    pub fn suggested_name(project: &Path) -> String {
        let raw = project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "sandbox".to_string());
        let mut name = String::new();
        let mut last_dash = false;
        for ch in raw.chars() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                name.push(ch);
                last_dash = false;
            } else if !last_dash {
                name.push('-');
                last_dash = true;
            }
        }
        let name = name.trim_matches(['-', '.']).to_string();
        if name.is_empty() {
            "sandbox".to_string()
        } else {
            name.chars().take(MAX_NAME_BYTES).collect()
        }
    }

    pub fn dir(&self, root: &Path) -> PathBuf {
        root.join(&self.name)
    }

    pub fn config_path(&self, root: &Path) -> PathBuf {
        self.dir(root).join(CONFIG_FILE)
    }

    /// Root filesystem used by this sandbox, without creating it.
    pub fn rootfs_path(&self, config: &Config) -> PathBuf {
        self.rootfs_image.clone().unwrap_or_else(|| {
            self.dir(&config.sandboxes_dir)
                .join(format!("rootfs.{}", config.rootfs_format.as_str()))
        })
    }

    /// Ensure the sandbox has a private root filesystem.
    ///
    /// The first start copies (or reflinks) the global image into the sandbox.
    /// Concurrent sandboxes therefore never open the same writable disk.
    pub fn prepare_rootfs(&self, config: &Config) -> Result<PathBuf> {
        let dest = self.rootfs_path(config);
        if crate::images::is_regular_file(&dest) {
            return Ok(dest);
        }
        if self.rootfs_image.is_some() {
            bail!("sandbox rootfs_image {} does not exist", dest.display());
        }
        if !crate::images::is_regular_file(&config.rootfs_image) {
            bail!("base rootfs {} is missing", config.rootfs_image.display());
        }
        create_private_dir(dest.parent().unwrap_or(&dest))?;
        let tmp = dest.with_file_name(format!(".rootfs.part-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let result = (|| -> Result<()> {
            let mut source = fs::OpenOptions::new()
                .read(true)
                .open(&config.rootfs_image)
                .with_context(|| format!("failed to open {}", config.rootfs_image.display()))?;
            let mut target = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&tmp)
                .with_context(|| format!("failed to create {}", tmp.display()))?;
            // FICLONE is supported by btrfs/XFS/overlayfs. Fall back to a
            // full copy on filesystems without reflink support.
            use std::os::unix::io::AsRawFd;
            const FICLONE: libc::c_ulong = 0x4004_9409;
            let cloned =
                unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0;
            if !cloned {
                target.set_len(0)?;
                std::io::copy(&mut source, &mut target)?;
            }
            target.sync_all()?;
            fs::rename(&tmp, &dest)
                .with_context(|| format!("failed to install {}", dest.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        Ok(dest)
    }

    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported sandbox config version {} (expected {CONFIG_VERSION})",
                self.version
            );
        }
        if self.project.as_os_str().is_empty() || !self.project.is_absolute() {
            bail!("sandbox project must be an absolute path");
        }
        if self.workspace.as_os_str().is_empty() || !self.workspace.is_absolute() {
            bail!("sandbox workspace must be an absolute path");
        }
        if let Some(rootfs) = &self.rootfs_image {
            if rootfs.as_os_str().is_empty() || !rootfs.is_absolute() {
                bail!("sandbox rootfs_image must be an absolute path");
            }
            // Cloud Hypervisor packs this path into `path=...,image_type=...`.
            if rootfs.to_string_lossy().contains(',') {
                bail!("sandbox rootfs_image must not contain a comma");
            }
        }
        if self.vcpus == 0 || self.vcpus > 32 {
            bail!("sandbox vcpus must be between 1 and 32");
        }
        if self.memory_mib < 128 {
            bail!("sandbox memory_mib must be at least 128");
        }
        crate::cloud_init::safe_token(&self.shared_tag)
            .then_some(())
            .context("sandbox shared_tag contains unsafe characters")?;
        if self.shared_tag.len() > 36 {
            bail!("sandbox shared_tag must be at most 36 bytes");
        }
        crate::cloud_init::safe_mount(&self.guest_mount)
            .then_some(())
            .context("sandbox guest_mount must be an absolute, simple path")?;
        if let Some(source) = &self.source_sandbox {
            validate_name(source)?;
        }
        Ok(())
    }
}

/// Validate a sandbox name. It becomes a directory name, so traversal and
/// hidden entries are rejected.
pub fn validate_name(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("sandbox name cannot be empty");
    }
    if name.len() > MAX_NAME_BYTES {
        bail!("sandbox name is too long");
    }
    if name.starts_with('.') || name.contains('/') || name.contains('\\') {
        bail!("sandbox name must not contain path separators or start with '.'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("sandbox name may only contain ASCII letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

/// Filesystem-only store for sandbox configuration and cloned workspaces.
#[derive(Debug, Clone)]
pub struct SandboxStore {
    root: PathBuf,
    defaults: Config,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub removed: bool,
    /// Workspace kept on disk when files were not deleted.
    pub kept_workspace: Option<PathBuf>,
}

impl SandboxStore {
    pub fn new(config: Config) -> Self {
        Self {
            root: config.sandboxes_dir.clone(),
            defaults: config,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_root(&self) -> Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(&self.root)
            && metadata.file_type().is_symlink()
        {
            bail!(
                "refusing symlinked sandboxes directory {}",
                self.root.display()
            );
        }
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", self.root.display()))?;
        }
        Ok(())
    }

    pub fn dir(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }

    pub fn config_path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.dir(name)?.join(CONFIG_FILE))
    }

    pub fn load(&self, name: &str) -> Result<SandboxConfig> {
        let dir = self.dir(name)?;
        let path = dir.join(CONFIG_FILE);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("sandbox '{name}' has no config at {}", path.display()))?;
        let config: SandboxConfig =
            toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
        config.validate()?;
        if config.name != name {
            bail!(
                "sandbox config {} names '{}' but the directory is '{name}'",
                path.display(),
                config.name
            );
        }
        Ok(config)
    }

    pub fn save(&self, config: &SandboxConfig) -> Result<()> {
        config.validate()?;
        let dir = self.dir(&config.name)?;
        create_private_dir(&dir)?;
        let path = dir.join(CONFIG_FILE);
        let raw = toml::to_string_pretty(config)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<SandboxConfig>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.root.display()));
            }
        };
        let mut sandboxes = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if validate_name(&name).is_err() {
                continue;
            }
            let path = entry.path();
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                _ => continue,
            }
            if !path.join(CONFIG_FILE).is_file() {
                continue;
            }
            if let Ok(config) = self.load(&name) {
                sandboxes.push(config);
            }
        }
        sandboxes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sandboxes)
    }

    /// Create a sandbox from an existing project.
    ///
    /// With `clone = true`, the project is copied into the sandbox and marked
    /// ephemeral. Without it, the sandbox exports the project directly.
    pub fn create_from_project(
        &self,
        project: &Path,
        name: Option<&str>,
        harness: Option<Harness>,
        clone: bool,
    ) -> Result<SandboxConfig> {
        let project = project
            .canonicalize()
            .with_context(|| format!("project {} does not exist", project.display()))?;
        if !project.is_dir() {
            bail!("{} is not a directory", project.display());
        }
        let name = name
            .map(str::to_string)
            .unwrap_or_else(|| SandboxConfig::suggested_name(&project));
        validate_name(&name)?;
        self.ensure_root()?;
        let dir = self.root.join(&name);
        if dir.exists() {
            bail!("sandbox '{name}' already exists");
        }
        let workspace = if clone {
            let workspace = dir.join(WORKSPACE_DIR);
            if let Err(err) = self.clone_workspace(&project, &workspace) {
                // A failed clone must not leave a half-initialised sandbox
                // behind that blocks the next attempt with "already exists".
                let _ = fs::remove_dir_all(&dir);
                return Err(err);
            }
            workspace
        } else {
            project.clone()
        };
        let config = self.build_config(&name, &project, workspace, harness, clone, None, None);
        if let Err(err) = self.save(&config) {
            let _ = fs::remove_dir_all(&dir);
            return Err(err);
        }
        Ok(config)
    }

    /// Clone an existing sandbox into a new ephemeral workspace.
    pub fn clone_from(
        &self,
        source: &SandboxConfig,
        name: &str,
        harness: Option<Harness>,
    ) -> Result<SandboxConfig> {
        validate_name(name)?;
        self.ensure_root()?;
        let dir = self.root.join(name);
        if dir.exists() {
            bail!("sandbox '{name}' already exists");
        }
        let workspace = dir.join(WORKSPACE_DIR);
        if let Err(err) = self.clone_workspace(&source.workspace, &workspace) {
            let _ = fs::remove_dir_all(&dir);
            return Err(err);
        }
        let config = self.build_config(
            name,
            &source.project,
            workspace,
            harness.or(Some(source.harness)),
            true,
            Some(source.name.clone()),
            Some(source),
        );
        if let Err(err) = self.save(&config) {
            let _ = fs::remove_dir_all(&dir);
            return Err(err);
        }
        Ok(config)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_config(
        &self,
        name: &str,
        project: &Path,
        workspace: PathBuf,
        harness: Option<Harness>,
        ephemeral: bool,
        source_sandbox: Option<String>,
        template: Option<&SandboxConfig>,
    ) -> SandboxConfig {
        let base = template.cloned().unwrap_or_else(|| SandboxConfig {
            harness: self.defaults.harness,
            vcpus: self.defaults.vcpus,
            memory_mib: self.defaults.memory_mib,
            shared_tag: self.defaults.shared_tag.clone(),
            guest_mount: self.defaults.guest_mount.clone(),
            auto_branch: self.defaults.auto_branch,
            security: self.defaults.security,
            extra_packages: self.defaults.extra_packages.clone(),
            ..SandboxConfig::default()
        });
        SandboxConfig {
            version: CONFIG_VERSION,
            name: name.to_string(),
            project: project.to_path_buf(),
            workspace,
            rootfs_image: None,
            harness: harness.unwrap_or(base.harness),
            vcpus: base.vcpus,
            memory_mib: base.memory_mib,
            shared_tag: base.shared_tag,
            guest_mount: base.guest_mount,
            ephemeral,
            source_sandbox,
            created_at: Utc::now(),
            auto_branch: base.auto_branch,
            security: base.security,
            extra_packages: base.extra_packages,
        }
    }

    /// Remove a sandbox directory.
    ///
    /// A workspace outside the sandbox directory (the original project of a
    /// normal sandbox) is never deleted. `delete_files = false` keeps an
    /// in-sandbox clone by renaming it to `<name>-files-<timestamp>`.
    pub fn delete(&self, name: &str, delete_files: bool) -> Result<DeleteOutcome> {
        let config = self.load(name)?;
        let dir = self.dir(name)?;
        let workspace = config.workspace.clone();
        let workspace_inside = path_is_inside(&workspace, &dir);
        if delete_files && workspace_inside {
            remove_dir_all(&dir)?;
            return Ok(DeleteOutcome {
                removed: true,
                kept_workspace: None,
            });
        }
        if !workspace_inside {
            // The sandbox directory only contains config/runtime; the source
            // project is outside and is left untouched.
            remove_dir_all(&dir)?;
            return Ok(DeleteOutcome {
                removed: true,
                kept_workspace: None,
            });
        }
        // Keep the clone workspace, but unregister the sandbox. Rename first:
        // if that fails, the original sandbox (with its hegn.toml) is still
        // intact and the delete can be retried.
        let kept = unique_kept_path(&self.root, name);
        fs::rename(&dir, &kept)
            .with_context(|| format!("failed to move {} to {}", dir.display(), kept.display()))?;
        // The kept directory is no longer listed as a sandbox once hegn.toml
        // is gone; runtime sockets are removed best-effort.
        let _ = remove_runtime(&kept);
        Ok(DeleteOutcome {
            removed: true,
            kept_workspace: Some(kept.join(WORKSPACE_DIR)),
        })
    }

    fn clone_workspace(&self, source: &Path, dest: &Path) -> Result<()> {
        if !source.is_dir() {
            bail!("workspace source {} is not a directory", source.display());
        }
        if path_is_inside(dest, source) || path_is_inside(source, dest) {
            bail!(
                "refusing to clone {} into itself ({})",
                source.display(),
                dest.display()
            );
        }
        create_private_dir(dest.parent().unwrap_or(dest))?;
        if is_git_repo(source) {
            let status = Command::new("git")
                .arg("clone")
                .arg("--no-hardlinks")
                .arg("--quiet")
                .arg("--")
                .arg(source)
                .arg(dest)
                .status()
                .context("failed to run git clone (is git installed?)")?;
            if !status.success() {
                let _ = fs::remove_dir_all(dest);
                bail!("git clone from {} failed with {status}", source.display());
            }
            copy_git_identity(source, dest);
            return Ok(());
        }
        copy_tree(source, dest)?;
        Ok(())
    }
}

fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Give a clone the source repository's commit identity when available.
///
/// `git clone` copies tracked files and the object database, but not the local
/// `user.name`/`user.email`. Without this, an agent's first commit in a clone
/// fails with "Author identity unknown".
fn copy_git_identity(source: &Path, dest: &Path) {
    let mut found = false;
    for key in ["user.name", "user.email"] {
        let Some(value) = git_config_get(source, key) else {
            continue;
        };
        if git_config_set(dest, key, &value).is_ok() {
            found = true;
        }
    }
    if !found {
        let _ = git_config_set(dest, "user.name", "hegn agent");
        let _ = git_config_set(dest, "user.email", "agent@hegn.local");
    }
}

fn git_config_get(repo: &Path, key: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn git_config_set(repo: &Path, key: &str, value: &str) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["config", "--local", key, value])
        .status()
        .context("failed to run git config")?;
    if !status.success() {
        bail!("git config {key} failed with {status}");
    }
    Ok(())
}

fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    create_private_dir(dest)?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&from)?;
            std::os::unix::fs::symlink(target, &to)
                .with_context(|| format!("failed to copy symlink {}", from.display()))?;
        } else if metadata.is_dir() {
            copy_tree(&from, &to)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to).with_context(|| format!("failed to copy {}", from.display()))?;
        }
    }
    Ok(())
}

fn remove_runtime(dir: &Path) -> Result<()> {
    for name in ["run", CONFIG_FILE] {
        let path = dir.join(name);
        if path.is_dir() {
            remove_dir_all(&path)?;
        } else {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn remove_dir_all(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(path).with_context(|| format!("failed to delete {}", path.display()))
}

fn unique_kept_path(root: &Path, name: &str) -> PathBuf {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let mut candidate = root.join(format!("{name}-files-{stamp}"));
    let mut counter = 1;
    while candidate.exists() {
        candidate = root.join(format!("{name}-files-{stamp}-{counter}"));
        counter += 1;
    }
    candidate
}

fn path_is_inside(path: &Path, parent: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    path != parent && path.starts_with(&parent)
}

fn create_private_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        bail!("refusing symlinked directory {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::fs::PermissionsExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", path.display()))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.sandboxes_dir = dir.join("sandboxes");
        config.projects_root = dir.join("projects");
        config.harness = Harness::Shell;
        config
    }

    fn project(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join("projects").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("README.md"), b"hello\n").unwrap();
        path
    }

    #[test]
    fn creates_and_lists_a_normal_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let store = SandboxStore::new(cfg);
        let project = project(dir.path(), "demo");
        let sandbox = store
            .create_from_project(&project, None, Some(Harness::Codex), false)
            .unwrap();
        assert_eq!(sandbox.name, "demo");
        assert_eq!(sandbox.workspace, project.canonicalize().unwrap());
        assert!(!sandbox.ephemeral);
        assert_eq!(store.list().unwrap().len(), 1);
        let loaded = store.load("demo").unwrap();
        assert_eq!(loaded.harness, Harness::Codex);
        assert_eq!(loaded.vcpus, cfg_default_vcpus());
    }

    fn cfg_default_vcpus() -> u8 {
        Config::default().vcpus
    }

    #[test]
    fn clone_creates_an_isolated_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let store = SandboxStore::new(cfg);
        let project = project(dir.path(), "demo");
        let base = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        let clone = store.clone_from(&base, "demo-agent-1", None).unwrap();
        assert!(clone.ephemeral);
        assert_eq!(clone.source_sandbox.as_deref(), Some("demo"));
        assert!(clone.workspace.starts_with(store.root()));
        assert!(clone.workspace.join("README.md").is_file());
        // Changing the clone does not change the source project.
        fs::write(clone.workspace.join("README.md"), b"clone\n").unwrap();
        assert_eq!(fs::read(project.join("README.md")).unwrap(), b"hello\n");
    }

    #[test]
    fn deleting_never_removes_the_source_project() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let store = SandboxStore::new(cfg);
        let project = project(dir.path(), "demo");
        store
            .create_from_project(&project, None, None, false)
            .unwrap();
        let outcome = store.delete("demo", true).unwrap();
        assert!(outcome.removed);
        assert!(project.join("README.md").is_file());
        assert!(!store.dir("demo").unwrap().exists());
    }

    #[test]
    fn keeping_clone_files_renames_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let store = SandboxStore::new(cfg);
        let project = project(dir.path(), "demo");
        let base = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        let clone = store.clone_from(&base, "demo-agent-1", None).unwrap();
        let outcome = store.delete("demo-agent-1", false).unwrap();
        let kept = outcome.kept_workspace.unwrap();
        assert!(kept.join("README.md").is_file());
        assert!(!store.dir("demo-agent-1").unwrap().exists());
        assert!(!kept.starts_with(store.root().join("demo-agent-1")));
        let _ = clone;
    }

    #[test]
    fn rejects_unsafe_names_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let store = SandboxStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        for name in ["", ".hidden", "../x", "a/b", "a b"] {
            assert!(
                store
                    .create_from_project(&project, Some(name), None, false)
                    .is_err(),
                "{name}"
            );
        }
        store
            .create_from_project(&project, Some("one"), None, false)
            .unwrap();
        assert!(
            store
                .create_from_project(&project, Some("one"), None, false)
                .is_err()
        );
    }

    #[test]
    fn git_clone_copies_the_source_commit_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = SandboxStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&project)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Demo Agent"]);
        git(&["config", "user.email", "demo@example.com"]);
        git(&["add", "README.md"]);
        git(&["commit", "-qm", "init"]);

        let base = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        let clone = store.clone_from(&base, "demo-agent-2", None).unwrap();
        assert_eq!(
            git_config_get(&clone.workspace, "user.name").as_deref(),
            Some("Demo Agent")
        );
        assert_eq!(
            git_config_get(&clone.workspace, "user.email").as_deref(),
            Some("demo@example.com")
        );
    }

    #[test]
    fn failed_clone_does_not_leave_a_partial_sandbox_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = SandboxStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        // A `.git` entry that is not a repository makes `git clone` fail.
        fs::create_dir(project.join(".git")).unwrap();
        let err = store
            .create_from_project(&project, Some("broken"), None, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("git clone"), "{err}");
        assert!(!store.dir("broken").unwrap().exists());
    }

    #[test]
    fn non_git_clone_copies_files_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let store = SandboxStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        std::os::unix::fs::symlink("README.md", project.join("link")).unwrap();
        let clone = store
            .clone_from(
                &store
                    .create_from_project(&project, None, None, false)
                    .unwrap(),
                "copy",
                None,
            )
            .unwrap();
        assert!(clone.workspace.join("README.md").is_file());
        assert!(clone.workspace.join("link").is_symlink());
    }

    #[test]
    fn prepares_a_private_rootfs_copy_without_touching_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.rootfs_image = dir.path().join("base.qcow2");
        cfg.rootfs_format = crate::config::RootfsFormat::Qcow2;
        fs::write(&cfg.rootfs_image, b"rootfs-bytes").unwrap();
        let store = SandboxStore::new(cfg.clone());
        let project = project(dir.path(), "demo");
        let sandbox = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        let rootfs = sandbox.prepare_rootfs(&cfg).unwrap();
        assert!(rootfs.starts_with(store.dir("demo").unwrap()));
        assert_eq!(fs::read(&rootfs).unwrap(), b"rootfs-bytes");
        assert_eq!(fs::read(&cfg.rootfs_image).unwrap(), b"rootfs-bytes");
        // A second call reuses the same private copy.
        assert_eq!(sandbox.prepare_rootfs(&cfg).unwrap(), rootfs);
    }

    #[test]
    fn config_file_is_private_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = SandboxStore::new(config(dir.path()));
        let project = project(dir.path(), "demo");
        let mut sandbox = store
            .create_from_project(&project, None, None, false)
            .unwrap();
        sandbox.memory_mib = 4096;
        store.save(&sandbox).unwrap();
        let path = store.config_path("demo").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(store.load("demo").unwrap().memory_mib, 4096);
    }
}
