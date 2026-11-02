//! Shared data types and the [`App`] state container.
//!
//! These definitions are the contract between the state machine in
//! [`crate::tui::app`] and the pure renderer in [`crate::tui::ui`]. They carry
//! no behaviour beyond small pure helpers on the forms and health line.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::agent::launch::{Integration, LaunchSpec};
use crate::config::form::{ConfigField, ConfigPage};
pub use crate::config::form::{ConfigForm, ConfigKind};
use crate::config::{Config, Harness};
use crate::diagnostics::doctor::DoctorReport;
use crate::project::{GitSummary, Project, ProjectMeta};
use crate::security::{Inspection, SecurityEngine};
use crate::session::console::TerminalConsole;
use crate::session::snapshot::SnapshotMeta;
use crate::session::state::StateStore;
use crate::session::terminal::TerminalBuffer;
use crate::vm::instance::InstanceSummary;

const TOAST_TTL: Duration = Duration::from_secs(6);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Project,
}

/// Sections shown on the project screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSection {
    Instances,
    Snapshots,
    Health,
    Git,
}

impl ProjectSection {
    pub const ALL: [Self; 4] = [Self::Instances, Self::Snapshots, Self::Health, Self::Git];

    pub fn number(self) -> usize {
        match self {
            Self::Instances => 1,
            Self::Snapshots => 2,
            Self::Health => 3,
            Self::Git => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Instances => "instances",
            Self::Snapshots => "snapshots",
            Self::Health => "health",
            Self::Git => "git",
        }
    }

    pub fn focus(self) -> Focus {
        match self {
            Self::Instances => Focus::Instances,
            Self::Snapshots => Focus::Snapshots,
            Self::Health => Focus::Health,
            Self::Git => Focus::Git,
        }
    }

    pub fn from_focus(focus: Focus) -> Self {
        match focus {
            Focus::Snapshots => Self::Snapshots,
            Focus::Health => Self::Health,
            Focus::Git => Self::Git,
            Focus::Projects | Focus::Instances => Self::Instances,
        }
    }
}

/// Terminal geometry on the project screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectView {
    /// One-line terminal strip; the composed project page fills the window.
    Compact,
    /// Terminal on the right at 55%; section headers collapsed on the left.
    Split,
    /// One section expanded on the left at 70%; terminal on the right at 30%.
    Section,
}

/// Vim-style input mode for the terminal view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalMode {
    /// Navigation and hegn shortcuts. Keys are not sent to the guest.
    Normal,
    /// Command line, inspected by tirith-core before it is submitted.
    Insert,
    /// Every key is forwarded to the guest unchanged.
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Projects,
    Instances,
    Snapshots,
    Health,
    Git,
}

/// Side effects that need the terminal to be released (editor/lazygit), an
/// async runtime (snapshots), or the event loop (quit).
#[derive(Debug, Clone)]
pub enum Effect {
    Quit,
    OpenEditor(PathBuf),
    /// Open a harness profile template; missing templates are created first.
    OpenProfileEditor(Harness),
    OpenLazygit(PathBuf),
    RefreshProjects,
    RestoreSnapshot(SnapshotMeta),
    DownloadImages,
    /// Create directories/config, download images and configure the guarded
    /// network as needed. Privileged steps prompt via sudo/pkexec.
    SetupEnvironment,
    /// Ensure an instance for a project and attach its console.
    StartProjectInstance {
        project: PathBuf,
        harness: Harness,
    },
    /// Start (if needed) and attach an existing instance.
    AttachInstance {
        instance: String,
    },
    /// Launch a configured command through a terminal integration.
    LaunchAgent {
        spec: LaunchSpec,
        instance: String,
        project: PathBuf,
    },
    /// Leave the TUI and attach the real serial console client.
    AttachConsole {
        instance: String,
    },
    StopInstance {
        instance: String,
    },
    DeleteInstance {
        instance: String,
        delete_files: bool,
    },
    /// Remove a project from the dashboard, its instances and optionally its
    /// files. Instances are deleted first.
    DeleteProject {
        project: PathBuf,
        delete_files: bool,
    },
    CloneInstance {
        source: String,
        count: usize,
        harness: Harness,
        start: bool,
    },
    RefreshInstances,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub level: ToastLevel,
    pub created: Instant,
}

impl Toast {
    pub(super) fn new(text: impl Into<String>, level: ToastLevel) -> Self {
        Self {
            text: text.into(),
            level,
            created: Instant::now(),
        }
    }

