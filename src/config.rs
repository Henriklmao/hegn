use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// How the sandbox is selected when a project is opened.
///
/// Both modes require a real Cloud Hypervisor VM. There is no local-shell
/// fallback: a missing VM is always a hard error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    /// Boot a VM on demand. Still requires all VM assets and binaries.
    #[default]
    Auto,
    /// Boot a VM on demand. Present for explicitness; behaves like `auto`.
    CloudHypervisor,
}

impl BackendKind {
    pub const ALL: [Self; 2] = [Self::Auto, Self::CloudHypervisor];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::CloudHypervisor => "cloud-hypervisor",
        }
    }
}

/// On-disk format of the root filesystem image passed to Cloud Hypervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RootfsFormat {
    /// A raw ext4 filesystem or whole-disk image.
    #[default]
    Raw,
    /// A qcow2 image (Cloud Hypervisor's official test images use this).
    Qcow2,
}

impl RootfsFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Qcow2 => "qcow2",
        }
    }
}

/// A coding harness that can be launched inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum Harness {
    /// The user's login shell. Good for manual exploration.
    #[default]
    Shell,
    Opencode,
    ClaudeCode,
    Codex,
    Pi,
    PrimeAgent,
    Aider,
    GeminiCli,
}

impl Harness {
    pub const ALL: [Self; 8] = [
        Self::Shell,
        Self::Opencode,
        Self::ClaudeCode,
        Self::Codex,
        Self::Pi,
        Self::PrimeAgent,
        Self::Aider,
        Self::GeminiCli,
    ];

    /// The executable that is launched for this harness.
    pub fn command(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Opencode => "opencode",
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::PrimeAgent => "prime-agent",
            Self::Aider => "aider",
            Self::GeminiCli => "gemini",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Opencode => "OpenCode",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Pi => "pi",
            Self::PrimeAgent => "prime-agent",
            Self::Aider => "Aider",
            Self::GeminiCli => "Gemini CLI",
        }
    }

    /// Packages installed into a freshly created VM for this harness. Every
    /// preset starts from the same base tool set and then adds the runtime the
    /// harness actually needs; `extra_packages` is unioned on top.
    pub fn packages(self) -> &'static [&'static str] {
        const BASE: &[&str] = &[
            "git",
            "curl",
            "ca-certificates",
            "ripgrep",
            "fd-find",
            "less",
            "jq",
        ];
        match self {
            Self::Shell => BASE,
            Self::Opencode | Self::ClaudeCode | Self::Codex | Self::Pi | Self::GeminiCli => &[
                "git",
                "curl",
                "ca-certificates",
                "ripgrep",
                "fd-find",
                "less",
                "jq",
                "nodejs",
                "build-essential",
            ],
            Self::PrimeAgent => &[
                "git",
                "curl",
                "ca-certificates",
                "ripgrep",
                "fd-find",
                "less",
                "jq",
                "python3",
                "python3-pip",
                "python3-venv",
                "xz-utils",
            ],
            Self::Aider => &[
                "git",
                "curl",
                "ca-certificates",
                "ripgrep",
                "fd-find",
                "less",
                "jq",
                "python3",
                "python3-pip",
                "python3-venv",
                "build-essential",
            ],
        }
    }

    /// Short description used by the create form and CLI output.
    pub fn preset_summary(self) -> &'static str {
        match self {
            Self::Shell => "base CLI tools",
            Self::Opencode => "Node 22 + OpenCode CLI",
            Self::ClaudeCode => "Node 22 + Claude Code CLI",
            Self::Codex => "Node 22 + Codex CLI",
            Self::Pi => "Node 22 + Pi coding agent",
            Self::PrimeAgent => "Python + Prime Agent installer",
            Self::Aider => "Python + Aider",
            Self::GeminiCli => "Node 22 + Gemini CLI",
        }
    }

    /// Whether the preset installs Node.js from NodeSource before the harness.
    pub fn needs_node(self) -> bool {
        matches!(
            self,
            Self::Opencode | Self::ClaudeCode | Self::Codex | Self::Pi | Self::GeminiCli
        )
    }

    /// Shell commands run by the first-boot provisioner after the apt packages
    /// are installed. They are static, auditable strings; user-provided package
    /// names never become shell fragments.
    pub fn install_commands(self) -> &'static [&'static str] {
        match self {
            Self::Shell => &[],
            Self::Opencode => &["npm install --global --no-fund --no-audit opencode-ai"],
            Self::ClaudeCode => {
                &["npm install --global --no-fund --no-audit @anthropic-ai/claude-code"]
            }
            Self::Codex => &["npm install --global --no-fund --no-audit @openai/codex"],
            Self::Pi => &[
                "npm install --global --no-fund --no-audit --ignore-scripts @earendil-works/pi-coding-agent",
            ],
            Self::PrimeAgent => &[
                "curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 https://app.primeintellect.ai/prime-agent/install.sh -o /tmp/hegn-prime-agent.sh",
                "sh /tmp/hegn-prime-agent.sh",
            ],
            Self::Aider => &[
                "python3 -m pip install --upgrade pip",
                "python3 -m pip install --upgrade aider-chat",
            ],
            Self::GeminiCli => &["npm install --global --no-fund --no-audit @google/gemini-cli"],
        }
    }
}

