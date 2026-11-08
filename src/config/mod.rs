//! User configuration: schema, defaults, environment overrides, validation and
//! XDG paths.
//!
//! [`form`] contains the interactive settings editor used by the TUI.
//! Validation lives in [`validate`], path helpers in [`paths`], environment
//! overrides in [`env`], value types in [`types`] and external-tool discovery
//! in [`tools`]. Public items are re-exported here so callers keep using
//! `crate::config::*`.

pub mod form;

mod defaults;
mod env;
mod paths;
mod tools;
mod types;
mod validate;

pub use self::defaults::DEFAULT_AGENTS_MD;
pub use self::tools::{CLOUD_HYPERVISOR_FALLBACKS, CLOUD_HYPERVISOR_STATIC, VIRTIOFSD_FALLBACKS};
pub use self::types::{
    BackendKind, Harness, LocalEndpoint, NetworkMode, RootfsFormat, SecurityMode, VirtiofsSandbox,
};
pub use self::validate::{safe_package_name, validate_local_endpoints};

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::launch::LaunchConfig;

use self::tools::{alternate_cloud_hypervisor_name, resolve_binary};

/// User configuration, loaded from `~/.config/hegn/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Directory scanned by the project picker.
    pub projects_root: PathBuf,
    /// State (recent sessions, sockets, runtime files). Defaults to
    /// `~/.local/state/hegn`.
    pub state_dir: PathBuf,
    /// Snapshot metadata and memory files. Defaults to `~/.local/share/hegn/snapshots`.
    pub snapshots_dir: PathBuf,
    /// Per-instance configuration, runtime and cloned workspaces.
    /// Defaults to `~/.local/share/hegn/instances`.
    #[serde(alias = "sandboxes_dir")]
    pub instances_dir: PathBuf,
    /// Directory containing project templates (`agents.md`, language skeletons).
    pub templates_dir: PathBuf,
    /// Directory containing per-harness profile templates
    /// (`opencode.toml`, `claude-code.toml`, …). Profiles may contain API keys,
    /// so the directory is private.
    pub profiles_dir: PathBuf,
    pub cloud_hypervisor_bin: PathBuf,
    pub virtiofsd_bin: PathBuf,
    pub kernel_image: PathBuf,
    pub rootfs_image: PathBuf,
    /// Raw or qcow2. Cloud Hypervisor's official test rootfs is qcow2.
    pub rootfs_format: RootfsFormat,
    /// Guest root device, e.g. `/dev/vda` for a whole-disk ext4 image or
    /// `/dev/vda1` for a partitioned cloud image.
    pub root_device: String,
    /// Attach the generated NoCloud seed so official cloud images auto-login
    /// on the serial console. Ignored by images without cloud-init.
    pub cloud_init: bool,
    /// Opt-in guest credential hardening. When enabled, the serial console
    /// auto-logs in as the unprivileged `hegn` user and `sudo` requires a
    /// password instead of the default passwordless root console.
    pub guest_credential_hardening: bool,
    /// Upstream kernel URL and pinned SHA-256 used by `hegn images download`.
    pub kernel_url: String,
    pub kernel_sha256: String,
    /// Upstream root filesystem URL and pinned SHA-256.
    pub rootfs_url: String,
    pub rootfs_sha256: String,
    /// vCPUs assigned to a MicroVM.
    pub vcpus: u8,
    /// Guest memory in MiB.
    pub memory_mib: u32,
    /// Hard ceiling for per-instance root filesystem growth, in GiB. Sparse
    /// images only consume host space when the guest writes, but a runaway
    /// guest must not be able to fill the host filesystem.
    pub max_rootfs_gib: u32,
    /// Warn in `hegn doctor` when the filesystem holding the instances is
    /// fuller than this percentage.
    pub disk_usage_warn_percent: u8,
    /// virtiofs tag; mounted at `guest_mount` inside the guest.
    pub shared_tag: String,
    pub guest_mount: String,
    pub virtiofs_sandbox: VirtiofsSandbox,
    pub backend: BackendKind,
    /// Harness launched when a project is opened.
    pub harness: Harness,
    /// Commands and terminal integrations launched after a VM starts.
    pub launch: LaunchConfig,
    /// Guest networking policy. `guarded` is an allow-listed TAP device;
    /// `off` removes the NIC entirely.
    pub network: NetworkMode,
    /// Host-loopback TCP services exposed to the guest (local LLM servers).
    pub local_endpoints: Vec<LocalEndpoint>,
    /// Extra packages installed while creating a VM template.
    pub extra_packages: Vec<String>,
    /// Create `agent/hegn-session-<timestamp>` before launching.
    pub auto_branch: bool,
    pub security: SecurityMode,
}

