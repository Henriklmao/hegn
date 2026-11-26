//! Shared data types and the [`App`] state container.
//!
//! These definitions are the contract between the state machine in
//! [`crate::tui::app`] and the pure renderer in [`crate::tui::ui`]. They carry
//! no behaviour beyond small pure helpers on the forms and health line.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::agent::launch::{Integration, LaunchSpec};
use crate::config::form::{ConfigField, ConfigPage};
pub use crate::config::form::{ConfigForm, ConfigKind};
use crate::config::{Config, Harness};
use crate::diagnostics::doctor::DoctorReport;
use crate::project::ForkMode;
use crate::project::{GitSummary, Project, ProjectEntry, ProjectMeta};
use crate::security::{Inspection, SecurityEngine};
use crate::session::console::TerminalConsole;
use crate::session::snapshot::SnapshotMeta;
use crate::session::state::StateStore;
use crate::session::terminal::TerminalBuffer;
use crate::vm::registry::VmSummary;

const TOAST_TTL: Duration = Duration::from_secs(6);

/// Top-level screens. `Vm` is the reusable VM view opened with `v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Project,
    Vm,
}

/// Sections shown on the project screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSection {
    Vm,
    Snapshots,
    Health,
    Git,
}

impl ProjectSection {
    pub const ALL: [Self; 4] = [Self::Vm, Self::Snapshots, Self::Health, Self::Git];

    pub fn number(self) -> usize {
        match self {
            Self::Vm => 1,
            Self::Snapshots => 2,
            Self::Health => 3,
            Self::Git => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Vm => "vm",
            Self::Snapshots => "snapshots",
            Self::Health => "health",
            Self::Git => "git",
        }
    }

    pub fn focus(self) -> Focus {
        match self {
            Self::Vm => Focus::Vm,
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
            Focus::Projects | Focus::Vm | Focus::Tools => Self::Vm,
        }
    }
}

/// Sections shown on the reusable VM screen opened with `v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmSection {
    Projects,
    Snapshots,
    Tools,
    Health,
}

impl VmSection {
    pub const ALL: [Self; 4] = [Self::Projects, Self::Snapshots, Self::Tools, Self::Health];

    pub fn number(self) -> usize {
        match self {
            Self::Projects => 1,
            Self::Snapshots => 2,
            Self::Tools => 3,
            Self::Health => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Projects => "projects",
            Self::Snapshots => "snapshots",
            Self::Tools => "tools",
            Self::Health => "health",
        }
    }

    pub fn focus(self) -> Focus {
        match self {
            Self::Projects => Focus::Projects,
            Self::Snapshots => Focus::Snapshots,
            Self::Tools => Focus::Tools,
            Self::Health => Focus::Health,
        }
    }