    pub fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.created) > TOAST_TTL
    }
}

/// A launch request held back until the first-boot provisioner is done.
#[derive(Debug, Clone)]
pub struct PendingLaunch {
    pub spec: LaunchSpec,
    pub instance: String,
    pub project: PathBuf,
    pub since: Instant,
    pub provision_seen: bool,
    pub ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    Stopped,
    Starting,
    Running,
    Paused,
    Failed,
}

impl InstanceState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstanceStatus {
    pub state: InstanceState,
    pub backend: &'static str,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub virtiofs: &'static str,
    pub socket: Option<PathBuf>,
    pub note: Option<String>,
    pub started_at: Option<Instant>,
}

/// Last guest-reported health line from the serial log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceHealth {
    pub apt: Option<u32>,
    pub reason: String,
    pub checked: Option<chrono::DateTime<chrono::Utc>>,
}

impl InstanceHealth {
    pub(super) fn parse(text: &str) -> Option<Self> {
        for line in text.lines().rev() {
            let Some(rest) = line.trim().strip_prefix("hegn-health v1 ") else {
                continue;
            };
            let mut health = Self::default();
            for token in rest.split_whitespace() {
                if let Some(value) = token.strip_prefix("apt=") {
                    health.apt = value.parse().ok();
                } else if let Some(value) = token.strip_prefix("reason=") {
                    health.reason = value.to_string();
                } else if let Some(value) = token.strip_prefix("checked=")
                    && let Ok(seconds) = value.parse::<i64>()
                {
                    health.checked = chrono::DateTime::from_timestamp(seconds, 0);
                }
            }
            return Some(health);
        }
        None
    }

    pub fn label(&self) -> String {
        if self.apt.is_none() && !self.reason.is_empty() && self.reason != "ok" {
            return self.reason.clone();
        }
        let apt = match self.apt {
            Some(count) => format!("{count} upgrades"),
            None => "not checked".to_string(),
        };
        match &self.checked {
            Some(checked) => format!("{apt} · checked {}", checked.format("%Y-%m-%d %H:%M")),
            None => apt,
        }
    }
}

impl InstanceStatus {
    pub fn from_config(config: &Config) -> Self {
        let backend = "cloud-hypervisor";
        let virtiofs = if config.vm_assets_ready() {
            "configured"
        } else {
            "missing VM"
        };
        Self {
            state: InstanceState::Stopped,
            backend,
            vcpus: config.vcpus,
            memory_mib: config.memory_mib,
            virtiofs,
            socket: None,
            note: None,
            started_at: None,
        }
    }

    pub fn uptime(&self) -> Option<Duration> {
        self.started_at.map(|start| start.elapsed())
    }
}

/// Buttons rendered in the footer. The UI records their rectangles so mouse
/// clicks can dispatch the same action as the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionId {
    Open,
    Create,
    Import,
    Confirm,
    Cancel,
    NextField,
    SubmitCreate,
    SubmitImport,
    Edit,
    Lazygit,
    Refresh,
    Help,
    HelpSearch,
    HelpSearchStop,
    Quit,
    Back,
    ToggleRaw,
    InsertMode,
    NormalMode,
    ScrollUp,
    NewSnapshot,
    DeleteSnapshot,
    Restore,
    FocusInstances,
    StartInstance,
    StopInstance,
    CloneInstance,
    DeleteInstance,
    DeleteProject,
    DeleteProjectFiles,
    InstanceConfig,
    GlobalConfig,
    ProjectConfig,
    ProjectConfigSave,
    ProjectConfigLaunch,
    MultiAgent,
    Launch,
    KeepFiles,
    EditConfig,
    ReloadConfig,
    Doctor,
    DoctorSetup,
    DownloadImages,
    Allow,
    Checkpoint,
    Deny,
    MultiAgentMore,
    MultiAgentFewer,
    HarnessPrev,
    HarnessNext,
    ToggleCloneStart,
    SaveConfig,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterHit {
    pub rect: Rect,
    pub action: ActionId,
}

/// Placeholder row rectangles for mouse selection, filled by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowHit {
    pub rect: Rect,
    pub index: usize,
}

/// Clickable project section header or tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionHit {
    pub rect: Rect,
    pub section: ProjectSection,
}