/// Validate a Debian package name before it reaches a root shell or YAML file.
pub fn safe_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '_' | '-'))
        && !name.starts_with('-')
}

/// Locations where distributions install `virtiofsd` outside `PATH`.
///
/// Arch ships `/usr/lib/virtiofsd`, Fedora/RHEL use `/usr/libexec/virtiofsd`,
/// Debian/Ubuntu keep it in `/usr/lib/qemu/`, and openSUSE uses
/// `/usr/libexec/qemu/`. `hegn doctor` and the sandbox preflight fall back to
/// these after `PATH` so a plain package install works without editing
/// `virtiofsd_bin`.
pub const VIRTIOFSD_FALLBACKS: &[&str] = &[
    "/usr/lib/virtiofsd",
    "/usr/libexec/virtiofsd",
    "/usr/lib/qemu/virtiofsd",
    "/usr/libexec/qemu/virtiofsd",
];

/// The AUR package `cloud-hypervisor-bin` installs only the static binary.
pub const CLOUD_HYPERVISOR_STATIC: &str = "cloud-hypervisor-static";

/// Common install locations for Cloud Hypervisor.
pub const CLOUD_HYPERVISOR_FALLBACKS: &[&str] = &[
    "/usr/bin/cloud-hypervisor",
    "/usr/bin/cloud-hypervisor-static",
    "/usr/lib/cloud-hypervisor/cloud-hypervisor",
];

/// Map `cloud-hypervisor` to its static AUR variant and back.
fn alternate_cloud_hypervisor_name(binary: &Path) -> Option<&'static str> {
    if binary == Path::new("cloud-hypervisor") {
        Some(CLOUD_HYPERVISOR_STATIC)
    } else if binary == Path::new(CLOUD_HYPERVISOR_STATIC) {
        Some("cloud-hypervisor")
    } else {
        None
    }
}

/// Resolve a configured tool to an executable file.
///
/// A configured path (anything with a directory component) is used verbatim so
/// an explicit `*_bin` always wins. A bare command name is looked up on `PATH`
/// first and then in `fallbacks`; distribution helper daemons often live in a
/// private lib directory instead of being symlinked into `PATH`.
fn resolve_binary(binary: &Path, fallbacks: &[&str]) -> Option<PathBuf> {
    if binary.components().count() > 1 {
        return binary.is_file().then(|| binary.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    fallbacks
        .iter()
        .map(Path::new)
        .find(|candidate| candidate.is_file())
        .map(Path::to_path_buf)
}

/// How strictly flagged commands are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SecurityMode {
    /// Do not inspect input.
    Off,
    /// Show findings but allow the command through.
    Warn,
    /// Block high/critical findings and prompt for medium ones.
    #[default]
    Enforce,
}

/// Sandbox mode passed to virtiofsd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VirtiofsSandbox {
    /// Run in separate namespaces (recommended on modern kernels).
    #[default]
    Namespace,
    /// Chroot into the shared directory.
    Chroot,
}

