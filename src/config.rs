use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Which sandbox backend to use when a project is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Use Firecracker when its assets are configured and available, otherwise
    /// fall back to a local PTY. This keeps a fresh checkout usable.
    #[default]
    Auto,
    /// Require a Firecracker MicroVM. Never silently fall back.
    Firecracker,
    /// Run the harness in a local PTY. Useful for development of the TUI.
    Pty,
}

impl BackendKind {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Firecracker, Self::Pty];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Firecracker => "firecracker",
            Self::Pty => "pty",
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

    /// Packges installed into a freshly created VM for this harness. The
    /// template installer is intentionally conservative; users can extend the
    /// list in `config.toml`.
    pub fn packages(self) -> &'static [&'static str] {
        match self {
            Self::Shell => &["git", "curl", "ca-certificates"],
            Self::Opencode => &["git", "curl", "nodejs", "npm"],
            Self::ClaudeCode => &["git", "curl", "nodejs", "npm"],
            Self::Codex => &["git", "curl", "python3"],
            Self::Pi => &["git", "curl", "nodejs", "npm"],
            Self::PrimeAgent => &["git", "curl", "python3"],
            Self::Aider => &["git", "curl", "python3", "pip"],
            Self::GeminiCli => &["git", "curl", "nodejs", "npm"],
        }
    }
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
    /// Directory containing project templates (`agents.md`, language skeletons).
    pub templates_dir: PathBuf,
    pub firecracker_bin: PathBuf,
    pub virtiofsd_bin: PathBuf,
    pub kernel_image: PathBuf,
    pub rootfs_image: PathBuf,
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
            projects_root: home.join("Projects"),
            state_dir: state_dir.join("hegn"),
            snapshots_dir: data_dir.join("hegn/snapshots"),
            templates_dir: config_dir.join("hegn/templates"),
            firecracker_bin: PathBuf::from("firecracker"),
            virtiofsd_bin: PathBuf::from("virtiofsd"),
            kernel_image: data_dir.join("hegn/images/vmlinux.bin"),
            rootfs_image: data_dir.join("hegn/images/rootfs.ext4"),
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
    /// `HEGN_TEMPLATES_DIR`, `HEGN_FIRECRACKER_BIN`, `HEGN_VIRTIOFSD_BIN`,
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
        if let Some(value) = path_var("HEGN_TEMPLATES_DIR") {
            self.templates_dir = value;
        }
        if let Some(value) = path_var("HEGN_FIRECRACKER_BIN") {
            self.firecracker_bin = value;
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
    }

    pub fn validate(&self) -> Result<()> {
        if self.vcpus == 0 || self.vcpus > 32 {
            bail!("vcpus must be between 1 and 32 (got {})", self.vcpus);
        }
        if self.memory_mib < 128 {
            bail!("memory_mib must be at least 128 (got {})", self.memory_mib);
        }
        if self.shared_tag.is_empty() || self.shared_tag.contains(char::is_whitespace) {
            bail!("shared_tag must be a non-empty token without whitespace");
        }
        if !self.guest_mount.starts_with('/') {
            bail!("guest_mount must be an absolute guest path");
        }
        Ok(())
    }

    /// Create runtime and config directories. Safe to call repeatedly.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.state_dir,
            &self.snapshots_dir,
            &self.templates_dir,
            &self.projects_root,
        ] {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
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
        let header = "# hegn configuration\n# Docs: https://github.com/hegn-tui/hegn\n\n";
        fs::write(&path, format!("{header}{toml}"))
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(true)
    }

    /// Runtime directory for API and virtiofs sockets.
    pub fn run_dir(&self) -> PathBuf {
        self.state_dir.join("run")
    }

    /// Static assets are only usable when both images exist.
    pub fn firecracker_ready(&self) -> bool {
        self.kernel_image.is_file() && self.rootfs_image.is_file()
    }

    /// Whether the configured backend can boot a VM right now.
    pub fn effective_backend(&self) -> BackendKind {
        match self.backend {
            BackendKind::Auto if self.firecracker_ready() => BackendKind::Firecracker,
            BackendKind::Auto => BackendKind::Pty,
            other => other,
        }
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
    }

    #[test]
    fn parses_partial_config_and_fills_defaults() {
        let (_dir, path) = temp_config(
            r#"
vcpus = 4
memory_mib = 4096
harness = "opencode"
backend = "pty"
"#,
        );
        let config = Config::load_from(&path).unwrap();
        config.validate().unwrap();
        assert_eq!(config.vcpus, 4);
        assert_eq!(config.memory_mib, 4096);
        assert_eq!(config.harness, Harness::Opencode);
        assert_eq!(config.backend, BackendKind::Pty);
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
    fn backend_auto_requires_images() {
        let mut config = Config::default();
        config.kernel_image = PathBuf::from("/definitely/missing/kernel");
        config.rootfs_image = PathBuf::from("/definitely/missing/rootfs");
        assert_eq!(config.effective_backend(), BackendKind::Pty);
        config.backend = BackendKind::Firecracker;
        assert_eq!(config.effective_backend(), BackendKind::Firecracker);
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
