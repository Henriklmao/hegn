//! Fields, pages and row tables for the settings modal.
//!
//! The same field enum covers the global `config.toml` and an instance's
//! `hegn.toml`; fields that do not apply to the current kind render as `—` and
//! never appear in the row list.

use crate::config::{Harness, LocalEndpoint};

/// Which configuration file the modal edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    Global,
    Instance,
}

/// Pages (tabs) inside the settings modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPage {
    General,
    Profiles,
    LocalLlm,
}

impl ConfigPage {
    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Profiles => "Profiles",
            Self::LocalLlm => "Local LLM",
        }
    }
}

/// A single editable setting. The same enum covers both file kinds; values that
/// do not apply to the current kind render as `—` and are never in the row list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigField {
    Harness,
    Vcpus,
    Memory,
    AutoBranch,
    RootfsFormat,
    RootDevice,
    CloudInit,
    KernelImage,
    RootfsImage,
    SharedTag,
    GuestMount,
    Network,
    Security,
    Backend,
    CloudHypervisorBin,
    VirtiofsdBin,
    ProjectsRoot,
    StateDir,
    SnapshotsDir,
    InstancesDir,
    TemplatesDir,
    ProfilesDir,
    ExtraPackages,
    /// Host-loopback LLM endpoints exposed through the guarded network.
    LocalEndpoints,
    /// A harness profile template (`profiles/<harness>.toml`).
    Profile(Harness),
    // Instance-only, read-only context.
    Project,
    Workspace,
    RootfsPath,
    NetworkIndex,
}

/// How a field responds to `Enter`, the arrow keys and a mouse click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Cycles through a fixed list of variants.
    Choice,
    /// A numeric value with a step size for the arrow keys.
    Integer { min: i64, max: i64, step: i64 },
    /// An on/off switch.
    Bool,
    /// Free-form text, edited inline.
    Text,
    /// Perform an action instead of editing a value (open a profile, …).
    Action,
    /// Informational value; never changes.
    Locked,
}

impl ConfigField {
    pub fn label(self) -> &'static str {
        match self {
            Self::Harness => "Harness",
            Self::Vcpus => "vCPUs",
            Self::Memory => "Memory (MiB)",
            Self::AutoBranch => "Session branch",
            Self::RootfsFormat => "Rootfs format",
            Self::RootDevice => "Root device",
            Self::CloudInit => "Cloud-init seed",
            Self::KernelImage => "Kernel image",
            Self::RootfsImage => "Rootfs image",
            Self::SharedTag => "Shared tag",
            Self::GuestMount => "Guest mount",
            Self::Network => "Network",
            Self::Security => "Security",
            Self::Backend => "Backend",
            Self::CloudHypervisorBin => "Hypervisor bin",
            Self::VirtiofsdBin => "virtiofsd bin",
            Self::ProjectsRoot => "Projects root",
            Self::StateDir => "State dir",
            Self::SnapshotsDir => "Snapshots dir",
            Self::InstancesDir => "Instances dir",
            Self::TemplatesDir => "Templates dir",
            Self::ProfilesDir => "Profiles dir",
            Self::ExtraPackages => "Extra packages",
            Self::LocalEndpoints => "Endpoints (name=port)",
            Self::Profile(harness) => harness.label(),
            Self::Project => "Project",
            Self::Workspace => "Workspace",
            Self::RootfsPath => "Rootfs",
            Self::NetworkIndex => "Network slot",
        }
    }

    pub fn kind(self) -> FieldKind {
        match self {
            Self::Harness | Self::RootfsFormat | Self::Network | Self::Security | Self::Backend => {
                FieldKind::Choice
            }
            Self::Vcpus => FieldKind::Integer {
                min: 1,
                max: 32,
                step: 1,
            },
            Self::Memory => FieldKind::Integer {
                min: 128,
                max: 262_144,
                step: 256,
            },
            Self::AutoBranch | Self::CloudInit => FieldKind::Bool,
            Self::Profile(_) => FieldKind::Action,
            Self::Project | Self::Workspace | Self::RootfsPath | Self::NetworkIndex => {
                FieldKind::Locked
            }
            _ => FieldKind::Text,
        }
    }

    pub(super) fn is_path(self) -> bool {
        matches!(
            self,
            Self::KernelImage
                | Self::RootfsImage
                | Self::CloudHypervisorBin
                | Self::VirtiofsdBin
                | Self::ProjectsRoot
                | Self::StateDir
                | Self::SnapshotsDir
                | Self::InstancesDir
                | Self::TemplatesDir
                | Self::ProfilesDir
        )
    }
}