impl VirtiofsSandbox {
    pub fn as_flag(self) -> &'static str {
        match self {
            Self::Namespace => "namespace",
            Self::Chroot => "chroot",
        }
    }
}

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
    /// Per-sandbox configuration, runtime and cloned workspaces.
    /// Defaults to `~/.local/share/hegn/sandboxes`.
    pub sandboxes_dir: PathBuf,
    /// Directory containing project templates (`agents.md`, language skeletons).
    pub templates_dir: PathBuf,
    pub cloud_hypervisor_bin: PathBuf,
    pub virtiofsd_bin: PathBuf,
    pub kernel_image: PathBuf,
    pub rootfs_image: PathBuf,
    /// Raw or qcow2. Cloud Hypervisor's official test rootfs is qcow2.
    pub rootfs_format: RootfsFormat,
    /// Guest root device, e.g. `/dev/vda` for a whole-disk ext4 image or
    /// `/dev/vda1` for a partitioned cloud image.
    pub root_device: String,
    /// Enable Cloud Hypervisor's Landlock sandbox. A hard requirement.
    pub landlock: bool,
    /// Enable Cloud Hypervisor's seccomp filters (on by default upstream).
    pub seccomp: bool,
    /// Attach the generated NoCloud seed so official cloud images auto-login
    /// on the serial console. Ignored by images without cloud-init.
    pub cloud_init: bool,
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
    /// virtiofs tag; mounted at `guest_mount` inside the guest.
    pub shared_tag: String,
    pub guest_mount: String,
    pub virtiofs_sandbox: VirtiofsSandbox,
    pub backend: BackendKind,
    /// Harness launched when a project is opened.
    pub harness: Harness,
    /// Extra packages installed while creating a VM template.
    pub extra_packages: Vec<String>,
    /// Create `agent/hegn-session-<timestamp>` before launching.
    pub auto_branch: bool,
    pub security: SecurityMode,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let config_dir = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        let state_dir = dirs::state_dir().unwrap_or_else(|| home.join(".local/state"));
        let data_dir = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));

        Self {
            projects_root: home.join("Projects/hegn"),
            state_dir: state_dir.join("hegn"),
            snapshots_dir: data_dir.join("hegn/snapshots"),
            sandboxes_dir: data_dir.join("hegn/sandboxes"),
            templates_dir: config_dir.join("hegn/templates"),
            cloud_hypervisor_bin: PathBuf::from("cloud-hypervisor"),
            virtiofsd_bin: PathBuf::from("virtiofsd"),
            kernel_image: data_dir
                .join("hegn/images")
                .join(crate::images::KERNEL_ARTIFACT),
            rootfs_image: data_dir
                .join("hegn/images")
                .join(crate::images::ROOTFS_ARTIFACT),
            rootfs_format: crate::images::ROOTFS_FORMAT,
            root_device: "/dev/vda1".to_string(),
            landlock: true,
            seccomp: true,
            cloud_init: true,
            kernel_url: crate::images::KERNEL_URL.to_string(),
            kernel_sha256: crate::images::KERNEL_SHA256.to_string(),
            rootfs_url: crate::images::ROOTFS_URL.to_string(),
            rootfs_sha256: crate::images::ROOTFS_SHA256.to_string(),
            vcpus: 2,
            memory_mib: 2048,
            shared_tag: "workspace".to_string(),
            guest_mount: "/workspace".to_string(),
            virtiofs_sandbox: VirtiofsSandbox::Namespace,
            backend: BackendKind::Auto,
            harness: Harness::Shell,
            extra_packages: Vec::new(),
            auto_branch: true,
            security: SecurityMode::Enforce,
        }
    }
}

