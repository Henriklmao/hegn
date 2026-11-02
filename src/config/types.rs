//! Configuration value types: the enums stored in `config.toml`, plus
//! [`LocalEndpoint`] and its helpers.

use serde::{Deserialize, Serialize};

/// How the instance is selected when a project is opened.
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
    pub const ALL: [Self; 2] = [Self::Raw, Self::Qcow2];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Qcow2 => "qcow2",
        }
    }
}

/// A coding harness that can be launched inside the instance.
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

    /// Stable file-name fragment for per-harness profile templates.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Opencode => "opencode",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::PrimeAgent => "prime-agent",
            Self::Aider => "aider",
            Self::GeminiCli => "gemini-cli",
        }
    }

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

/// Whether the guest gets a guarded network device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    /// No network device at all (air-gapped).
    Off,
    /// A host-guarded TAP device: public HTTP(S) and public DNS only.
    #[default]
    Guarded,
}

impl NetworkMode {
    pub const ALL: [Self; 2] = [Self::Off, Self::Guarded];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Guarded => "guarded",
        }
    }
}

/// A host-local TCP service the guest may reach through the guarded network.
///
/// The service is expected to listen on `127.0.0.1:<port>` on the host (the
/// default for Ollama, llama.cpp, LM Studio, vLLM, …). `hegn network setup`
/// installs a per-TAP DNAT rule so the guest can use the same port on its TAP
/// host address instead of the loopback address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalEndpoint {
    /// Short label, also used for the guest's `HEGN_ENDPOINT_<NAME>` variable.
    pub name: String,
    /// TCP port on the host loopback that is exposed on the TAP host address.
    pub port: u16,
}

impl LocalEndpoint {
    pub fn env_name(&self) -> String {
        let mut name = String::from("HEGN_ENDPOINT_");
        for ch in self.name.chars() {
            if ch.is_ascii_alphanumeric() {
                name.push(ch.to_ascii_uppercase());
            } else {
                name.push('_');
            }
        }
        name
    }

    /// URL the guest uses. `host` is the instance's TAP host address.
    pub fn url(&self, host: std::net::Ipv4Addr) -> String {
        format!("http://{host}:{}", self.port)
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

impl SecurityMode {
    pub const ALL: [Self; 3] = [Self::Off, Self::Warn, Self::Enforce];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Enforce => "enforce",
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_commands_are_non_empty() {
        for harness in Harness::ALL {
            assert!(!harness.command().is_empty());
            assert!(!harness.label().is_empty());
            assert!(!harness.packages().is_empty());
        }
    }
}