/// One line of the modal. Group headers and notes are decorative and skipped by
/// keyboard and wheel navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigRow {
    Group(&'static str),
    Field(ConfigField),
    Note(&'static str),
}

const GENERAL_ROWS: &[ConfigRow] = &[
    ConfigRow::Group("Runtime"),
    ConfigRow::Field(ConfigField::Harness),
    ConfigRow::Field(ConfigField::Vcpus),
    ConfigRow::Field(ConfigField::Memory),
    ConfigRow::Field(ConfigField::AutoBranch),
    ConfigRow::Group("Storage"),
    ConfigRow::Field(ConfigField::RootfsFormat),
    ConfigRow::Field(ConfigField::RootDevice),
    ConfigRow::Field(ConfigField::CloudInit),
    ConfigRow::Field(ConfigField::KernelImage),
    ConfigRow::Field(ConfigField::RootfsImage),
    ConfigRow::Group("Guest sharing"),
    ConfigRow::Field(ConfigField::SharedTag),
    ConfigRow::Field(ConfigField::GuestMount),
    ConfigRow::Group("Network & security"),
    ConfigRow::Field(ConfigField::Network),
    ConfigRow::Field(ConfigField::Security),
    ConfigRow::Note("landlock and seccomp stay enabled (required)"),
    ConfigRow::Group("Tools"),
    ConfigRow::Field(ConfigField::Backend),
    ConfigRow::Field(ConfigField::CloudHypervisorBin),
    ConfigRow::Field(ConfigField::VirtiofsdBin),
    ConfigRow::Group("Directories"),
    ConfigRow::Field(ConfigField::ProjectsRoot),
    ConfigRow::Field(ConfigField::StateDir),
    ConfigRow::Field(ConfigField::SnapshotsDir),
    ConfigRow::Field(ConfigField::InstancesDir),
    ConfigRow::Field(ConfigField::TemplatesDir),
    ConfigRow::Field(ConfigField::ProfilesDir),
    ConfigRow::Group("Packages"),
    ConfigRow::Field(ConfigField::ExtraPackages),
];

const LOCAL_LLM_ROWS: &[ConfigRow] = &[
    ConfigRow::Group("Host services"),
    ConfigRow::Field(ConfigField::LocalEndpoints),
    ConfigRow::Note("loopback only: host 127.0.0.1:<port>"),
    ConfigRow::Note("guest URL: http://<tap-ip>:<port>"),
    ConfigRow::Note("Ollama also exports OLLAMA_HOST in the guest"),
    ConfigRow::Note("apply with: sudo hegn network setup"),
];

const PROFILE_ROWS: &[ConfigRow] = &[
    ConfigRow::Group("Harness profiles"),
    ConfigRow::Note("Enter opens the profile TOML; missing templates are created"),
    ConfigRow::Note("host files and skills are copied into a per-instance bundle"),
    ConfigRow::Field(ConfigField::Profile(Harness::Opencode)),
    ConfigRow::Field(ConfigField::Profile(Harness::ClaudeCode)),
    ConfigRow::Field(ConfigField::Profile(Harness::Codex)),
    ConfigRow::Field(ConfigField::Profile(Harness::Pi)),
    ConfigRow::Field(ConfigField::Profile(Harness::PrimeAgent)),
    ConfigRow::Field(ConfigField::Profile(Harness::Aider)),
    ConfigRow::Field(ConfigField::Profile(Harness::GeminiCli)),
    ConfigRow::Note("CLI: hegn profile show <harness> · hegn skills sync-back <instance>"),
];

const INSTANCE_ROWS: &[ConfigRow] = &[
    ConfigRow::Group("Runtime"),
    ConfigRow::Field(ConfigField::Harness),
    ConfigRow::Field(ConfigField::Vcpus),
    ConfigRow::Field(ConfigField::Memory),
    ConfigRow::Field(ConfigField::AutoBranch),
    ConfigRow::Group("Guest sharing"),
    ConfigRow::Field(ConfigField::SharedTag),
    ConfigRow::Field(ConfigField::GuestMount),
    ConfigRow::Group("Packages"),
    ConfigRow::Field(ConfigField::ExtraPackages),
    ConfigRow::Group("Instance (read-only)"),
    ConfigRow::Field(ConfigField::Project),
    ConfigRow::Field(ConfigField::Workspace),
    ConfigRow::Field(ConfigField::RootfsPath),
    ConfigRow::Field(ConfigField::NetworkIndex),
];

pub fn rows(kind: ConfigKind) -> &'static [ConfigRow] {
    rows_for(kind, ConfigPage::General)
}