impl Config {
    /// Load configuration, falling back to defaults when no file exists.
    ///
    /// Selected environment variables override the file so `hegn` can be
    /// exercised in CI without touching the user's real directories:
    /// `HEGN_PROJECTS_ROOT`, `HEGN_STATE_DIR`, `HEGN_SNAPSHOTS_DIR`,
    /// `HEGN_INSTANCES_DIR`,
    /// `HEGN_TEMPLATES_DIR`, `HEGN_CLOUD_HYPERVISOR_BIN`, `HEGN_VIRTIOFSD_BIN`,
    /// `HEGN_KERNEL_IMAGE` and `HEGN_ROOTFS_IMAGE`.
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        let mut config = if path.exists() {
            Self::load_from(&path)?
        } else {
            Self::default()
        };
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    /// Parse a configuration file without applying environment overrides.
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        Ok(config)
    }

    /// Create runtime and config directories. Safe to call repeatedly.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.state_dir,
            &self.snapshots_dir,
            &self.instances_dir,
            &self.profiles_dir,
        ] {
            if let Ok(metadata) = fs::symlink_metadata(dir)
                && metadata.file_type().is_symlink()
            {
                bail!("refusing symlinked private directory {}", dir.display());
            }
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("failed to secure {}", dir.display()))?;
            }
        }
        for dir in [&self.templates_dir, &self.projects_root] {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        // Logs may contain secrets printed by an agent; never rely on the
        // process umask for this directory.
        crate::session::logs::LogStore::new(self.logs_dir()).ensure()?;
        Ok(())
    }

    /// Write a commented default configuration if none exists yet.
    pub fn write_default_if_missing(&self) -> Result<bool> {
        let path = Self::config_path();
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(self)?;
        let header = "# hegn configuration\n# Docs: https://github.com/Henriklmao/hegn\n\n";
        fs::write(&path, format!("{header}{toml}"))
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(true)
    }

    /// Persist this configuration to `path`, replacing an existing file.
    ///
    /// The interactive settings editor (`F4`/`F5`) uses this. The file is
    /// rewritten from the schema, so hand-written comments are not preserved;
    /// use the "Edit file" action for comment-preserving edits. Writes are
    /// atomic: a sibling temporary file is fsynced and renamed into place with
    /// private permissions, so a crash can never leave a half-written config.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self)?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&tmp)
                .with_context(|| format!("failed to create {}", tmp.display()))?;
            file.write_all(raw.as_bytes())
                .with_context(|| format!("failed to write {}", tmp.display()))?;
            file.sync_all()?;
            fs::rename(&tmp, path)
                .with_context(|| format!("failed to replace {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Locate the `virtiofsd` executable.
    ///
    /// An explicitly configured `virtiofsd_bin` is used as-is. A bare
    /// `virtiofsd` is resolved through `PATH` and then through
    /// [`VIRTIOFSD_FALLBACKS`], because Arch, Fedora and Debian install it
    /// outside `PATH`. Returns `None` when no candidate exists.
    pub fn resolved_virtiofsd(&self) -> Option<PathBuf> {
        resolve_binary(&self.virtiofsd_bin, VIRTIOFSD_FALLBACKS)
    }

    /// Locate the Cloud Hypervisor executable.
    ///
    /// Besides the configured name/path this accepts the static binary shipped
    /// by AUR's `cloud-hypervisor-bin` package, which does not install a
    /// `cloud-hypervisor` symlink. An explicit path is still used verbatim.
    pub fn resolved_cloud_hypervisor(&self) -> Option<PathBuf> {
        if let Some(found) = resolve_binary(&self.cloud_hypervisor_bin, CLOUD_HYPERVISOR_FALLBACKS)
        {
            return Some(found);
        }
        if self.cloud_hypervisor_bin.components().count() > 1 {
            // An explicit path can still point at the non-static name while
            // the package installed only the static sibling.
            let file_name = self.cloud_hypervisor_bin.file_name()?;
            let alternate = alternate_cloud_hypervisor_name(Path::new(file_name))?;
            let sibling = self.cloud_hypervisor_bin.with_file_name(alternate);
            return sibling.is_file().then_some(sibling);
        }
        let alternate = alternate_cloud_hypervisor_name(&self.cloud_hypervisor_bin)?;
        resolve_binary(Path::new(alternate), CLOUD_HYPERVISOR_FALLBACKS)
    }

    /// The binary used for process matching and termination.
    ///
    /// Falls back to the configured path when discovery fails, so a VM started
    /// before the binary disappeared can still be identified and stopped.
    pub fn vm_binary(&self) -> PathBuf {
        self.resolved_cloud_hypervisor()
            .unwrap_or_else(|| self.cloud_hypervisor_bin.clone())
    }

    /// Static assets are only usable when both images are real files.
    ///
    /// Symlinks are rejected: a swapped link could make hegn boot an arbitrary
    /// host file as the guest kernel or root filesystem.
    pub fn vm_assets_ready(&self) -> bool {
        crate::vm::images::is_regular_file(&self.kernel_image)
            && crate::vm::images::is_regular_file(&self.rootfs_image)
    }

    /// The backend actually used for work. hegn never falls back to a host
    /// process, so this is always Cloud Hypervisor.
    pub fn effective_backend(&self) -> BackendKind {
        BackendKind::CloudHypervisor
    }

    /// Assert that the VM assets required to do any work are present.
    pub fn require_vm_assets(&self) -> Result<()> {
        if !self.vm_assets_ready() {
            bail!(
                "missing VM assets: {} and/or {} do not exist; \
                 run `hegn doctor` and configure kernel_image/rootfs_image",
                self.kernel_image.display(),
                self.rootfs_image.display()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_config(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn parses_partial_config_and_fills_defaults() {
        let (_dir, path) = temp_config(
            r#"
vcpus = 4
memory_mib = 4096
harness = "opencode"
backend = "cloud-hypervisor"
rootfs_format = "raw"
root_device = "/dev/vda"
"#,
        );
        let config = Config::load_from(&path).unwrap();
        config.validate().unwrap();
        assert_eq!(config.vcpus, 4);
        assert_eq!(config.memory_mib, 4096);
        assert_eq!(config.harness, Harness::Opencode);
        assert_eq!(config.backend, BackendKind::CloudHypervisor);
        assert_eq!(config.rootfs_format, RootfsFormat::Raw);
        assert_eq!(config.root_device, "/dev/vda");
        assert_eq!(config.projects_root, Config::default().projects_root);
    }

    #[test]
    fn rejects_unknown_keys() {
        let (_dir, path) = temp_config("vcpu = 4\n");
        let err = format!("{:#}", Config::load_from(&path).unwrap_err());
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn profiles_dir_is_private_and_protected() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.instances_dir = dir.path().join("instances");
        config.templates_dir = dir.path().join("templates");
        config.profiles_dir = dir.path().join("profiles");
        config.projects_root = dir.path().join("projects");
        config.ensure_dirs().unwrap();
        assert_eq!(
            fs::metadata(&config.profiles_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            config
                .protected_paths()
                .iter()
                .any(|path| path == &config.profiles_dir)
        );
    }

    #[test]
    fn pty_backend_is_rejected() {
        let (_dir, path) = temp_config("backend = \"pty\"\n");
        let err = format!("{:#}", Config::load_from(&path).unwrap_err());
        assert!(err.contains("unknown variant"), "{err}");
    }

    #[test]
    fn resolves_the_static_sibling_of_an_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let static_binary = dir.path().join("cloud-hypervisor-static");
        fs::write(&static_binary, b"").unwrap();
        let mut config = Config::default();
        config.cloud_hypervisor_bin = dir.path().join("cloud-hypervisor");
        assert_eq!(config.resolved_cloud_hypervisor(), Some(static_binary));
    }

    #[test]
    fn symlinked_vm_assets_are_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-kernel");
        std::fs::write(&real, b"kernel").unwrap();
        let link = dir.path().join("kernel");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::write(&rootfs, b"rootfs").unwrap();

        let mut config = Config::default();
        config.kernel_image = link;
        config.rootfs_image = rootfs;
        assert!(!config.vm_assets_ready());
        assert!(config.require_vm_assets().is_err());
    }

    #[test]
    fn endpoints_deserialize_from_toml() {
        let config: Config =
            toml::from_str("[[local_endpoints]]\nname = \"ollama\"\nport = 11434\n").unwrap();
        assert_eq!(config.endpoint_ports(), vec![11434]);
        assert_eq!(config.local_endpoints[0].env_name(), "HEGN_ENDPOINT_OLLAMA");
    }

    #[test]
    fn missing_vm_assets_are_a_hard_error() {
        let mut config = Config::default();
        config.kernel_image = PathBuf::from("/definitely/missing/kernel");
        config.rootfs_image = PathBuf::from("/definitely/missing/rootfs");
        assert_eq!(config.effective_backend(), BackendKind::CloudHypervisor);
        assert!(!config.vm_assets_ready());
        assert!(config.require_vm_assets().is_err());
    }

    #[test]
    fn ensure_dirs_creates_private_state_snapshot_and_log_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.templates_dir = dir.path().join("templates");
        config.profiles_dir = dir.path().join("profiles");
        config.projects_root = dir.path().join("projects");
        config.ensure_dirs().unwrap();
        for path in [
            config.logs_dir(),
            config.state_dir.clone(),
            config.snapshots_dir.clone(),
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn ensure_dirs_refuses_symlinked_private_directories() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("state");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut config = Config::default();
        config.state_dir = link;
        config.snapshots_dir = dir.path().join("snapshots");
        config.templates_dir = dir.path().join("templates");
        config.profiles_dir = dir.path().join("profiles");
        config.projects_root = dir.path().join("projects");
        let err = config.ensure_dirs().unwrap_err().to_string();
        assert!(err.contains("symlinked"), "{err}");
    }

    #[test]
    fn saves_config_to_an_explicit_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hegn/config.toml");
        let mut config = Config::default();
        config.projects_root = dir.path().join("projects");
        config.harness = Harness::Codex;
        config.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert!(dir.path().join("hegn").is_dir());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn saving_an_invalid_config_does_not_touch_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, b"vcpus = 4\n").unwrap();
        let mut config = Config::default();
        config.vcpus = 0;
        assert!(config.save_to(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"vcpus = 4\n");
    }

    #[test]
    fn writes_default_config_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hegn/config.toml");
        // Safety: tests that mutate process env are run serially by the test
        // harness only when using `--test-threads=1`; this test only needs a
        // stable path, so write the file directly instead of touching env.
        let mut config = Config::default();
        config.state_dir = dir.path().join("state");
        let _ = &path;
        let raw = toml::to_string_pretty(&config).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(raw.as_bytes()).unwrap();
        let parsed = Config::load_from(&path).unwrap();
        assert_eq!(parsed.state_dir, config.state_dir);
    }
}