    pub fn from_focus(focus: Focus) -> Self {
        match focus {
            Focus::Projects => Self::Projects,
            Focus::Snapshots => Self::Snapshots,
            Focus::Tools => Self::Tools,
            Focus::Health => Self::Health,
            Focus::Vm | Focus::Git => Self::Projects,
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
    /// Project picker on the dashboard or member list on the VM screen.
    Projects,
    Vm,
    Snapshots,
    Health,
    Git,
    /// Tool profile, packages and resources section of the VM screen.
    Tools,
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
    /// Start a VM (when needed) and attach its console. The optional project
    /// selects the guest working directory for an agent launch.
    StartVm {
        vm: String,
        project: Option<PathBuf>,
    },
    /// Attach an existing VM, booting it first when it is stopped.
    AttachVm {
        vm: String,
    },
    /// Restart a running VM so a changed config or membership takes effect.
    ApplyVm {
        vm: String,
    },
    StopVm {
        vm: String,
    },
    DeleteVm {
        vm: String,
        delete_files: bool,
    },
    CreateVm {
        name: String,
        harness: Harness,
        vcpus: u8,
        memory_mib: u32,
        packages: Vec<String>,
    },
    AssignProject {
        project: String,
        path: PathBuf,
        vm: String,
        read_only: bool,
    },
    UnassignProject {
        project: String,
    },
    SetVmPrimary {
        vm: String,
        project: String,
    },
    ForkWorkspace {
        parent: String,
        name: String,
        mode: ForkMode,
    },
    /// Launch a configured command through a terminal integration.
    LaunchAgent {
        spec: LaunchSpec,
        vm: String,
        project: PathBuf,
    },
    /// Leave the TUI and attach the real serial console client.
    AttachConsole {
        vm: String,
    },
    /// Remove a project from the dashboard and optionally its files.
    DeleteProject {
        project: PathBuf,
        delete_files: bool,
    },
    RefreshVms,
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
    pub vm: String,
    pub project: PathBuf,
    pub since: Instant,
    pub provision_seen: bool,
    pub ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmUiState {
    Stopped,
    Starting,
    Running,
    Paused,
    Failed,
}

impl VmUiState {
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

/// Lifecycle state of the VM the TUI is attached to (or last attached to).
#[derive(Debug, Clone)]
pub struct VmStatus {
    pub state: VmUiState,
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
pub struct VmHealth {
    pub apt: Option<u32>,
    pub reason: String,
    pub checked: Option<chrono::DateTime<chrono::Utc>>,
}

impl VmHealth {
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

impl VmStatus {
    pub fn from_config(config: &Config) -> Self {
        let backend = "cloud-hypervisor";
        let virtiofs = if config.vm_assets_ready() {
            "configured"
        } else {
            "missing VM"
        };
        Self {
            state: VmUiState::Stopped,
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
    SubmitVm,
    SubmitAssign,
    SubmitFork,
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
    FocusVm,
    StartVm,
    StopVm,
    ApplyVm,
    Shell,
    AssignProject,
    DetachProject,
    ForkWorkspace,
    SetPrimary,
    NewVm,
    DeleteVm,
    DeleteProject,
    DeleteProjectFiles,
    VmConfig,
    GlobalConfig,
    ProjectConfig,
    ProjectConfigSave,
    ProjectConfigLaunch,
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
    ToggleForkMode,
    ToggleReadOnly,
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

/// Clickable VM section header or tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmSectionHit {
    pub rect: Rect,
    pub section: VmSection,
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
    /// Restart a VM so a changed config or membership takes effect.
    ApplyVm(String),
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
pub enum CreateVmFocus {
    Name,
    Harness,
    Vcpus,
    Memory,
    Packages,
    Create,
}

/// Form for `hegn vm create` (the `v`/`n` path on the VM screen).
#[derive(Debug, Clone)]
pub struct CreateVmForm {
    pub name: String,
    pub harness: Harness,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub focus: CreateVmFocus,
    pub packages: Vec<PackageOption>,
    pub package_cursor: usize,
    pub error: Option<String>,
}

impl CreateVmForm {
    pub fn new(default_harness: Harness, vcpus: u8, memory_mib: u32) -> Self {
        Self {
            name: String::new(),
            harness: default_harness,
            vcpus,
            memory_mib,
            focus: CreateVmFocus::Name,
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
            .filter(|package| package.selected)
            .map(|package| package.name.to_string())
            .collect()
    }

    pub(super) fn cycle_focus(&mut self, backwards: bool) {
        let order = [
            CreateVmFocus::Name,
            CreateVmFocus::Harness,
            CreateVmFocus::Vcpus,
            CreateVmFocus::Memory,
            CreateVmFocus::Packages,
            CreateVmFocus::Create,
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

/// Choose an existing VM for a project, or an unassigned project for a VM.
#[derive(Debug, Clone)]
pub struct AssignProjectForm {
    /// Project being assigned; empty when the user still picks one.
    pub project: Option<String>,
    /// Fixed target VM; empty when the user still picks one.
    pub vm: Option<String>,
    pub candidates: Vec<String>,
    pub cursor: usize,
    pub read_only: bool,
    pub error: Option<String>,
}

impl AssignProjectForm {
    /// Pick the VM that receives `project`.
    pub fn for_project(project: impl Into<String>, vms: Vec<String>) -> Self {
        Self {
            project: Some(project.into()),
            vm: None,
            candidates: vms,
            cursor: 0,
            read_only: false,
            error: None,
        }
    }

    /// Pick the project that joins `vm`.
    pub fn for_vm(vm: impl Into<String>, projects: Vec<String>) -> Self {
        Self {
            project: None,
            vm: Some(vm.into()),
            candidates: projects,
            cursor: 0,
            read_only: false,
            error: None,
        }
    }

    pub fn selected(&self) -> Option<&str> {
        self.candidates.get(self.cursor).map(String::as_str)
    }

    pub(super) fn cycle(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len() as isize;
        self.cursor = (self.cursor as isize + delta).rem_euclid(len) as usize;
    }
}

/// Fork a project into an isolated clone or a nested git worktree.
#[derive(Debug, Clone)]
pub struct ForkWorkspaceForm {
    pub parent: String,
    pub vm: String,
    pub name: String,
    pub mode: ForkMode,
    pub error: Option<String>,
}

impl ForkWorkspaceForm {
    pub fn new(parent: impl Into<String>, vm: impl Into<String>) -> Self {
        let parent = parent.into();
        let vm = vm.into();
        let name = format!("{parent}-fork");
        Self {
            parent,
            vm,
            name,
            mode: ForkMode::Clone,
            error: None,
        }
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            ForkMode::Clone => ForkMode::Worktree,
            ForkMode::Worktree => ForkMode::Clone,
        };
    }

    pub fn mode_label(&self) -> &'static str {
        match self.mode {
            ForkMode::Clone => "clone",
            ForkMode::Worktree => "worktree",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteVmStage {
    Confirm,
    Files,
}

#[derive(Debug, Clone)]
pub struct DeleteVmModal {
    pub vm: VmSummary,
    /// Exported members that will be unassigned, never deleted.
    pub members: usize,
    pub stage: DeleteVmStage,
    pub error: Option<String>,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct DetachProjectModal {
    pub project: String,
    pub vm: String,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct DeleteProjectModal {
    pub project: Project,
    /// Git state shown before deletion so the user can decide about files.
    pub git: GitSummary,
    /// VMs referenced by the project (unassigned, never deleted).
    pub vms: Vec<String>,
    pub scroll: usize,
}

/// Editable per-project presentation settings stored in `hegn.toml`.
///
/// V2 keeps only presentation keys here; the VM tool profile, packages and
/// harness live in the VM config, and membership lives in the registry.
#[derive(Debug, Clone)]
pub struct ProjectConfigForm {
    pub project: PathBuf,
    pub launch: Option<String>,
    pub integration: Option<Integration>,
    pub auto_start: Option<bool>,
    pub row: usize,
    pub error: Option<String>,
}

impl ProjectConfigForm {
    pub const ROWS: usize = 3;

    pub fn new(project: PathBuf, meta: &ProjectMeta) -> Self {
        Self {
            project,
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
    CreateVm(CreateVmForm),
    AssignProject(AssignProjectForm),
    ForkWorkspace(ForkWorkspaceForm),
    DeleteVm(DeleteVmModal),
    DetachProject(DetachProjectModal),
    DeleteProject(DeleteProjectModal),
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
    /// VM whose reusable screen is open.
    pub vm_open: Option<String>,
    pub project_view: ProjectView,
    /// True while the terminal pane has keyboard focus in [`ProjectView::Split`].
    pub terminal_focus: bool,
    /// Clickable section headers recorded by the renderer.
    pub section_hits: Vec<SectionHit>,
    /// Clickable VM section headers.
    pub vm_section_hits: Vec<VmSectionHit>,
    /// Clickable terminal pane and collapsed strip for mouse focus.
    pub terminal_hit: Option<Rect>,
    pub terminal_strip_hit: Option<Rect>,
    pub modal: Option<Modal>,
    /// Lifecycle of the VM the embedded console is attached to.
    pub vm: VmStatus,
    /// VM name the embedded console is attached to, when any.
    pub attached_vm: Option<String>,
    /// Health line parsed from the selected VM's serial log.
    pub health: Option<VmHealth>,
    pub snapshots: Vec<SnapshotMeta>,
    pub selected_snapshot: usize,
    /// Indices into [`App::snapshots`] for the open project's VM.
    pub project_snapshots: Vec<usize>,
    pub project_snapshot_selected: usize,
    /// Indices into [`App::snapshots`] for the open VM screen's VM.
    pub vm_snapshots: Vec<usize>,
    pub vm_snapshot_selected: usize,
    pub snapshot_rows: Vec<RowHit>,
    /// Private project registry entries, refreshed with the project scan.
    pub project_entries: Vec<ProjectEntry>,
    pub vms: Vec<VmSummary>,
    /// Indices into [`App::vms`] for the selected project.
    pub vm_visible: Vec<usize>,
    pub vm_selected: usize,
    pub(super) vm_project: Option<PathBuf>,
    pub vm_rows: Vec<RowHit>,
    /// Projects assigned to [`App::vm_open`].
    pub vm_members: Vec<ProjectEntry>,
    pub vm_member_selected: usize,
    pub vm_member_rows: Vec<RowHit>,
    pub project_rows: Vec<RowHit>,
    /// Settings read from the open project's `hegn.toml`.
    pub project_meta: ProjectMeta,
    pub session: Option<Box<dyn TerminalConsole>>,
    pub terminal: TerminalBuffer,
    pub input: String,
    pub terminal_mode: TerminalMode,
    pub scroll: usize,
    pub toast: Option<Toast>,
    /// VMs whose pending config/membership change has not been applied yet.
    ///
    /// This is a TUI-session hint only and is never persisted.
    pub pending_apply: BTreeSet<String>,
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
            !help.matches("Vm", "S", "stop selected VM"),
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
    fn create_vm_form_cycles_and_package_selection() {
        let mut form = CreateVmForm::new(Harness::Shell, 2, 2048);
        assert_eq!(form.focus, CreateVmFocus::Name);
        for expected in [
            CreateVmFocus::Harness,
            CreateVmFocus::Vcpus,
            CreateVmFocus::Memory,
            CreateVmFocus::Packages,
            CreateVmFocus::Create,
            CreateVmFocus::Name,
        ] {
            form.cycle_focus(false);
            assert_eq!(form.focus, expected);
        }
        form.packages[0].selected = true;
        assert_eq!(form.selected_packages(), vec!["git"]);
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
    fn assign_and_fork_forms_cycle_choices() {
        let mut assign = AssignProjectForm::for_project("demo", vec!["one".into(), "two".into()]);
        assert_eq!(assign.project.as_deref(), Some("demo"));
        assert_eq!(assign.selected(), Some("one"));
        assign.cycle(-1);
        assert_eq!(assign.selected(), Some("two"));
        assign.cycle(1);
        assert_eq!(assign.selected(), Some("one"));
        let assign = AssignProjectForm::for_vm("rust", vec!["demo".into()]);
        assert_eq!(assign.vm.as_deref(), Some("rust"));
        assert_eq!(assign.project, None);

        let mut fork = ForkWorkspaceForm::new("demo", "rust");
        assert_eq!(fork.mode, ForkMode::Clone);
        assert_eq!(fork.name, "demo-fork");
        fork.toggle_mode();
        assert_eq!(fork.mode_label(), "worktree");
        fork.toggle_mode();
        assert_eq!(fork.mode_label(), "clone");
    }

    #[test]
    fn health_parser_takes_the_last_guest_line() {
        let parsed = VmHealth::parse(
            "boot\nhegn-health v1 apt=2 checked=1700000000 reason=ok\n\
             hegn-health v1 apt=5 checked=1700003600 reason=ok\n",
        )
        .unwrap();
        assert_eq!(parsed.apt, Some(5));
        assert_eq!(parsed.reason, "ok");
        assert!(parsed.checked.is_some());
        assert!(VmHealth::parse("no health line").is_none());
    }

    #[test]
    fn section_lookup_maps_focus_round_trip() {
        for section in ProjectSection::ALL {
            assert_eq!(ProjectSection::from_focus(section.focus()), section);
        }
        for section in VmSection::ALL {
            assert_eq!(VmSection::from_focus(section.focus()), section);
        }
    }
}