/// Pages available for a file kind. Endpoints are host-wide, so an instance's
/// config has only the general page.
pub fn pages(kind: ConfigKind) -> &'static [ConfigPage] {
    match kind {
        ConfigKind::Global => &[
            ConfigPage::General,
            ConfigPage::Profiles,
            ConfigPage::LocalLlm,
        ],
        ConfigKind::Instance => &[ConfigPage::General],
    }
}

pub fn rows_for(kind: ConfigKind, page: ConfigPage) -> &'static [ConfigRow] {
    match (kind, page) {
        (ConfigKind::Global, ConfigPage::General) => GENERAL_ROWS,
        (ConfigKind::Global, ConfigPage::Profiles) => PROFILE_ROWS,
        (ConfigKind::Global, ConfigPage::LocalLlm) => LOCAL_LLM_ROWS,
        (ConfigKind::Instance, _) => INSTANCE_ROWS,
    }
}

/// Find the display row for a field. Used by tests and by callers that only
/// know the semantic setting they want to focus.
pub fn row_of(kind: ConfigKind, field: ConfigField) -> Option<usize> {
    rows(kind)
        .iter()
        .position(|row| matches!(row, ConfigRow::Field(candidate) if *candidate == field))
}

/// Index of the first selectable field on a page.
pub(super) fn first_field(kind: ConfigKind, page: ConfigPage) -> usize {
    rows_for(kind, page)
        .iter()
        .position(|row| matches!(row, ConfigRow::Field(_)))
        .unwrap_or(0)
}

/// How a boolean field is displayed in the modal.
pub(super) fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

/// How a package list is displayed in the modal.
pub(super) fn list_value(items: &[String]) -> String {
    if items.is_empty() {
        "—".to_string()
    } else {
        items.join(", ")
    }
}

/// How a local-endpoint list is displayed in the modal.
pub(super) fn endpoints_value(endpoints: &[LocalEndpoint]) -> String {
    if endpoints.is_empty() {
        "—".to_string()
    } else {
        endpoints
            .iter()
            .map(|endpoint| format!("{}={}", endpoint.name, endpoint.port))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use crate::config::form::ConfigForm;
    use crate::config::form::test_support::global_config;
    use crate::vm::instance_config::InstanceStore;

    #[test]
    fn instance_form_has_no_endpoint_page() {
        let dir = tempfile::tempdir().unwrap();
        let config = global_config(dir.path());
        let store = InstanceStore::new(config.clone());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let instance = store
            .create_from_project(&project, Some("demo"), None, false)
            .unwrap();
        let form = ConfigForm::instance(instance, config);
        assert_eq!(form.pages(), &[super::ConfigPage::General]);
        assert_eq!(form.value(super::ConfigField::LocalEndpoints), "—");
    }
}