/// Scrollbar geometry and bounds recorded by the renderer for the open modal.
/// The rectangle is the full track, so clicks and drags map to offsets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollHit {
    pub rect: Rect,
    /// Largest valid scroll offset in lines.
    pub max: usize,
    /// Visible rows, used for page-wise keyboard scrolling.
    pub page: usize,
    /// Offset that was actually rendered (already clamped).
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct PackageOption {
    pub name: &'static str,
    pub selected: bool,
}

pub const PACKAGE_CHOICES: [&str; 10] = [
    "git",
    "nodejs",
    "python3",
    "build-essential",
    "ripgrep",
    "fd-find",
    "curl",
    "jq",
    "less",
    "golang",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateFocus {
    Name,
    Harness,
    Packages,
    Create,
}

#[derive(Debug, Clone)]
pub struct CreateProjectForm {
    pub name: String,
    pub harness: Harness,
    pub focus: CreateFocus,
    pub packages: Vec<PackageOption>,
    pub package_cursor: usize,
    pub error: Option<String>,
}

impl CreateProjectForm {
    pub fn new(default_harness: Harness) -> Self {
        Self {
            name: String::new(),
            harness: default_harness,
            focus: CreateFocus::Name,
            // The harness preset always provides its own runtime and base
            // tools; these are additive convenience packages.
            packages: PACKAGE_CHOICES
                .iter()
                .map(|name| PackageOption {
                    name,
                    selected: false,
                })
                .collect(),
            package_cursor: 0,
            error: None,
        }
    }

    pub fn selected_packages(&self) -> Vec<String> {
        self.packages
            .iter()
            .filter(|p| p.selected)
            .map(|p| p.name.to_string())
            .collect()
    }

    pub(super) fn cycle_focus(&mut self, backwards: bool) {
        let order = [
            CreateFocus::Name,
            CreateFocus::Harness,
            CreateFocus::Packages,
            CreateFocus::Create,
        ];
        let current = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if backwards {
            (current + order.len() - 1) % order.len()
        } else {
            (current + 1) % order.len()
        };
        self.focus = order[next];
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFocus {
    Path,
    Name,
}

#[derive(Debug, Clone)]
pub struct ImportProjectForm {
    pub path: String,
    pub name: String,
    pub focus: ImportFocus,
    pub error: Option<String>,
}

impl ImportProjectForm {
    pub fn new() -> Self {
        Self {
            path: String::new(),
            name: String::new(),
            focus: ImportFocus::Path,
            error: None,
        }
    }

    pub(super) fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            ImportFocus::Path => ImportFocus::Name,
            ImportFocus::Name => ImportFocus::Path,
        };
    }
}

impl Default for ImportProjectForm {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct SecurityPrompt {
    pub command: String,
    pub project: PathBuf,
    pub inspection: Inspection,
    pub error: Option<String>,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    DeleteSnapshot(String),
    RestoreSnapshot(Box<SnapshotMeta>),
    SetupEnvironment,
}

#[derive(Debug, Clone)]
pub struct ConfirmPrompt {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub action: ConfirmAction,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct DoctorModal {
    pub report: DoctorReport,
    pub downloading: bool,
    pub setting_up: bool,
    /// The failure that opened the modal, if any.
    pub error: Option<String>,
    pub scroll: usize,
}

impl DoctorModal {
    pub fn new(report: DoctorReport) -> Self {
        Self {
            report,
            downloading: false,
            setting_up: false,
            error: None,
            scroll: 0,
        }
    }

    pub fn with_error(report: DoctorReport, error: impl Into<String>) -> Self {
        Self {
            report,
            downloading: false,
            setting_up: false,
            error: Some(error.into()),
            scroll: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigStepHit {
    pub rect: Rect,
    pub field: ConfigField,
    pub step: i8,
}

/// Clickable tab in the settings modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigTabHit {
    pub rect: Rect,
    pub page: ConfigPage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteInstanceStage {
    Confirm,
    Files,
}

#[derive(Debug, Clone)]
pub struct DeleteInstanceModal {
    pub instance: InstanceSummary,
    pub stage: DeleteInstanceStage,
    pub error: Option<String>,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct DeleteProjectModal {
    pub project: Project,
    /// Git state shown before deletion so the user can decide about files.
    pub git: GitSummary,
    /// Instances that will be stopped and removed with the project.
    pub instances: Vec<String>,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct MultiAgentForm {
    pub source: String,
    pub count: usize,
    pub harness: Harness,
    pub start: bool,
    pub error: Option<String>,
    pub scroll: usize,
}

impl MultiAgentForm {
    pub fn new(source: String, harness: Harness) -> Self {
        Self {
            source,
            count: 2,
            harness,
            start: true,
            error: None,
            scroll: 0,
        }
    }
}

/// Editable per-project settings stored in the project's `hegn.toml`.
///
/// `None` means "inherit the global setting", so the form distinguishes an
/// explicit choice from the default instead of writing a snapshot of it.
#[derive(Debug, Clone)]
pub struct ProjectConfigForm {
    pub project: PathBuf,
    pub harness: Option<Harness>,
    pub launch: Option<String>,
    pub integration: Option<Integration>,
    pub auto_start: Option<bool>,
    pub row: usize,
    pub error: Option<String>,
}

impl ProjectConfigForm {
    pub const ROWS: usize = 4;

    pub fn new(project: PathBuf, meta: &ProjectMeta) -> Self {
        Self {
            project,
            harness: meta.harness,
            launch: meta.launch.clone(),
            integration: meta.integration,
            auto_start: meta.auto_start,
            row: 0,
            error: None,
        }
    }

    /// Move the row cursor with wrap-around.
    pub fn select(&mut self, delta: isize) {
        self.row = (self.row as isize + delta).rem_euclid(Self::ROWS as isize) as usize;
    }
}

/// Search state for the help modal. `/` starts editing the query, so typed
/// characters filter the keybinding list instead of triggering shortcuts.
#[derive(Debug, Clone, Default)]
pub struct HelpState {
    pub query: String,
    pub searching: bool,
    pub scroll: usize,
}

impl HelpState {
    /// Case-insensitive search across the section title, keybinding and
    /// description. Every whitespace-separated term must match somewhere.
    pub fn matches(&self, section: &str, key: &str, description: &str) -> bool {
        let query = self.query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let haystack = format!("{section} {key} {description}").to_lowercase();
        query.split_whitespace().all(|term| haystack.contains(term))
    }
}

#[derive(Debug, Clone)]
pub enum Modal {
    CreateProject(CreateProjectForm),
    ImportProject(ImportProjectForm),
    Doctor(DoctorModal),
    Security(SecurityPrompt),
    Confirm(ConfirmPrompt),
    Config(ConfigForm),
    DeleteInstance(DeleteInstanceModal),
    DeleteProject(DeleteProjectModal),
    MultiAgent(MultiAgentForm),
    ProjectConfig(ProjectConfigForm),
    Help(HelpState),
    Error {
        title: String,
        message: String,
        scroll: usize,
    },
}

/// Main TUI state.
pub struct App {
    pub config: Config,
    pub store: StateStore,
    pub projects: Vec<Project>,
    pub visible: Vec<usize>,
    pub selected: usize,
    pub query: String,
    pub searching: bool,
    pub focus: Focus,
    pub screen: Screen,
    /// Project whose dedicated screen is open, frozen while the user browses.
    pub open_project: Option<PathBuf>,
    pub project_view: ProjectView,
    /// True while the terminal pane has keyboard focus in [`ProjectView::Split`].
    pub terminal_focus: bool,
    /// Clickable section headers recorded by the renderer.
    pub section_hits: Vec<SectionHit>,
    /// Clickable terminal pane and collapsed strip for mouse focus.
    pub terminal_hit: Option<Rect>,
    pub terminal_strip_hit: Option<Rect>,
    pub modal: Option<Modal>,
    pub instance: InstanceStatus,
    /// Health line parsed from the selected instance's serial log.
    pub health: Option<InstanceHealth>,
    pub snapshots: Vec<SnapshotMeta>,
    pub selected_snapshot: usize,
    /// Indices into [`App::snapshots`] for the open project.
    pub project_snapshots: Vec<usize>,
    pub project_snapshot_selected: usize,
    pub snapshot_rows: Vec<RowHit>,
    pub instances: Vec<InstanceSummary>,
    /// Indices into [`App::instances`] for the selected project.
    pub instance_visible: Vec<usize>,
    pub instance_selected: usize,
    pub(super) instance_project: Option<PathBuf>,
    pub instance_rows: Vec<RowHit>,
    pub project_rows: Vec<RowHit>,
    /// Settings read from the open project's `hegn.toml`.
    pub project_meta: ProjectMeta,
    pub session: Option<Box<dyn TerminalConsole>>,
    pub terminal: TerminalBuffer,
    pub input: String,
    pub terminal_mode: TerminalMode,
    pub scroll: usize,
    pub toast: Option<Toast>,
    /// In-flight VM image download, rendered as a progress notification.
    pub download: Option<crate::vm::images::DownloadProgress>,
    /// First-boot harness provisioning, rendered above the download/toast.
    pub provision: Option<crate::agent::provision::ProvisionProgress>,
    /// Command waiting for the VM (and its preset) to become ready.
    pub pending_launch: Option<PendingLaunch>,
    /// Provisioning progress arrived before the console was attached.
    pub(super) provision_running: bool,
    /// The "preset ready" marker arrived before the console was attached.
    pub(super) provision_ready_seen: bool,
    pub footer_hits: Vec<FooterHit>,
    /// Settings rows and `‹ ›` steppers recorded by the renderer.
    pub config_rows: Vec<RowHit>,
    pub config_steppers: Vec<ConfigStepHit>,
    pub config_tabs: Vec<ConfigTabHit>,
    /// Settings row under the mouse pointer.
    pub config_hover: Option<usize>,
    /// Footer button under the mouse pointer, for the inverted hover style.
    pub hovered: Option<ActionId>,
    /// Scrollbar geometry of the open modal, recorded by the renderer.
    pub modal_scroll: Option<ScrollHit>,
    /// Pointer is over the modal scrollbar track.
    pub scroll_hover: bool,
    /// Left mouse button is dragging the modal scrollbar.
    pub scroll_dragging: bool,
    pub should_quit: bool,
    pub loading: bool,
    pub session_closed_reported: bool,
    pub viewport: (u16, u16),
    pub tick: u64,
    pub(super) security_engine: SecurityEngine,
    pub(super) last_click: Option<(Instant, PathBuf)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_search_matches_all_terms_case_insensitively() {
        let mut help = HelpState::default();
        assert!(help.matches("Projects", "c", "create project"));

        help.query = "CHECKPOINT".into();
        assert!(help.matches("Security", "c", "create git checkpoint, then allow"));
        assert!(!help.matches("Snapshots", "Enter", "restore selected snapshot"));

        help.query = "stop snapshot".into();
        assert!(
            !help.matches("Instances", "S", "stop selected instance"),
            "every query term must match"
        );

        help.query = "terminal".into();
        assert!(
            help.matches("Terminal", "F2", "toggle raw passthrough"),
            "the section title is searchable too"
        );
    }

    #[test]
    fn create_form_focus_cycles_and_only_selected_packages_are_returned() {
        let mut form = CreateProjectForm::new(Harness::Shell);
        assert_eq!(form.focus, CreateFocus::Name);
        form.cycle_focus(false);
        assert_eq!(form.focus, CreateFocus::Harness);
        form.cycle_focus(false);
        assert_eq!(form.focus, CreateFocus::Packages);
        form.cycle_focus(false);
        assert_eq!(form.focus, CreateFocus::Create);
        form.cycle_focus(false);
        assert_eq!(form.focus, CreateFocus::Name, "forward cycles wrap");
        form.cycle_focus(true);
        assert_eq!(form.focus, CreateFocus::Create, "backward cycles wrap");

        assert!(form.selected_packages().is_empty());
        form.packages[2].selected = true;
        form.packages[5].selected = true;
        assert_eq!(form.selected_packages(), vec!["python3", "fd-find"]);
    }

    #[test]
    fn import_and_project_config_forms_cycle_their_focus() {
        let mut import = ImportProjectForm::new();
        assert_eq!(import.focus, ImportFocus::Path);
        import.cycle_focus();
        assert_eq!(import.focus, ImportFocus::Name);
        import.cycle_focus();
        assert_eq!(import.focus, ImportFocus::Path);

        let mut config =
            ProjectConfigForm::new(PathBuf::from("/projects/demo"), &ProjectMeta::default());
        assert_eq!(config.row, 0);
        config.select(-1);
        assert_eq!(config.row, ProjectConfigForm::ROWS - 1);
        config.select(1);
        assert_eq!(config.row, 0);
        config.select(1);
        assert_eq!(config.row, 1);
    }

    #[test]
    fn health_parser_takes_the_last_guest_line() {
        let parsed = InstanceHealth::parse(
            "boot\nhegn-health v1 apt=2 checked=1700000000 reason=ok\n\
             hegn-health v1 apt=5 checked=1700003600 reason=ok\n",
        )
        .unwrap();
        assert_eq!(parsed.apt, Some(5));
        assert_eq!(parsed.reason, "ok");
        assert!(parsed.checked.is_some());
        assert!(InstanceHealth::parse("no health line").is_none());
    }
}