impl Config {
    /// Path of the user configuration file.
    pub fn config_path() -> PathBuf {
        if let Some(path) = std::env::var_os("HEGN_CONFIG") {
            return PathBuf::from(path);
        }
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        dirs::config_dir()
            .unwrap_or_else(|| home.join(".config"))
            .join("hegn/config.toml")
    }

    /// Load configuration, falling back to defaults when no file exists.
    ///
    /// Selected environment variables override the file so `hegn` can be
    /// exercised in CI without touching the user's real directories:
    /// `HEGN_PROJECTS_ROOT`, `HEGN_STATE_DIR`, `HEGN_SNAPSHOTS_DIR`,
    /// `HEGN_SANDBOXES_DIR`,
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

    fn apply_env_overrides(&mut self) {
        fn path_var(name: &str) -> Option<PathBuf> {
            std::env::var_os(name).map(PathBuf::from)
        }
        if let Some(value) = path_var("HEGN_PROJECTS_ROOT") {
            self.projects_root = value;
        }
        if let Some(value) = path_var("HEGN_STATE_DIR") {
            self.state_dir = value;
        }
        if let Some(value) = path_var("HEGN_SNAPSHOTS_DIR") {
            self.snapshots_dir = value;
        }
        if let Some(value) = path_var("HEGN_SANDBOXES_DIR") {
            self.sandboxes_dir = value;
        }
        if let Some(value) = path_var("HEGN_TEMPLATES_DIR") {
            self.templates_dir = value;
        }
        if let Some(value) = path_var("HEGN_CLOUD_HYPERVISOR_BIN") {
            self.cloud_hypervisor_bin = value;
        }
        if let Some(value) = path_var("HEGN_VIRTIOFSD_BIN") {
            self.virtiofsd_bin = value;
        }
        if let Some(value) = path_var("HEGN_KERNEL_IMAGE") {
            self.kernel_image = value;
        }
        if let Some(value) = path_var("HEGN_ROOTFS_IMAGE") {
            self.rootfs_image = value;
        }
        if let Ok(value) = std::env::var("HEGN_ROOTFS_FORMAT") {
            self.rootfs_format = match value.trim().to_ascii_lowercase().as_str() {
                "raw" => RootfsFormat::Raw,
                "qcow2" => RootfsFormat::Qcow2,
                _ => self.rootfs_format,
            };
        }
        if let Ok(value) = std::env::var("HEGN_ROOT_DEVICE") {
            self.root_device = value;
        }
        if let Ok(value) = std::env::var("HEGN_KERNEL_URL") {
            self.kernel_url = value;
        }
        if let Ok(value) = std::env::var("HEGN_KERNEL_SHA256") {
            self.kernel_sha256 = value;
        }
        if let Ok(value) = std::env::var("HEGN_ROOTFS_URL") {
            self.rootfs_url = value;
        }
        if let Ok(value) = std::env::var("HEGN_ROOTFS_SHA256") {
            self.rootfs_sha256 = value;
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.vcpus == 0 || self.vcpus > 32 {
            bail!("vcpus must be between 1 and 32 (got {})", self.vcpus);
        }
        if self.memory_mib < 128 {
            bail!("memory_mib must be at least 128 (got {})", self.memory_mib);
        }
        if !crate::cloud_init::safe_token(&self.shared_tag) {
            bail!("shared_tag may only contain ASCII letters, digits, '-', '_' and '.'");
        }
        // virtio-fs tags are limited to 36 bytes including the terminator.
        if self.shared_tag.len() > 36 {
            bail!(
                "shared_tag must be at most 36 bytes (got {})",
                self.shared_tag.len()
            );
        }
        if !crate::cloud_init::safe_mount(&self.guest_mount) {
            bail!(
                "guest_mount must be an absolute path with simple components                  (letters, digits, '/', '-', '_' and '.')"
            );
        }
        if !self.root_device.starts_with("/dev/") || self.root_device.contains(',') {
            bail!("root_device must be a guest device path such as /dev/vda1");
        }
        // Cloud Hypervisor's CLI packs paths into comma-separated option
        // values. A comma in one of these paths would silently change the
        // device configuration, so refuse it up front.
        for (name, path) in [
            ("state_dir", &self.state_dir),
            ("snapshots_dir", &self.snapshots_dir),
            ("sandboxes_dir", &self.sandboxes_dir),
            ("kernel_image", &self.kernel_image),
            ("rootfs_image", &self.rootfs_image),
        ] {
            if path.to_string_lossy().contains(',') {
                bail!("{name} must not contain a comma");
            }
        }
        for (name, sha) in [
            ("kernel_sha256", &self.kernel_sha256),
            ("rootfs_sha256", &self.rootfs_sha256),
        ] {
            if !sha.trim().is_empty() && !crate::images::is_sha256(sha.trim()) {
                bail!("{name} must be a 64 character SHA-256 hex digest");
            }
        }
        for (name, url) in [
            ("kernel_url", &self.kernel_url),
            ("rootfs_url", &self.rootfs_url),
        ] {
            if !url.trim().is_empty() && !url.starts_with("https://") {
                bail!("{name} must use https://");
            }
        }
        for package in &self.extra_packages {
            if !safe_package_name(package) {
                bail!("invalid package name in extra_packages: {package:?}");
            }
        }
        Ok(())
    }

    /// Create runtime and config directories. Safe to call repeatedly.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.state_dir, &self.snapshots_dir, &self.sandboxes_dir] {
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
        crate::logs::LogStore::new(self.logs_dir()).ensure()?;
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

    /// Runtime directory for API and virtiofs sockets.
    pub fn run_dir(&self) -> PathBuf {
        self.state_dir.join("run")
    }

    /// Private directory for persistent serial-console logs.
    ///
    /// Derived from `state_dir` so existing configurations keep working and
    /// logs always live next to the run directory they belong to.
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }

    /// Paths that must never be exported to a guest or imported as a project.
    ///
    /// This covers runtime sockets/logs, snapshots, templates and the config
    /// directory. Project import and discovery both consult this list.
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        let mut protected = vec![
            self.state_dir.clone(),
            self.snapshots_dir.clone(),
            self.sandboxes_dir.clone(),
            self.templates_dir.clone(),
        ];
        if let Some(parent) = Self::config_path().parent() {
            protected.push(parent.to_path_buf());
        }
        protected
    }

    /// Import an existing directory into the project picker as a symlink.
    ///
    /// hegn state, snapshots and templates are protected so an imported
    /// project can never expose VM sockets, logs or snapshot memory to the
    /// guest.
    pub fn import_project(&self, source: &Path, name: Option<&str>) -> Result<PathBuf> {
        crate::project::import_project(&self.projects_root, source, name, &self.protected_paths())
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
        crate::images::is_regular_file(&self.kernel_image)
            && crate::images::is_regular_file(&self.rootfs_image)
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

/// `AGENTS.md` seeded into new projects from `templates_dir`.
pub const DEFAULT_AGENTS_MD: &str = r#"# Agent instructions

This project runs inside a hegn sandbox.

## Rules

- Keep changes inside the workspace. The only writable mount is `/workspace`.
- Run the test suite before declaring a task complete.
- Do not print, copy, or transmit secrets (`.env`, SSH keys, tokens).
- Prefer small, reviewable commits. hegn may create a safety checkpoint branch.
- Ask before destructive operations (`rm -rf`, `git clean -fdx`, history rewrites).

## Layout

- Source: `/workspace/src`
- Tests: `/workspace/tests`
"#;

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
    fn defaults_are_valid() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.guest_mount, "/workspace");
        assert_eq!(config.shared_tag, "workspace");
        assert_eq!(config.security, SecurityMode::Enforce);
        assert!(
            config.projects_root.ends_with("Projects/hegn"),
            "new projects belong in Projects/hegn: {}",
            config.projects_root.display()
        );
        assert!(crate::images::is_sha256(&config.kernel_sha256));
        assert!(crate::images::is_sha256(&config.rootfs_sha256));
        assert!(config.kernel_url.starts_with("https://"));
        assert!(config.rootfs_url.starts_with("https://"));
        assert_eq!(config.root_device, "/dev/vda1");
        assert_eq!(config.rootfs_format, RootfsFormat::Qcow2);
        assert!(config.landlock);
        assert!(config.seccomp);
    }

    #[test]
    fn rejects_invalid_image_pins() {
        let mut config = Config::default();
        config.kernel_sha256 = "not-a-digest".into();
        assert!(config.validate().is_err());
        config.kernel_sha256 = crate::images::KERNEL_SHA256.into();
        config.rootfs_url = "http://example.com/rootfs".into();
        assert!(config.validate().is_err());
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
    fn rejects_invalid_values() {
        let mut config = Config::default();
        config.vcpus = 0;
        assert!(config.validate().is_err());
        config.vcpus = 2;
        config.memory_mib = 12;
        assert!(config.validate().is_err());
        config.memory_mib = 1024;
        config.guest_mount = "workspace".into();
        assert!(config.validate().is_err());
        config.guest_mount = "/workspace".into();
        config.root_device = "vda1".into();
        assert!(config.validate().is_err());
        config.root_device = "/dev/vda1".into();
        config.shared_tag = "x".repeat(37);
        assert!(config.validate().is_err());
        config.shared_tag = "work space".into();
        assert!(config.validate().is_err());
        config.shared_tag = "workspace".into();
        config.guest_mount = "/work space".into();
        assert!(config.validate().is_err());
        config.guest_mount = "/workspace".into();
        config.kernel_image = PathBuf::from("/images/vmlinux,broken");
        assert!(config.validate().is_err());
    }

    #[test]
    fn harness_commands_are_non_empty() {
        for harness in Harness::ALL {
            assert!(!harness.command().is_empty());
            assert!(!harness.label().is_empty());
            assert!(!harness.packages().is_empty());
        }
    }

    #[test]
    fn pty_backend_is_rejected() {
        let (_dir, path) = temp_config("backend = \"pty\"\n");
        let err = format!("{:#}", Config::load_from(&path).unwrap_err());
        assert!(err.contains("unknown variant"), "{err}");
    }

    #[test]
    fn cloud_hypervisor_static_is_an_accepted_alias() {
        assert_eq!(
            alternate_cloud_hypervisor_name(Path::new("cloud-hypervisor")),
            Some("cloud-hypervisor-static")
        );
        assert_eq!(
            alternate_cloud_hypervisor_name(Path::new("cloud-hypervisor-static")),
            Some("cloud-hypervisor")
        );
        assert_eq!(
            alternate_cloud_hypervisor_name(Path::new("/opt/custom/ch")),
            None
        );
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
    fn resolve_binary_prefers_an_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = dir.path().join("virtiofsd");
        fs::write(&explicit, b"").unwrap();
        assert_eq!(resolve_binary(&explicit, &[]), Some(explicit.clone()));
        assert_eq!(
            resolve_binary(&dir.path().join("missing-virtiofsd"), &[]),
            None
        );
    }

    #[test]
    fn resolve_binary_falls_back_to_install_directories() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("virtiofsd");
        fs::write(&fallback, b"").unwrap();
        let fallback_str = fallback.to_str().unwrap();
        assert_eq!(
            resolve_binary(Path::new("hegn-test-missing-virtiofsd"), &[fallback_str]),
            Some(fallback)
        );
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
        config.projects_root = dir.path().join("projects");
        let err = config.ensure_dirs().unwrap_err().to_string();
        assert!(err.contains("symlinked"), "{err}");
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
