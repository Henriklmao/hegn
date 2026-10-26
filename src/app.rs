//! TUI application state and event handling.
//!
//! All state transitions live here and are covered by unit tests; [`crate::ui`]
//! is a pure renderer and [`crate::tui`] owns the terminal loop. This separation
//! makes the dashboard testable with ratatui's `TestBackend`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::config::{Config, Harness, SecurityMode};
use crate::config_form::{Applied, ConfigField, ConfigPage, FormOutcome};
pub use crate::config_form::{ConfigForm, ConfigKind};
use crate::console::TerminalConsole;
use crate::doctor::DoctorReport;
use crate::project::{self, GitSummary, Project, ProjectMeta};
use crate::sandbox::SandboxSummary;
use crate::security::{Decision, Inspection, SecurityEngine};
use crate::snapshot::{SnapshotMeta, SnapshotStore};
use crate::state::StateStore;
use crate::terminal::TerminalBuffer;

const TOAST_TTL: Duration = Duration::from_secs(6);
const DOUBLE_CLICK: Duration = Duration::from_millis(450);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Terminal,
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
    Sandboxes,
    Snapshots,
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
    /// Ensure a sandbox for a project and attach its console.
    StartProjectSandbox {
        project: PathBuf,
        harness: Harness,
    },
    /// Start (if needed) and attach an existing sandbox.
    AttachSandbox {
        sandbox: String,
    },
    StopSandbox {
        sandbox: String,
    },
    DeleteSandbox {
        sandbox: String,
        delete_files: bool,
    },
    CloneSandbox {
        source: String,
        count: usize,
        harness: Harness,
        start: bool,
    },
    RefreshSandboxes,
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
    fn new(text: impl Into<String>, level: ToastLevel) -> Self {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxState {
    Stopped,
    Starting,
    Running,
    Paused,
    Failed,
}

impl SandboxState {
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
pub struct SandboxStatus {
    pub state: SandboxState,
    pub backend: &'static str,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub virtiofs: &'static str,
    pub socket: Option<PathBuf>,
    pub note: Option<String>,
    pub started_at: Option<Instant>,
}

impl SandboxStatus {
    pub fn from_config(config: &Config) -> Self {
        let backend = "cloud-hypervisor";
        let virtiofs = if config.vm_assets_ready() {
            "configured"
        } else {
            "missing VM"
        };
        Self {
            state: SandboxState::Stopped,
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
    FocusSandboxes,
    StartSandbox,
    StopSandbox,
    CloneSandbox,
    DeleteSandbox,
    SandboxConfig,
    GlobalConfig,
    MultiAgent,
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

    fn cycle_focus(&mut self, backwards: bool) {
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

    fn cycle_focus(&mut self) {
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
pub enum DeleteSandboxStage {
    Confirm,
    Files,
}

#[derive(Debug, Clone)]
pub struct DeleteSandboxModal {
    pub sandbox: SandboxSummary,
    pub stage: DeleteSandboxStage,
    pub error: Option<String>,
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
    DeleteSandbox(DeleteSandboxModal),
    MultiAgent(MultiAgentForm),
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
    pub modal: Option<Modal>,
    pub sandbox: SandboxStatus,
    pub snapshots: Vec<SnapshotMeta>,
    pub selected_snapshot: usize,
    pub snapshot_rows: Vec<RowHit>,
    pub sandboxes: Vec<SandboxSummary>,
    /// Indices into [`App::sandboxes`] for the selected project.
    pub sandbox_visible: Vec<usize>,
    pub sandbox_selected: usize,
    sandbox_project: Option<PathBuf>,
    pub sandbox_rows: Vec<RowHit>,
    pub project_rows: Vec<RowHit>,
    pub session: Option<Box<dyn TerminalConsole>>,
    pub terminal: TerminalBuffer,
    pub input: String,
    pub terminal_mode: TerminalMode,
    pub scroll: usize,
    pub toast: Option<Toast>,
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
    security_engine: SecurityEngine,
    last_click: Option<(Instant, PathBuf)>,
}

impl App {
    pub fn new(config: Config) -> Self {
        let store = StateStore::load(config.state_dir.join("state.json"));
        let snapshots = SnapshotStore::new(&config.snapshots_dir)
            .list()
            .unwrap_or_default();
        let sandboxes = crate::sandbox::SandboxRegistry::new(config.clone())
            .summaries()
            .unwrap_or_default();
        let sandbox = SandboxStatus::from_config(&config);
        let security_engine = SecurityEngine::new(config.security);
        Self {
            config,
            store,
            projects: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            query: String::new(),
            searching: false,
            focus: Focus::Projects,
            screen: Screen::Dashboard,
            modal: None,
            sandbox,
            snapshots,
            selected_snapshot: 0,
            snapshot_rows: Vec::new(),
            sandboxes,
            sandbox_visible: Vec::new(),
            sandbox_selected: 0,
            sandbox_project: None,
            sandbox_rows: Vec::new(),
            project_rows: Vec::new(),
            session: None,
            terminal: TerminalBuffer::default(),
            input: String::new(),
            terminal_mode: TerminalMode::Normal,
            scroll: 0,
            toast: None,
            footer_hits: Vec::new(),
            config_rows: Vec::new(),
            config_steppers: Vec::new(),
            config_tabs: Vec::new(),
            config_hover: None,
            hovered: None,
            modal_scroll: None,
            scroll_hover: false,
            scroll_dragging: false,
            should_quit: false,
            loading: true,
            session_closed_reported: false,
            viewport: (120, 40),
            tick: 0,
            security_engine,
            last_click: None,
        }
    }

    /// Install a fresh project list (usually from the background scanner).
    pub fn set_projects(&mut self, mut projects: Vec<Project>) {
        let opened = self.store.state().opened_at();
        for project in &mut projects {
            project.last_opened = opened.get(&project.path).copied();
        }
        let previous = self.selected_project().map(|p| p.path.clone());
        self.projects = projects;
        self.loading = false;
        self.refresh_filter();
        let preferred = previous.or_else(|| self.store.state().last_project.clone());
        if let Some(path) = preferred
            && let Some(index) = self
                .visible
                .iter()
                .position(|&i| self.projects[i].path == path)
        {
            self.selected = index;
        }
        self.refresh_sandbox_filter();
    }

    pub fn add_project(&mut self, project: Project) {
        let path = project.path.clone();
        self.projects.retain(|p| p.path != project.path);
        self.projects.push(project);
        self.projects
            .sort_by_key(|project| project.name.to_lowercase());
        self.loading = false;
        self.query.clear();
        self.refresh_filter();
        if let Some(index) = self
            .visible
            .iter()
            .position(|&i| self.projects[i].path == path)
        {
            self.selected = index;
        }
        self.refresh_sandbox_filter();
    }

    pub fn refresh_snapshots(&mut self) {
        self.snapshots = SnapshotStore::new(&self.config.snapshots_dir)
            .list()
            .unwrap_or_default();
        if self.selected_snapshot >= self.snapshots.len() {
            self.selected_snapshot = self.snapshots.len().saturating_sub(1);
        }
    }

    pub fn refresh_filter(&mut self) {
        self.visible = project::filter_projects(&self.projects, &self.query);
        if self.visible.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.visible.len() {
            self.selected = self.visible.len() - 1;
        }
        self.refresh_sandbox_filter();
    }

    pub fn selected_project(&self) -> Option<&Project> {
        self.visible
            .get(self.selected)
            .and_then(|&i| self.projects.get(i))
    }

    pub fn selected_snapshot(&self) -> Option<&SnapshotMeta> {
        self.snapshots.get(self.selected_snapshot)
    }

    pub fn selected_sandbox(&self) -> Option<&SandboxSummary> {
        self.sandbox_visible
            .get(self.sandbox_selected)
            .and_then(|&index| self.sandboxes.get(index))
    }

    /// Rebuild the sandbox list so it only contains sandboxes for the project
    /// selected in the project list. Multi-agent clones share their source
    /// project, so they stay visible and are one `Enter` away.
    ///
    /// The previous selection is preserved while the project stays the same;
    /// switching projects selects a running sandbox when one exists.
    pub fn refresh_sandbox_filter(&mut self) {
        let selected = self.selected_sandbox().map(|sandbox| sandbox.name.clone());
        self.refresh_sandbox_filter_named(selected);
    }

    /// Rebuild the filter while preserving `selected` by name. The name must be
    /// captured before [`App::sandboxes`] is replaced: the visible indices point
    /// into the old vector.
    fn refresh_sandbox_filter_named(&mut self, selected: Option<String>) {
        let project = self.selected_project().map(|project| project.path.clone());
        let project_changed = self.sandbox_project != project;
        self.sandbox_project = project.clone();

        self.sandbox_visible = match project {
            Some(project) => self
                .sandboxes
                .iter()
                .enumerate()
                .filter(|(_, sandbox)| crate::sandbox::same_project(&sandbox.project, &project))
                .map(|(index, _)| index)
                .collect(),
            None => Vec::new(),
        };

        if !project_changed
            && let Some(name) = selected
            && let Some(index) = self
                .sandbox_visible
                .iter()
                .position(|&index| self.sandboxes[index].name == name)
        {
            self.sandbox_selected = index;
            return;
        }
        if project_changed {
            self.sandbox_selected = self
                .sandbox_visible
                .iter()
                .position(|&index| self.sandboxes[index].state == "running")
                .unwrap_or(0);
        } else {
            self.sandbox_selected = self
                .sandbox_selected
                .min(self.sandbox_visible.len().saturating_sub(1));
        }
    }

    /// Refresh the sandbox list from disk/process state. Cheap enough for the
    /// UI tick; it only reads files and `/proc`.
    pub fn refresh_sandboxes(&mut self) {
        let selected = self.selected_sandbox().map(|sandbox| sandbox.name.clone());
        let registry = crate::sandbox::SandboxRegistry::new(self.config.clone());
        if let Ok(sandboxes) = registry.summaries() {
            self.sandboxes = sandboxes;
        }
        self.refresh_sandbox_filter_named(selected);
        let selected = self.selected_sandbox().cloned();
        if let Some(summary) = selected {
            self.sandbox.vcpus = summary.vcpus;
            self.sandbox.memory_mib = summary.memory_mib;
            // Do not overwrite the optimistic Starting state while the attach
            // thread is still booting; it would re-enable double starts.
            if self.sandbox.state != SandboxState::Starting {
                self.sandbox.state = match summary.state.as_str() {
                    "running" => SandboxState::Running,
                    "paused" => SandboxState::Paused,
                    _ => SandboxState::Stopped,
                };
            }
        }
    }

    pub fn focus_next(&mut self) {
        self.focus = match self.focus {
            Focus::Projects => Focus::Sandboxes,
            Focus::Sandboxes => Focus::Snapshots,
            Focus::Snapshots => Focus::Projects,
        };
    }

    pub fn select_next(&mut self, delta: isize) {
        match self.focus {
            Focus::Projects => {
                if self.visible.is_empty() {
                    return;
                }
                let max = self.visible.len() - 1;
                self.selected = clamp_index(self.selected, delta, max);
                self.refresh_sandbox_filter();
            }
            Focus::Sandboxes => {
                if self.sandbox_visible.is_empty() {
                    return;
                }
                let max = self.sandbox_visible.len() - 1;
                self.sandbox_selected = clamp_index(self.sandbox_selected, delta, max);
            }
            Focus::Snapshots => {
                if self.snapshots.is_empty() {
                    return;
                }
                let max = self.snapshots.len() - 1;
                self.selected_snapshot = clamp_index(self.selected_snapshot, delta, max);
            }
        }
    }

    pub fn toast(&mut self, text: impl Into<String>, level: ToastLevel) {
        self.toast = Some(Toast::new(text, level));
    }

    pub fn set_viewport(&mut self, cols: u16, rows: u16) {
        if self.viewport == (cols, rows) {
            return;
        }
        self.viewport = (cols, rows);
    }

    pub fn poll(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if self.tick.is_multiple_of(40) {
            self.refresh_sandboxes();
        }
        let mut closed = false;
        if let Some(session) = &mut self.session {
            for bytes in session.drain() {
                self.terminal.push_bytes(&bytes);
            }
            if session.is_exited() {
                closed = true;
            }
        }
        if closed && !self.session_closed_reported {
            self.session_closed_reported = true;
            self.sandbox.state = SandboxState::Stopped;
            self.sandbox.started_at = None;
            self.toast("session exited", ToastLevel::Info);
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.expired(Instant::now()))
        {
            self.toast = None;
        }
    }

    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Handle a key press and return side effects for the terminal loop.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        self.hovered = None;
        self.config_hover = None;
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        self.scroll_hover = false;
        self.scroll_dragging = false;
        match self.screen {
            Screen::Dashboard => self.handle_dashboard_key(key),
            Screen::Terminal => self.handle_terminal_key(key),
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return vec![Effect::Quit];
        }

        if self.searching {
            match key.code {
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.query.clear();
                    self.refresh_filter();
                }
                KeyCode::Char(c) => {
                    self.query.push(c);
                    self.refresh_filter();
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.refresh_filter();
                }
                KeyCode::Enter => {
                    self.searching = false;
                    return self.open_selected();
                }
                KeyCode::Esc => {
                    self.searching = false;
                }
                KeyCode::Up => self.select_next(-1),
                KeyCode::Down => self.select_next(1),
                KeyCode::Tab => self.focus_next(),
                _ => {}
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                vec![Effect::Quit]
            }
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.refresh_filter();
                }
                Vec::new()
            }
            KeyCode::Char('/') => {
                self.searching = true;
                Vec::new()
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.modal = Some(Modal::Help(HelpState::default()));
                Vec::new()
            }
            KeyCode::F(3) => {
                self.modal = Some(Modal::Doctor(DoctorModal::new(crate::doctor::run(
                    &self.config,
                ))));
                Vec::new()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next(1);
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_next(-1);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.select_next(5);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.select_next(-5);
                Vec::new()
            }
            KeyCode::Tab => {
                self.focus_next();
                Vec::new()
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                if self.focus == Focus::Snapshots {
                    self.confirm_restore_snapshot()
                } else {
                    self.open_selected()
                }
            }
            KeyCode::Char('1') => {
                self.focus = Focus::Projects;
                Vec::new()
            }
            KeyCode::Char('2') => {
                self.focus = Focus::Sandboxes;
                self.refresh_sandboxes();
                Vec::new()
            }
            KeyCode::Char('3') => {
                self.focus = Focus::Snapshots;
                Vec::new()
            }
            KeyCode::Char('c') if self.focus == Focus::Projects => {
                self.modal = Some(Modal::CreateProject(CreateProjectForm::new(
                    self.config.harness,
                )));
                Vec::new()
            }
            KeyCode::Char('h') | KeyCode::Left if self.focus == Focus::Projects => {
                self.modal = Some(Modal::ImportProject(ImportProjectForm::new()));
                Vec::new()
            }
            KeyCode::Char('e') => self.effect_for_selected(Effect::OpenEditor),
            KeyCode::Char('g') => self.effect_for_selected(Effect::OpenLazygit),
            KeyCode::Char('r') => vec![Effect::RefreshProjects, Effect::RefreshSandboxes],
            KeyCode::Char('d') => match self.focus {
                Focus::Snapshots => self.confirm_delete_snapshot(),
                Focus::Sandboxes => self.confirm_delete_sandbox(),
                Focus::Projects => self.confirm_delete_project_sandbox(),
            },
            KeyCode::Char('n') if self.focus == Focus::Snapshots => {
                self.handle_action(ActionId::NewSnapshot)
            }
            KeyCode::Char('s') => {
                self.focus = Focus::Snapshots;
                Vec::new()
            }
            KeyCode::Char('p') => {
                self.focus = Focus::Projects;
                Vec::new()
            }
            KeyCode::Char('b') => {
                self.focus = Focus::Sandboxes;
                self.refresh_sandboxes();
                Vec::new()
            }
            KeyCode::Char('S') if self.focus == Focus::Sandboxes => self.stop_selected_sandbox(),
            KeyCode::F(4) => self.open_global_config(),
            KeyCode::F(5) if self.focus == Focus::Sandboxes => self.open_sandbox_config(),
            KeyCode::Char('t') => {
                if self.session.is_some() {
                    self.screen = Screen::Terminal;
                } else {
                    self.toast("no active session", ToastLevel::Warn);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn effect_for_selected(&self, effect: impl FnOnce(PathBuf) -> Effect) -> Vec<Effect> {
        match self.selected_project() {
            Some(project) => vec![effect(project.path.clone())],
            None => Vec::new(),
        }
    }

    fn handle_terminal_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.code == KeyCode::F(2) {
            self.toggle_raw_mode();
            return Vec::new();
        }
        if crate::attach_bar::detach_requested(key) {
            self.screen = Screen::Dashboard;
            return Vec::new();
        }
        // Left leaves the project terminal in normal mode. Raw mode still
        // forwards it to the guest; insert mode is for typing, where Esc
        // returns to normal mode.
        if key.code == KeyCode::Left && self.terminal_mode == TerminalMode::Normal {
            self.screen = Screen::Dashboard;
            return Vec::new();
        }

        if key.code == KeyCode::Esc {
            match self.terminal_mode {
                TerminalMode::Insert => {
                    self.terminal_mode = TerminalMode::Normal;
                    self.toast("normal mode — press i or a to type", ToastLevel::Info);
                }
                // Esc in normal/raw mode leaves the project terminal.
                _ => self.screen = Screen::Dashboard,
            }
            return Vec::new();
        }

        match self.terminal_mode {
            TerminalMode::Raw => {
                if let Some(bytes) = encode_key(key) {
                    self.write_bytes(&bytes);
                }
            }
            TerminalMode::Normal => match key.code {
                KeyCode::Char('i') | KeyCode::Char('a') => {
                    self.terminal_mode = TerminalMode::Insert;
                    self.toast("insert mode — Esc returns to normal", ToastLevel::Info);
                }
                KeyCode::Char('M') => return self.open_multi_agent(),
                KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.terminal.clear();
                    self.scroll = 0;
                }
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_add(5).min(self.terminal.len());
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_sub(5);
                }
                KeyCode::Home => {
                    self.scroll = self.terminal.len().saturating_sub(1);
                }
                KeyCode::End => {
                    self.scroll = 0;
                }
                _ => {}
            },
            TerminalMode::Insert => match key.code {
                KeyCode::Enter => {
                    return self.submit_input();
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.clear();
                    self.write_bytes(b"\x03");
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.write_bytes(b"\x04");
                }
                KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.terminal.clear();
                    self.scroll = 0;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.clear();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    if self.input.len() < 8192 {
                        self.input.push(c);
                    }
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Tab => self.write_bytes(b"\t"),
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_add(5).min(self.terminal.len());
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_sub(5);
                }
                KeyCode::Home => {
                    self.scroll = self.terminal.len().saturating_sub(1);
                }
                KeyCode::End => {
                    self.scroll = 0;
                }
                _ => {}
            },
        }
        Vec::new()
    }

    fn toggle_raw_mode(&mut self) {
        self.terminal_mode = match self.terminal_mode {
            TerminalMode::Raw => TerminalMode::Normal,
            _ => TerminalMode::Raw,
        };
        self.toast(
            match self.terminal_mode {
                TerminalMode::Raw => "raw passthrough — every key goes to the guest",
                _ => "normal mode — press i or a to type",
            },
            if self.terminal_mode == TerminalMode::Raw {
                ToastLevel::Warn
            } else {
                ToastLevel::Info
            },
        );
    }

    /// Inspect and submit the terminal input buffer.
    pub fn submit_input(&mut self) -> Vec<Effect> {
        let command = self.input.clone();
        if command.trim().is_empty() {
            self.input.clear();
            return Vec::new();
        }
        self.input.clear();

        if self.terminal_mode == TerminalMode::Raw {
            self.write_line(&command);
            return Vec::new();
        }

        let cwd = self
            .session
            .as_ref()
            .map(|s| s.project().to_path_buf())
            .or_else(|| self.selected_project().map(|p| p.path.clone()));
        let inspection = self.security_engine.inspect(&command, cwd.as_deref());
        match inspection.decision {
            Decision::Allow => {
                self.write_line(&command);
                if inspection.destructive {
                    self.toast("workspace guard: allowed", ToastLevel::Warn);
                }
            }
            Decision::Warn | Decision::Block => {
                self.modal = Some(Modal::Security(SecurityPrompt {
                    command,
                    project: cwd.unwrap_or_else(|| PathBuf::from(".")),
                    inspection,
                    error: None,
                    scroll: 0,
                }));
            }
        }
        Vec::new()
    }

    fn write_line(&mut self, line: &str) -> bool {
        let mut error = None;
        let mut active = false;
        if let Some(session) = self.session.as_mut()
            && !session.is_exited()
        {
            active = true;
            if let Err(err) = session.send(format!("{line}\r").as_bytes()) {
                error = Some(err.to_string());
            }
        }
        if let Some(error) = error {
            self.toast(format!("write failed: {error}"), ToastLevel::Error);
            false
        } else if active {
            self.scroll = 0;
            true
        } else {
            self.toast("no active session", ToastLevel::Warn);
            false
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        let mut error = None;
        let mut active = false;
        if let Some(session) = self.session.as_mut()
            && !session.is_exited()
        {
            active = true;
            if let Err(err) = session.send(bytes) {
                error = Some(err.to_string());
            }
        }
        if let Some(error) = error {
            self.toast(format!("write failed: {error}"), ToastLevel::Error);
        } else if active {
            self.scroll = 0;
        } else {
            self.toast("no active session", ToastLevel::Warn);
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(modal) = self.modal.take() else {
            return Vec::new();
        };
        match modal {
            Modal::Help(mut help) => {
                if help.searching {
                    let scroll = match key.code {
                        KeyCode::Up => Some(-1),
                        KeyCode::Down => Some(1),
                        _ => None,
                    };
                    let page = match key.code {
                        KeyCode::PageUp => Some(-1),
                        KeyCode::PageDown => Some(1),
                        _ => None,
                    };
                    match key.code {
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            help.query.clear();
                            help.scroll = 0;
                        }
                        KeyCode::Char(c) => {
                            help.query.push(c);
                            help.scroll = 0;
                        }
                        KeyCode::Backspace => {
                            help.query.pop();
                            help.scroll = 0;
                        }
                        KeyCode::Esc | KeyCode::Enter => help.searching = false,
                        _ => {}
                    }
                    self.modal = Some(Modal::Help(help));
                    if let Some(delta) = scroll {
                        self.scroll_modal(delta);
                    }
                    if let Some(pages) = page {
                        self.scroll_modal_page(pages);
                    }
                    return Vec::new();
                }
                match key.code {
                    KeyCode::Char('/') => {
                        help.searching = true;
                        self.modal = Some(Modal::Help(help));
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.modal = Some(Modal::Help(help));
                        self.scroll_modal(1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.modal = Some(Modal::Help(help));
                        self.scroll_modal(-1);
                    }
                    KeyCode::PageDown => {
                        self.modal = Some(Modal::Help(help));
                        self.scroll_modal_page(1);
                    }
                    KeyCode::PageUp => {
                        self.modal = Some(Modal::Help(help));
                        self.scroll_modal_page(-1);
                    }
                    KeyCode::Esc if !help.query.is_empty() => {
                        help.query.clear();
                        help.scroll = 0;
                        self.modal = Some(Modal::Help(help));
                    }
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
                        // closed
                    }
                    _ => self.modal = Some(Modal::Help(help)),
                }
                Vec::new()
            }
            Modal::Error { .. } => Vec::new(),
            Modal::Confirm(prompt) => self.handle_confirm_key(key, prompt),
            Modal::Config(mut form) => match form.handle_key(key) {
                FormOutcome::Stay => {
                    self.modal = Some(Modal::Config(form));
                    Vec::new()
                }
                FormOutcome::Close => Vec::new(),
                FormOutcome::Save => self.save_config_form(form),
                FormOutcome::Revert => self.revert_config_form(form),
                FormOutcome::OpenFile => vec![Effect::OpenEditor(form.path)],
                FormOutcome::OpenProfile(harness) => vec![Effect::OpenProfileEditor(harness)],
            },
            Modal::DeleteSandbox(mut modal) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Vec::new(),
                KeyCode::Char('y') => match modal.stage {
                    DeleteSandboxStage::Confirm => {
                        if self.workspace_is_inside_sandbox(&modal.sandbox) {
                            modal.stage = DeleteSandboxStage::Files;
                            self.modal = Some(Modal::DeleteSandbox(modal));
                            Vec::new()
                        } else {
                            vec![Effect::DeleteSandbox {
                                sandbox: modal.sandbox.name,
                                delete_files: false,
                            }]
                        }
                    }
                    DeleteSandboxStage::Files => vec![Effect::DeleteSandbox {
                        sandbox: modal.sandbox.name,
                        delete_files: true,
                    }],
                },
                KeyCode::Char('n') | KeyCode::Char('k')
                    if modal.stage == DeleteSandboxStage::Files =>
                {
                    vec![Effect::DeleteSandbox {
                        sandbox: modal.sandbox.name,
                        delete_files: false,
                    }]
                }
                KeyCode::Char('n') => Vec::new(),
                KeyCode::Char('j') | KeyCode::Down => {
                    self.modal = Some(Modal::DeleteSandbox(modal));
                    self.scroll_modal(1);
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up if modal.stage == DeleteSandboxStage::Confirm => {
                    self.modal = Some(Modal::DeleteSandbox(modal));
                    self.scroll_modal(-1);
                    Vec::new()
                }
                KeyCode::PageDown => {
                    self.modal = Some(Modal::DeleteSandbox(modal));
                    self.scroll_modal_page(1);
                    Vec::new()
                }
                KeyCode::PageUp => {
                    self.modal = Some(Modal::DeleteSandbox(modal));
                    self.scroll_modal_page(-1);
                    Vec::new()
                }
                _ => {
                    self.modal = Some(Modal::DeleteSandbox(modal));
                    Vec::new()
                }
            },
            Modal::MultiAgent(mut form) => match key.code {
                KeyCode::Esc => Vec::new(),
                KeyCode::Up | KeyCode::Char('k') => {
                    form.count = form.count.saturating_add(1).min(8);
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    form.count = form.count.saturating_sub(1).max(1);
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    let index = Harness::ALL
                        .iter()
                        .position(|harness| *harness == form.harness)
                        .unwrap_or(0);
                    let next = if key.code == KeyCode::Left {
                        (index + Harness::ALL.len() - 1) % Harness::ALL.len()
                    } else {
                        (index + 1) % Harness::ALL.len()
                    };
                    form.harness = Harness::ALL[next];
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
                KeyCode::Char(' ') => {
                    form.start = !form.start;
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
                KeyCode::PageDown => {
                    self.modal = Some(Modal::MultiAgent(form));
                    self.scroll_modal_page(1);
                    Vec::new()
                }
                KeyCode::PageUp => {
                    self.modal = Some(Modal::MultiAgent(form));
                    self.scroll_modal_page(-1);
                    Vec::new()
                }
                KeyCode::Enter => vec![Effect::CloneSandbox {
                    source: form.source,
                    count: form.count,
                    harness: form.harness,
                    start: form.start,
                }],
                _ => {
                    self.modal = Some(Modal::MultiAgent(form));
                    Vec::new()
                }
            },
            Modal::Doctor(mut modal) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Vec::new(),
                KeyCode::Char('r') => {
                    modal.report = crate::doctor::run(&self.config);
                    modal.downloading = false;
                    modal.scroll = 0;
                    self.modal = Some(Modal::Doctor(modal));
                    Vec::new()
                }
                KeyCode::Char('d') if modal.report.can_download() && !modal.downloading => {
                    modal.downloading = true;
                    self.modal = Some(Modal::Doctor(modal));
                    vec![Effect::DownloadImages]
                }
                KeyCode::Char('s')
                    if modal.report.can_setup() && !modal.downloading && !modal.setting_up =>
                {
                    self.modal = Some(Modal::Doctor(modal));
                    self.open_setup_confirmation()
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.modal = Some(Modal::Doctor(modal));
                    self.scroll_modal(1);
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.modal = Some(Modal::Doctor(modal));
                    self.scroll_modal(-1);
                    Vec::new()
                }
                KeyCode::PageDown => {
                    self.modal = Some(Modal::Doctor(modal));
                    self.scroll_modal_page(1);
                    Vec::new()
                }
                KeyCode::PageUp => {
                    self.modal = Some(Modal::Doctor(modal));
                    self.scroll_modal_page(-1);
                    Vec::new()
                }
                _ => {
                    self.modal = Some(Modal::Doctor(modal));
                    Vec::new()
                }
            },
            Modal::Security(prompt) => self.handle_security_key(key, prompt),
            Modal::ImportProject(mut form) => {
                if key.code == KeyCode::Esc {
                    return Vec::new();
                }
                match key.code {
                    KeyCode::Tab | KeyCode::BackTab => {
                        form.cycle_focus();
                        self.modal = Some(Modal::ImportProject(form));
                    }
                    KeyCode::Enter => return self.submit_import_project(form),
                    _ => {
                        self.edit_import_form(&mut form, key);
                        self.modal = Some(Modal::ImportProject(form));
                    }
                }
                Vec::new()
            }
            Modal::CreateProject(mut form) => {
                if key.code == KeyCode::Esc {
                    return Vec::new();
                }
                match key.code {
                    KeyCode::Tab => {
                        form.cycle_focus(false);
                        self.modal = Some(Modal::CreateProject(form));
                    }
                    KeyCode::BackTab => {
                        form.cycle_focus(true);
                        self.modal = Some(Modal::CreateProject(form));
                    }
                    KeyCode::Enter if form.focus == CreateFocus::Create => {
                        return self.submit_create_project(form);
                    }
                    KeyCode::Enter => {
                        form.cycle_focus(false);
                        self.modal = Some(Modal::CreateProject(form));
                    }
                    KeyCode::Char(' ') if form.focus == CreateFocus::Packages => {
                        if let Some(pkg) = form.packages.get_mut(form.package_cursor) {
                            pkg.selected = !pkg.selected;
                        }
                        self.modal = Some(Modal::CreateProject(form));
                    }
                    _ => {
                        self.edit_create_form(&mut form, key);
                        self.modal = Some(Modal::CreateProject(form));
                    }
                }
                Vec::new()
            }
        }
    }

    fn edit_create_form(&self, form: &mut CreateProjectForm, key: KeyEvent) {
        match form.focus {
            CreateFocus::Name => match key.code {
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    form.name.push(c)
                }
                KeyCode::Backspace => {
                    form.name.pop();
                }
                _ => {}
            },
            CreateFocus::Harness => {
                let harnesses = Harness::ALL;
                let index = harnesses
                    .iter()
                    .position(|h| *h == form.harness)
                    .unwrap_or(0);
                match key.code {
                    KeyCode::Left => {
                        form.harness = harnesses[(index + harnesses.len() - 1) % harnesses.len()];
                    }
                    KeyCode::Right | KeyCode::Char(' ') => {
                        form.harness = harnesses[(index + 1) % harnesses.len()];
                    }
                    _ => {}
                }
            }
            CreateFocus::Packages => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    form.package_cursor = form
                        .package_cursor
                        .checked_sub(1)
                        .unwrap_or(form.packages.len().saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    form.package_cursor = (form.package_cursor + 1) % form.packages.len().max(1);
                }
                KeyCode::Char(' ') => {
                    if let Some(pkg) = form.packages.get_mut(form.package_cursor) {
                        pkg.selected = !pkg.selected;
                    }
                }
                _ => {}
            },
            CreateFocus::Create => {
                if key.code == KeyCode::Char(' ') {
                    // Submit is handled on Enter; space is harmless here.
                }
            }
        }
    }

    fn submit_create_project(&mut self, form: CreateProjectForm) -> Vec<Effect> {
        let templates = self.config.templates_dir.join("AGENTS.md");
        let template = templates.is_file().then_some(templates.as_path());
        match project::create_project(
            &self.config.projects_root,
            &form.name,
            template,
            form.harness,
        ) {
            Ok(path) => {
                let meta = ProjectMeta {
                    harness: Some(form.harness),
                    packages: form.selected_packages(),
                    template: template.map(|path| path.display().to_string()),
                };
                if let Err(err) = project::write_meta(&path, &meta) {
                    self.toast(
                        format!("project created, metadata failed: {err}"),
                        ToastLevel::Warn,
                    );
                }
                let loaded = Project::load(path.clone());
                self.add_project(loaded);
                self.toast(format!("created {}", path.display()), ToastLevel::Info);
            }
            Err(err) => {
                let mut form = form;
                form.error = Some(err.to_string());
                self.modal = Some(Modal::CreateProject(form));
            }
        }
        Vec::new()
    }

    fn edit_import_form(&self, form: &mut ImportProjectForm, key: KeyEvent) {
        let field = match form.focus {
            ImportFocus::Path => &mut form.path,
            ImportFocus::Name => &mut form.name,
        };
        match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if field.len() < 4096 {
                    field.push(c);
                }
            }
            KeyCode::Backspace => {
                field.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                field.clear();
            }
            _ => {}
        }
    }

    fn submit_import_project(&mut self, form: ImportProjectForm) -> Vec<Effect> {
        let path = form.path.trim();
        if path.is_empty() {
            let mut form = form;
            form.error = Some("enter the path of an existing directory".into());
            self.modal = Some(Modal::ImportProject(form));
            return Vec::new();
        }
        let name = {
            let name = form.name.trim();
            (!name.is_empty()).then_some(name)
        };
        match self.config.import_project(Path::new(path), name) {
            Ok(link) => {
                let loaded = Project::load(link.clone());
                let target = link
                    .canonicalize()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| link.display().to_string());
                self.add_project(loaded);
                self.toast(
                    format!("imported {} → {target}", link.display()),
                    ToastLevel::Info,
                );
            }
            Err(err) => {
                let mut form = form;
                form.error = Some(err.to_string());
                self.modal = Some(Modal::ImportProject(form));
            }
        }
        Vec::new()
    }

    fn handle_confirm_key(&mut self, key: KeyEvent, prompt: ConfirmPrompt) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.modal = Some(Modal::Confirm(prompt));
                self.scroll_modal(1);
                return Vec::new();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.modal = Some(Modal::Confirm(prompt));
                self.scroll_modal(-1);
                return Vec::new();
            }
            KeyCode::PageDown => {
                self.modal = Some(Modal::Confirm(prompt));
                self.scroll_modal_page(1);
                return Vec::new();
            }
            KeyCode::PageUp => {
                self.modal = Some(Modal::Confirm(prompt));
                self.scroll_modal_page(-1);
                return Vec::new();
            }
            _ => {}
        }
        // Destructive confirmation must be explicit: Enter never confirms.
        let confirmed = matches!(key.code, KeyCode::Char('y'));
        let cancelled = matches!(key.code, KeyCode::Char('n') | KeyCode::Esc);
        if cancelled {
            return Vec::new();
        }
        if !confirmed {
            self.modal = Some(Modal::Confirm(prompt));
            return Vec::new();
        }
        match prompt.action {
            ConfirmAction::DeleteSnapshot(name) => {
                match SnapshotStore::new(&self.config.snapshots_dir).delete(&name) {
                    Ok(()) => {
                        self.toast(format!("deleted snapshot {name}"), ToastLevel::Info);
                        self.refresh_snapshots();
                    }
                    Err(err) => self.toast(format!("delete failed: {err}"), ToastLevel::Error),
                }
                Vec::new()
            }
            ConfirmAction::RestoreSnapshot(snapshot) => vec![Effect::RestoreSnapshot(*snapshot)],
            ConfirmAction::SetupEnvironment => vec![Effect::SetupEnvironment],
        }
    }

    fn handle_security_key(&mut self, key: KeyEvent, prompt: SecurityPrompt) -> Vec<Effect> {
        match key.code {
            // Allowing a flagged command is destructive; Enter deliberately
            // falls through to the catch-all and leaves the prompt open.
            KeyCode::Char('a') => {
                if self.write_line(&prompt.command) {
                    self.toast("command approved once", ToastLevel::Warn);
                }
                Vec::new()
            }
            KeyCode::Char('c') => {
                match project::create_checkpoint(&prompt.project, "before flagged command") {
                    Ok(branch) => {
                        if self.write_line(&prompt.command) {
                            self.toast(
                                format!("checkpoint {branch}; command approved"),
                                ToastLevel::Info,
                            );
                        } else {
                            self.toast(
                                format!(
                                    "checkpoint {branch} created, but the command was not sent"
                                ),
                                ToastLevel::Warn,
                            );
                        }
                    }
                    Err(err) => {
                        let mut prompt = prompt;
                        prompt.error = Some(err.to_string());
                        self.modal = Some(Modal::Security(prompt));
                    }
                }
                Vec::new()
            }
            KeyCode::Char('d') | KeyCode::Esc => {
                self.toast("command blocked", ToastLevel::Warn);
                Vec::new()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.modal = Some(Modal::Security(prompt));
                self.scroll_modal(1);
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.modal = Some(Modal::Security(prompt));
                self.scroll_modal(-1);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.modal = Some(Modal::Security(prompt));
                self.scroll_modal_page(1);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.modal = Some(Modal::Security(prompt));
                self.scroll_modal_page(-1);
                Vec::new()
            }
            _ => {
                self.modal = Some(Modal::Security(prompt));
                Vec::new()
            }
        }
    }

    fn confirm_delete_snapshot(&mut self) -> Vec<Effect> {
        if let Some(snapshot) = self.selected_snapshot().cloned() {
            self.modal = Some(Modal::Confirm(ConfirmPrompt {
                title: "Delete snapshot".into(),
                message: format!(
                    "Delete '{}' ({})?\nThis cannot be undone.",
                    snapshot.name,
                    snapshot.size_label()
                ),
                confirm_label: "Delete".into(),
                action: ConfirmAction::DeleteSnapshot(snapshot.name),
                scroll: 0,
            }));
        } else {
            self.toast("no snapshot selected", ToastLevel::Warn);
        }
        Vec::new()
    }

    fn confirm_restore_snapshot(&mut self) -> Vec<Effect> {
        if let Some(snapshot) = self.selected_snapshot().cloned() {
            self.modal = Some(Modal::Confirm(ConfirmPrompt {
                title: "Restore snapshot".into(),
                message: format!(
                    "Load VM state '{}'?\nThe current VM session will be replaced.",
                    snapshot.name
                ),
                confirm_label: "Restore".into(),
                action: ConfirmAction::RestoreSnapshot(Box::new(snapshot)),
                scroll: 0,
            }));
        }
        Vec::new()
    }

    fn attach_selected_sandbox(&mut self) -> Vec<Effect> {
        let Some(sandbox) = self.selected_sandbox().cloned() else {
            self.toast("no sandbox selected", ToastLevel::Warn);
            return Vec::new();
        };
        if self.sandbox.state == SandboxState::Starting {
            self.toast("a VM is already starting…", ToastLevel::Warn);
            return Vec::new();
        }
        self.session = None;
        self.terminal.clear();
        self.scroll = 0;
        self.input.clear();
        self.terminal_mode = TerminalMode::Normal;
        self.session_closed_reported = false;
        self.sandbox.state = SandboxState::Starting;
        self.sandbox.started_at = None;
        self.sandbox.note = Some(format!("attaching to sandbox {}", sandbox.name));
        vec![Effect::AttachSandbox {
            sandbox: sandbox.name,
        }]
    }

    fn stop_selected_sandbox(&mut self) -> Vec<Effect> {
        let Some(sandbox) = self.selected_sandbox().cloned() else {
            self.toast("no sandbox selected", ToastLevel::Warn);
            return Vec::new();
        };
        vec![Effect::StopSandbox {
            sandbox: sandbox.name,
        }]
    }

    fn confirm_delete_sandbox(&mut self) -> Vec<Effect> {
        let Some(sandbox) = self.selected_sandbox().cloned() else {
            self.toast("no sandbox selected", ToastLevel::Warn);
            return Vec::new();
        };
        self.modal = Some(Modal::DeleteSandbox(DeleteSandboxModal {
            sandbox,
            stage: DeleteSandboxStage::Confirm,
            error: None,
            scroll: 0,
        }));
        Vec::new()
    }

    /// Delete the VM/sandbox of the selected project without starting it.
    ///
    /// The project directory itself is never touched. This is the dashboard
    /// counterpart of `hegn sandbox delete`, and importantly it does not run
    /// any host prerequisite checks: a VM whose start failed (missing images,
    /// inactive firewall, broken relay) can still be removed.
    fn confirm_delete_project_sandbox(&mut self) -> Vec<Effect> {
        let Some(project) = self.selected_project().cloned() else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };
        let sandbox = self
            .sandboxes
            .iter()
            .filter(|sandbox| crate::sandbox::same_project(&sandbox.project, &project.path))
            // Prefer the base sandbox over multi-agent clones.
            .min_by_key(|sandbox| sandbox.ephemeral)
            .cloned();
        let Some(sandbox) = sandbox else {
            self.toast(
                format!("no VM for {} — nothing to delete", project.name),
                ToastLevel::Warn,
            );
            return Vec::new();
        };
        self.modal = Some(Modal::DeleteSandbox(DeleteSandboxModal {
            sandbox,
            stage: DeleteSandboxStage::Confirm,
            error: None,
            scroll: 0,
        }));
        Vec::new()
    }

    fn workspace_is_inside_sandbox(&self, sandbox: &SandboxSummary) -> bool {
        let dir = self.config.sandboxes_dir.join(&sandbox.name);
        let dir = dir.canonicalize().unwrap_or(dir);
        let workspace = sandbox
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| sandbox.workspace.clone());
        workspace.starts_with(&dir)
    }

    fn open_global_config(&mut self) -> Vec<Effect> {
        self.show_global_config_form(None);
        Vec::new()
    }

    fn show_global_config_form(&mut self, status: Option<String>) {
        let mut form = ConfigForm::global(self.config.clone(), Config::config_path());
        form.status = status;
        self.config_hover = None;
        self.modal = Some(Modal::Config(form));
    }

    /// Reopen the global settings modal on a specific page, e.g. after editing
    /// a harness profile from the Profiles tab.
    pub fn reopen_global_config(&mut self, page: ConfigPage, status: Option<String>) {
        self.show_global_config_form(status);
        if let Some(Modal::Config(form)) = self.modal.as_mut() {
            form.set_page(page);
        }
    }

    fn open_sandbox_config(&mut self) -> Vec<Effect> {
        let Some(sandbox) = self.selected_sandbox().map(|sandbox| sandbox.name.clone()) else {
            self.toast("select a sandbox first", ToastLevel::Warn);
            return Vec::new();
        };
        self.show_sandbox_config_form(&sandbox, None);
        Vec::new()
    }

    fn show_sandbox_config_form(&mut self, name: &str, status: Option<String>) {
        let store = crate::sandbox_config::SandboxStore::new(self.config.clone());
        match store.load(name) {
            Ok(sandbox) => {
                let mut form = ConfigForm::sandbox(sandbox, self.config.clone());
                form.status = status;
                self.config_hover = None;
                self.modal = Some(Modal::Config(form));
            }
            Err(err) => self.toast(
                format!("could not load sandbox config: {err:#}"),
                ToastLevel::Error,
            ),
        }
    }

    /// Persist the settings modal and apply the result. The modal stays open so
    /// the user can keep adjusting; a failed save leaves the draft intact.
    fn save_config_form(&mut self, mut form: ConfigForm) -> Vec<Effect> {
        match form.save() {
            Some(Applied::Global(config)) => {
                let projects_root_changed = self.config.projects_root != config.projects_root;
                let endpoints_changed = self.config.endpoint_ports() != config.endpoint_ports();
                self.apply_global_config(*config);
                self.modal = Some(Modal::Config(form));
                let effects = if projects_root_changed {
                    self.toast("settings saved — rescanning projects", ToastLevel::Info);
                    vec![Effect::RefreshProjects]
                } else {
                    self.toast("settings saved", ToastLevel::Info);
                    Vec::new()
                };
                if endpoints_changed {
                    self.toast(
                        "settings saved — run `sudo hegn network setup` for the new endpoints",
                        ToastLevel::Warn,
                    );
                }
                effects
            }
            Some(Applied::Sandbox) => {
                self.refresh_sandboxes();
                self.modal = Some(Modal::Config(form));
                self.toast("sandbox settings saved", ToastLevel::Info);
                Vec::new()
            }
            None => {
                self.modal = Some(Modal::Config(form));
                Vec::new()
            }
        }
    }

    /// Reload the settings modal from disk, discarding unsaved changes.
    fn revert_config_form(&mut self, mut form: ConfigForm) -> Vec<Effect> {
        match form.kind {
            ConfigKind::Global => match Config::load() {
                Ok(config) => {
                    let projects_root_changed = self.config.projects_root != config.projects_root;
                    self.apply_global_config(config);
                    self.show_global_config_form(Some("reverted to the file on disk".into()));
                    if projects_root_changed {
                        vec![Effect::RefreshProjects]
                    } else {
                        Vec::new()
                    }
                }
                Err(err) => {
                    form.error = Some(format!("reload failed: {err:#}"));
                    self.modal = Some(Modal::Config(form));
                    Vec::new()
                }
            },
            ConfigKind::Sandbox => {
                match form.sandbox_name().map(str::to_string) {
                    Some(name) => self.show_sandbox_config_form(
                        &name,
                        Some("reverted to the file on disk".into()),
                    ),
                    None => {
                        form.error = Some("sandbox name is missing".into());
                        self.modal = Some(Modal::Config(form));
                    }
                }
                Vec::new()
            }
        }
    }

    /// Install a saved global configuration in the running UI.
    fn apply_global_config(&mut self, config: Config) {
        self.security_engine = SecurityEngine::new(config.security);
        self.store = StateStore::load(config.state_dir.join("state.json"));
        self.config = config;
        self.sandbox.vcpus = self.config.vcpus;
        self.sandbox.memory_mib = self.config.memory_mib;
        self.sandbox.virtiofs = if self.config.vm_assets_ready() {
            "configured"
        } else {
            "missing VM"
        };
        self.refresh_snapshots();
        self.refresh_sandboxes();
    }

    fn open_multi_agent(&mut self) -> Vec<Effect> {
        let Some(session) = self.session.as_ref() else {
            self.toast(
                "open a project first — multi-agent clones live inside its terminal",
                ToastLevel::Warn,
            );
            return Vec::new();
        };
        let sandbox = session.sandbox_name().to_string();
        let harness = self
            .sandboxes
            .iter()
            .find(|entry| entry.name == sandbox)
            .map(|entry| entry.harness)
            .unwrap_or(self.config.harness);
        self.modal = Some(Modal::MultiAgent(MultiAgentForm::new(sandbox, harness)));
        Vec::new()
    }

    /// Reload global and sandbox configuration after an editor round-trip.
    pub fn reload_after_edit(&mut self) {
        match Config::load() {
            Ok(config) => self.apply_global_config(config),
            Err(err) => self.toast(format!("config reload failed: {err}"), ToastLevel::Error),
        }
    }

    /// Open the selected project in a Cloud Hypervisor VM.
    ///
    /// This only *requests* the VM start; the terminal loop owns the runtime and
    /// performs the boot. A missing VM is a hard error, never a local shell.
    pub fn open_selected(&mut self) -> Vec<Effect> {
        if self.focus == Focus::Snapshots {
            return self.confirm_restore_snapshot();
        }
        if self.focus == Focus::Sandboxes {
            return self.attach_selected_sandbox();
        }
        if self.sandbox.state == SandboxState::Starting {
            self.toast("a VM is already starting…", ToastLevel::Warn);
            return Vec::new();
        }
        let Some(project) = self.selected_project().cloned() else {
            self.toast("no project selected", ToastLevel::Warn);
            return Vec::new();
        };

        let attached_to_project = self
            .session
            .as_ref()
            .is_some_and(|session| crate::sandbox::same_project(session.project(), &project.path));
        if attached_to_project && self.session_active() {
            self.screen = Screen::Terminal;
            return Vec::new();
        }

        let meta = project::read_meta(&project.path);
        let harness = meta.harness.unwrap_or(self.config.harness);

        if let Err(err) = self.store.touch(&project.path) {
            self.toast(
                format!("could not save recent projects: {err}"),
                ToastLevel::Warn,
            );
        }

        self.session = None;
        self.terminal.clear();
        self.scroll = 0;
        self.input.clear();
        self.terminal_mode = TerminalMode::Normal;
        self.session_closed_reported = false;
        self.sandbox.state = SandboxState::Starting;
        self.sandbox.started_at = None;
        self.sandbox.note = Some(format!(
            "booting Cloud Hypervisor VM for {}",
            project.display_path()
        ));
        vec![Effect::StartProjectSandbox {
            project: project.path.clone(),
            harness,
        }]
    }

    /// Install a console for a successfully started/attached VM.
    pub fn attach_console(&mut self, console: Box<dyn TerminalConsole>) {
        self.sandbox.state = SandboxState::Running;
        self.sandbox.backend = "cloud-hypervisor";
        self.sandbox.note = Some(format!(
            "{} in {}",
            console.label(),
            console.project().display()
        ));
        self.sandbox.started_at = Some(Instant::now());
        self.sandbox.socket = Some(
            self.config
                .sandboxes_dir
                .join(console.sandbox_name())
                .join("run/cloud-hypervisor.sock"),
        );
        if let Some(index) = self
            .sandbox_visible
            .iter()
            .position(|&index| self.sandboxes[index].name == console.sandbox_name())
        {
            self.sandbox_selected = index;
        }
        self.session = Some(console);
        self.screen = Screen::Terminal;
        self.terminal_mode = TerminalMode::Normal;
        self.session_closed_reported = false;
        self.toast(
            "attached — normal mode, press i or a to type",
            ToastLevel::Info,
        );
        self.refresh_sandboxes();
    }

    /// Report a hard VM start/attach failure. No host fallback is attempted.
    ///
    /// The doctor modal opens automatically so the missing prerequisite and the
    /// image download hint are visible without leaving the TUI.
    pub fn fail_start(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.session = None;
        self.sandbox.state = SandboxState::Failed;
        self.sandbox.started_at = None;
        self.sandbox.note = Some(error.clone());
        self.toast(format!("VM start failed: {error}"), ToastLevel::Error);
        self.modal = Some(Modal::Doctor(DoctorModal::with_error(
            crate::doctor::run(&self.config),
            error,
        )));
    }

    /// Open the explicit confirmation for the automatic first-time setup.
    ///
    /// The setup itself may ask for sudo/pkexec; this confirmation is the
    /// in-TUI "yes" before anything privileged is attempted.
    pub fn open_setup_confirmation(&mut self) -> Vec<Effect> {
        let actions = crate::doctor::setup_actions(&self.config);
        if !actions.any() {
            self.toast("nothing to set up", ToastLevel::Info);
            return Vec::new();
        }
        let mut lines = vec![
            "hegn can prepare this machine automatically:".to_string(),
            String::new(),
        ];
        if actions.create_dirs {
            lines.push("• create missing state/project/template/log directories".into());
        }
        if actions.init_config {
            lines.push("• write the default ~/.config/hegn/config.toml".into());
        }
        if actions.init_profiles {
            lines
                .push("• write per-harness profile templates under ~/.config/hegn/profiles".into());
        }
        if actions.download_images {
            lines.push("• download and SHA-256 verify the VM kernel and rootfs".into());
        }
        if actions.configure_network {
            lines.push(
                "• create the guarded TAP pool and host nftables rules (asks for sudo)".into(),
            );
        }
        lines.push(String::new());
        lines.push("Continue?".into());
        self.modal = Some(Modal::Confirm(ConfirmPrompt {
            title: "First-time setup".into(),
            message: lines.join("\n"),
            confirm_label: "Set up".into(),
            action: ConfirmAction::SetupEnvironment,
            scroll: 0,
        }));
        Vec::new()
    }

    /// Install a fresh doctor report, e.g. after a background image download.
    pub fn show_doctor(&mut self, report: DoctorReport, note: Option<String>) {
        if let Some(note) = note {
            let level = if note.contains("failed") {
                ToastLevel::Error
            } else {
                ToastLevel::Info
            };
            self.toast(note, level);
        }
        self.modal = Some(Modal::Doctor(DoctorModal::new(report)));
    }

    /// Feed a synthetic key to the multi-agent modal (used by clickable hints).
    fn multi_agent_key(&mut self, code: KeyCode) -> Vec<Effect> {
        if matches!(self.modal, Some(Modal::MultiAgent(_))) {
            self.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        } else {
            Vec::new()
        }
    }

    /// Dispatch a footer click or a direct action (mouse support).
    pub fn handle_action(&mut self, action: ActionId) -> Vec<Effect> {
        match action {
            ActionId::Open => self.open_selected(),
            ActionId::Create => {
                self.modal = Some(Modal::CreateProject(CreateProjectForm::new(
                    self.config.harness,
                )));
                Vec::new()
            }
            ActionId::Import => {
                self.modal = Some(Modal::ImportProject(ImportProjectForm::new()));
                Vec::new()
            }
            ActionId::Confirm => match self.modal {
                Some(Modal::MultiAgent(_)) => {
                    self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                }
                Some(Modal::Confirm(_) | Modal::DeleteSandbox(_)) => {
                    self.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                }
                _ => Vec::new(),
            },
            ActionId::Cancel => {
                if matches!(
                    self.modal,
                    Some(Modal::Confirm(_) | Modal::DeleteSandbox(_) | Modal::MultiAgent(_))
                ) {
                    self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::NextField => {
                if matches!(
                    self.modal,
                    Some(Modal::CreateProject(_) | Modal::ImportProject(_))
                ) {
                    self.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::SubmitCreate => match self.modal.take() {
                Some(Modal::CreateProject(form)) => self.submit_create_project(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::SubmitImport => match self.modal.take() {
                Some(Modal::ImportProject(form)) => self.submit_import_project(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::Edit => self.effect_for_selected(Effect::OpenEditor),
            ActionId::Lazygit => self.effect_for_selected(Effect::OpenLazygit),
            ActionId::Refresh => vec![Effect::RefreshProjects, Effect::RefreshSandboxes],
            ActionId::Doctor => {
                self.modal = Some(Modal::Doctor(DoctorModal::new(crate::doctor::run(
                    &self.config,
                ))));
                Vec::new()
            }
            ActionId::DoctorSetup => {
                if matches!(self.modal, Some(Modal::Doctor(_))) {
                    self.open_setup_confirmation()
                } else {
                    Vec::new()
                }
            }
            ActionId::DownloadImages => match self.modal.take() {
                Some(Modal::Doctor(mut modal)) if modal.report.can_download() => {
                    modal.downloading = true;
                    self.modal = Some(Modal::Doctor(modal));
                    vec![Effect::DownloadImages]
                }
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::Help => {
                self.modal = Some(Modal::Help(HelpState::default()));
                Vec::new()
            }
            ActionId::HelpSearch => {
                if let Some(Modal::Help(help)) = self.modal.as_mut() {
                    help.searching = true;
                }
                Vec::new()
            }
            ActionId::HelpSearchStop => {
                if let Some(Modal::Help(help)) = self.modal.as_mut() {
                    help.searching = false;
                }
                Vec::new()
            }
            ActionId::Quit => {
                self.should_quit = true;
                vec![Effect::Quit]
            }
            ActionId::Back => {
                self.screen = Screen::Dashboard;
                Vec::new()
            }
            ActionId::ToggleRaw => {
                self.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE))
            }
            ActionId::InsertMode => {
                if self.screen == Screen::Terminal {
                    self.terminal_mode = TerminalMode::Insert;
                    self.toast("insert mode — Esc returns to normal", ToastLevel::Info);
                }
                Vec::new()
            }
            ActionId::NormalMode => {
                if self.screen == Screen::Terminal {
                    self.terminal_mode = TerminalMode::Normal;
                    self.toast("normal mode — press i or a to type", ToastLevel::Info);
                }
                Vec::new()
            }
            ActionId::ScrollUp => {
                if self.screen == Screen::Terminal {
                    self.scroll = self.scroll.saturating_add(5).min(self.terminal.len());
                }
                Vec::new()
            }
            ActionId::Restore => self.confirm_restore_snapshot(),
            ActionId::DeleteSnapshot => self.confirm_delete_snapshot(),
            ActionId::FocusSandboxes => {
                self.focus = Focus::Sandboxes;
                self.refresh_sandboxes();
                Vec::new()
            }
            ActionId::StartSandbox => self.open_selected(),
            ActionId::StopSandbox => self.stop_selected_sandbox(),
            ActionId::DeleteSandbox => self.confirm_delete_sandbox(),
            ActionId::SandboxConfig => self.open_sandbox_config(),
            ActionId::GlobalConfig => self.open_global_config(),
            ActionId::CloneSandbox | ActionId::MultiAgent => self.open_multi_agent(),
            ActionId::KeepFiles => {
                self.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            }
            ActionId::EditConfig => match self.modal.take() {
                Some(Modal::Config(mut form)) => {
                    if form.is_editing() {
                        form.error = Some("apply or cancel the current edit first".into());
                        self.modal = Some(Modal::Config(form));
                        Vec::new()
                    } else if form.dirty() {
                        form.error = Some("save or revert before editing the raw file".into());
                        self.modal = Some(Modal::Config(form));
                        Vec::new()
                    } else {
                        vec![Effect::OpenEditor(form.path)]
                    }
                }
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::SaveConfig => match self.modal.take() {
                Some(Modal::Config(form)) => self.save_config_form(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::ReloadConfig => match self.modal.take() {
                Some(Modal::Config(form)) => self.revert_config_form(form),
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::NewSnapshot => {
                self.toast(
                    "create snapshots with: hegn snapshot create",
                    ToastLevel::Info,
                );
                Vec::new()
            }
            ActionId::Allow => {
                if self.modal.is_some() {
                    self.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::Checkpoint => {
                if self.modal.is_some() {
                    self.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::Deny => {
                if self.modal.is_some() {
                    self.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::MultiAgentMore => self.multi_agent_key(KeyCode::Up),
            ActionId::MultiAgentFewer => self.multi_agent_key(KeyCode::Down),
            ActionId::HarnessPrev => self.multi_agent_key(KeyCode::Left),
            ActionId::HarnessNext => self.multi_agent_key(KeyCode::Right),
            ActionId::ToggleCloneStart => self.multi_agent_key(KeyCode::Char(' ')),
            ActionId::Close => {
                self.modal = None;
                self.scroll_hover = false;
                self.scroll_dragging = false;
                Vec::new()
            }
        }
    }

    /// Scroll offset of the open modal, if its body scrolls independently.
    fn modal_scroll_offset(&self) -> Option<usize> {
        match &self.modal {
            Some(Modal::Help(help)) => Some(help.scroll),
            Some(Modal::Doctor(modal)) => Some(modal.scroll),
            Some(Modal::Security(prompt)) => Some(prompt.scroll),
            Some(Modal::Confirm(prompt)) => Some(prompt.scroll),
            Some(Modal::DeleteSandbox(modal)) => Some(modal.scroll),
            Some(Modal::MultiAgent(form)) => Some(form.scroll),
            Some(Modal::Error { scroll, .. }) => Some(*scroll),
            _ => None,
        }
    }

    fn set_modal_scroll(&mut self, offset: usize) {
        match self.modal.as_mut() {
            Some(Modal::Help(help)) => help.scroll = offset,
            Some(Modal::Doctor(modal)) => modal.scroll = offset,
            Some(Modal::Security(prompt)) => prompt.scroll = offset,
            Some(Modal::Confirm(prompt)) => prompt.scroll = offset,
            Some(Modal::DeleteSandbox(modal)) => modal.scroll = offset,
            Some(Modal::MultiAgent(form)) => form.scroll = offset,
            Some(Modal::Error { scroll, .. }) => *scroll = offset,
            _ => {}
        }
    }

    /// Move the open modal's body by `delta` lines, clamped to its track.
    fn scroll_modal(&mut self, delta: isize) {
        let Some(hit) = self.modal_scroll else { return };
        let Some(current) = self.modal_scroll_offset() else {
            return;
        };
        let current = current.min(hit.max);
        self.set_modal_scroll(current.saturating_add_signed(delta).min(hit.max));
    }

    fn scroll_modal_page(&mut self, pages: isize) {
        let Some(hit) = self.modal_scroll else { return };
        let step = hit.page.max(1) as isize;
        self.scroll_modal(step * pages);
    }

    /// Map a click or drag row on the scrollbar to an offset. The settings form
    /// and the create form follow their selection, so the bar moves the
    /// selection instead of a free offset.
    fn jump_modal_scroll(&mut self, row: u16) {
        let Some(hit) = self.modal_scroll else { return };
        let track = hit.rect.height.max(1) as usize;
        let relative = row.saturating_sub(hit.rect.y) as usize;
        let span = track.saturating_sub(1).max(1);
        match self.modal.as_mut() {
            Some(Modal::Config(form)) => {
                let rows = form.rows().len();
                if rows > 1 {
                    form.select_row(relative * (rows - 1) / span);
                }
            }
            Some(Modal::CreateProject(form)) => {
                let rows = form.packages.len();
                if rows > 1 {
                    form.package_cursor = relative * (rows - 1) / span;
                    form.focus = CreateFocus::Packages;
                }
            }
            _ => {
                let offset = relative * hit.max / span;
                self.set_modal_scroll(offset.min(hit.max));
            }
        }
    }

    /// Mouse handling for footer clicks, row selection and scrolling.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Vec<Effect> {
        match mouse.kind {
            MouseEventKind::Moved => {
                let pos = (mouse.column, mouse.row);
                self.hovered = self
                    .footer_hits
                    .iter()
                    .find(|hit| rect_contains(hit.rect, pos))
                    .map(|hit| hit.action);
                self.config_hover = self
                    .config_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .map(|row| row.index);
                self.scroll_hover = self.scroll_dragging
                    || self
                        .modal_scroll
                        .is_some_and(|hit| rect_contains(hit.rect, pos));
                Vec::new()
            }
            MouseEventKind::ScrollUp => {
                if let Some(Modal::Config(form)) = self.modal.as_mut() {
                    form.select_delta(-1);
                } else if let Some(Modal::CreateProject(form)) = self.modal.as_mut() {
                    form.package_cursor = form.package_cursor.saturating_sub(1);
                } else if self.modal.is_some() {
                    self.scroll_modal(-3);
                } else if self.screen == Screen::Terminal {
                    self.scroll = self.scroll.saturating_add(3).min(self.terminal.len());
                } else {
                    self.select_next(-1);
                }
                Vec::new()
            }
            MouseEventKind::ScrollDown => {
                if let Some(Modal::Config(form)) = self.modal.as_mut() {
                    form.select_delta(1);
                } else if let Some(Modal::CreateProject(form)) = self.modal.as_mut() {
                    form.package_cursor =
                        (form.package_cursor + 1).min(form.packages.len().saturating_sub(1));
                } else if self.modal.is_some() {
                    self.scroll_modal(3);
                } else if self.screen == Screen::Terminal {
                    self.scroll = self.scroll.saturating_sub(3);
                } else {
                    self.select_next(1);
                }
                Vec::new()
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.scroll_dragging {
                    self.scroll_hover = true;
                    self.jump_modal_scroll(mouse.row);
                }
                Vec::new()
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.scroll_dragging = false;
                Vec::new()
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = (mouse.column, mouse.row);
                if let Some(hit) = self
                    .footer_hits
                    .iter()
                    .find(|hit| rect_contains(hit.rect, pos))
                    .copied()
                {
                    return self.handle_action(hit.action);
                }
                if let Some(hit) = self.modal_scroll
                    && rect_contains(hit.rect, pos)
                {
                    self.scroll_dragging = true;
                    self.scroll_hover = true;
                    self.jump_modal_scroll(pos.1);
                    return Vec::new();
                }
                if matches!(self.modal, Some(Modal::Config(_))) {
                    return self.click_config_row(pos);
                }
                if let Some(row) = self
                    .project_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .copied()
                {
                    self.focus = Focus::Projects;
                    self.selected = row.index;
                    self.refresh_sandbox_filter();
                    let path = self.selected_project().map(|p| p.path.clone());
                    if let Some(path) = path {
                        let now = Instant::now();
                        let double = self.last_click.as_ref().is_some_and(|(at, p)| {
                            p == &path && now.duration_since(*at) < DOUBLE_CLICK
                        });
                        self.last_click = Some((now, path));
                        if double {
                            return self.open_selected();
                        }
                    }
                }
                if let Some(row) = self
                    .sandbox_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .copied()
                {
                    self.focus = Focus::Sandboxes;
                    self.sandbox_selected = row.index;
                    let path = self
                        .selected_sandbox()
                        .map(|sandbox| sandbox.config_path.clone());
                    if let Some(path) = path {
                        let now = Instant::now();
                        let double = self.last_click.as_ref().is_some_and(|(at, p)| {
                            p == &path && now.duration_since(*at) < DOUBLE_CLICK
                        });
                        self.last_click = Some((now, path));
                        if double {
                            return self.attach_selected_sandbox();
                        }
                    }
                }
                if let Some(row) = self
                    .snapshot_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .copied()
                {
                    self.focus = Focus::Snapshots;
                    self.selected_snapshot = row.index;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Click inside the settings modal: steppers change a value, a row click
    /// selects and activates it (toggle, cycle, step or inline edit).
    fn click_config_row(&mut self, pos: (u16, u16)) -> Vec<Effect> {
        let tab = self
            .config_tabs
            .iter()
            .find(|hit| rect_contains(hit.rect, pos))
            .map(|hit| hit.page);
        if let Some(page) = tab
            && let Some(Modal::Config(form)) = self.modal.as_mut()
        {
            form.set_page(page);
            return Vec::new();
        }
        let stepper = self
            .config_steppers
            .iter()
            .find(|hit| rect_contains(hit.rect, pos))
            .copied();
        let row = self
            .config_rows
            .iter()
            .find(|row| rect_contains(row.rect, pos))
            .copied();
        let outcome = {
            let Some(Modal::Config(form)) = self.modal.as_mut() else {
                return Vec::new();
            };
            if let Some(stepper) = stepper {
                form.adjust_field(stepper.field, stepper.step);
                return Vec::new();
            }
            if let Some(row) = row
                && form.select_row(row.index)
            {
                form.click()
            } else {
                FormOutcome::Stay
            }
        };
        match outcome {
            FormOutcome::OpenProfile(harness) => {
                self.modal = None;
                vec![Effect::OpenProfileEditor(harness)]
            }
            _ => Vec::new(),
        }
    }

    /// Test/uploads access to the security engine.
    pub fn security_engine(&self) -> &SecurityEngine {
        &self.security_engine
    }

    pub fn set_security_mode(&mut self, mode: SecurityMode) {
        self.config.security = mode;
        self.security_engine = SecurityEngine::new(mode);
    }

    pub fn session_active(&mut self) -> bool {
        self.session
            .as_mut()
            .map(|session| !session.is_exited())
            .unwrap_or(false)
    }

    pub fn project_git(&self) -> Option<&GitSummary> {
        self.selected_project().map(|p| &p.git)
    }
}

fn clamp_index(current: usize, delta: isize, max: usize) -> usize {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(max)
    }
}

fn rect_contains(rect: Rect, pos: (u16, u16)) -> bool {
    pos.0 >= rect.x
        && pos.0 < rect.x.saturating_add(rect.width)
        && pos.1 >= rect.y
        && pos.1 < rect.y.saturating_add(rect.height)
}

/// Encode a key for raw passthrough mode.
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if c.is_ascii_alphabetic() {
                    vec![c.to_ascii_uppercase() as u8 - 0x40]
                } else {
                    match c {
                        ' ' | '@' => vec![0x00],
                        '[' => vec![0x1b],
                        '\\' => vec![0x1c],
                        ']' => vec![0x1d],
                        '^' => vec![0x1e],
                        '_' => vec![0x1f],
                        _ => return None,
                    }
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                let mut bytes = vec![0x1b];
                bytes.extend(c.to_string().into_bytes());
                bytes
            } else {
                c.to_string().into_bytes()
            }
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => return None,
        },
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::TempDir;

    fn test_app() -> (TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.projects_root = dir.path().join("projects");
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.templates_dir = dir.path().join("templates");
        config.sandboxes_dir = dir.path().join("sandboxes");
        config.profiles_dir = dir.path().join("profiles");
        config.backend = crate::config::BackendKind::Auto;
        config.auto_branch = false;
        std::fs::create_dir_all(&config.projects_root).unwrap();
        std::fs::create_dir_all(&config.snapshots_dir).unwrap();
        let app = App::new(config);
        (dir, app)
    }

    struct FakeConsole {
        project: PathBuf,
        output: Vec<Vec<u8>>,
        sent: Vec<u8>,
        exited: bool,
    }

    impl FakeConsole {
        fn new(project: impl Into<PathBuf>) -> Self {
            Self {
                project: project.into(),
                output: Vec::new(),
                sent: Vec::new(),
                exited: false,
            }
        }
    }

    impl TerminalConsole for FakeConsole {
        fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
            self.sent.extend_from_slice(bytes);
            Ok(())
        }

        fn drain(&mut self) -> Vec<Vec<u8>> {
            std::mem::take(&mut self.output)
        }

        fn is_exited(&mut self) -> bool {
            self.exited
        }

        fn project(&self) -> &std::path::Path {
            &self.project
        }

        fn label(&self) -> &str {
            "fake · serial"
        }

        fn sandbox_name(&self) -> &str {
            "fake"
        }
    }

    fn fake_project(name: &str) -> Project {
        Project {
            name: name.into(),
            path: PathBuf::from(format!("/projects/{name}")),
            git: GitSummary::default(),
            last_opened: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn starts_loading_with_no_projects() {
        let (_dir, app) = test_app();
        assert!(app.loading);
        assert!(app.visible.is_empty());
    }

    #[test]
    fn set_projects_applies_recency_and_filters() {
        let (_dir, mut app) = test_app();
        let mut a = fake_project("alpha");
        a.path = app.config.projects_root.join("alpha");
        let mut b = fake_project("hegn-tui");
        b.path = app.config.projects_root.join("hegn-tui");
        app.store.touch(&b.path).unwrap();
        app.set_projects(vec![a, b]);
        assert!(!app.loading);
        assert_eq!(app.visible.len(), 2);
        assert_eq!(app.selected_project().unwrap().name, "hegn-tui");

        app.query = "alpha".into();
        app.refresh_filter();
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.selected_project().unwrap().name, "alpha");
    }

    #[test]
    fn search_mode_edits_filter_and_escape_leaves_it() {
        let (_dir, mut app) = test_app();
        app.set_projects(vec![fake_project("alpha"), fake_project("beta")]);
        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.searching);
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.query, "b");
        assert_eq!(app.selected_project().unwrap().name, "beta");
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.searching);
    }

    #[test]
    fn quit_key_sets_flag() {
        let (_dir, mut app) = test_app();
        let effects = app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
        assert!(matches!(effects.as_slice(), [Effect::Quit]));
    }

    #[test]
    fn tab_toggles_focus() {
        let (_dir, mut app) = test_app();
        assert_eq!(app.focus, Focus::Projects);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Sandboxes);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Snapshots);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Projects);
    }

    #[test]
    fn number_keys_jump_to_the_matching_tab() {
        let (_dir, mut app) = test_app();
        assert_eq!(app.focus, Focus::Projects);
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.focus, Focus::Sandboxes);
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.focus, Focus::Snapshots);
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.focus, Focus::Projects);
    }

    #[test]
    fn enter_never_confirms_a_deletion() {
        let (_dir, mut app) = test_app();
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        app.focus = Focus::Sandboxes;
        let sandbox = fake_sandbox(&app, "demo", inside);
        install_sandboxes(&mut app, vec![sandbox]);
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            app.modal,
            Some(Modal::DeleteSandbox(crate::app::DeleteSandboxModal {
                stage: crate::app::DeleteSandboxStage::Confirm,
                ..
            }))
        ));

        app.handle_key(key(KeyCode::Char('y')));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(effects.is_empty(), "Enter must not delete files");
        assert!(matches!(
            app.modal,
            Some(Modal::DeleteSandbox(crate::app::DeleteSandboxModal {
                stage: crate::app::DeleteSandboxStage::Files,
                ..
            }))
        ));
    }

    #[test]
    fn enter_never_confirms_destructive_prompts() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Confirm(ConfirmPrompt {
            title: "Delete snapshot".into(),
            message: "Delete 'demo'?".into(),
            confirm_label: "Delete".into(),
            action: ConfirmAction::DeleteSnapshot("demo".into()),
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));

        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(app.modal, Some(Modal::Security(_))),
            "Enter must not allow a flagged command"
        );
    }

    #[test]
    fn create_project_modal_creates_directory() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('c')));
        assert!(matches!(app.modal, Some(Modal::CreateProject(_))));
        for c in "demo-app".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // Tab to the create button and submit.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.modal.is_none(), "{:?}", app.modal);
        let path = app.config.projects_root.join("demo-app");
        assert!(path.join("AGENTS.md").is_file());
        assert!(app.projects.iter().any(|p| p.path == path));
        let meta = project::read_meta(&path);
        assert_eq!(meta.harness, Some(Harness::Shell));
        assert!(meta.template.is_none());
    }

    #[test]
    fn create_project_error_keeps_modal_open() {
        let (_dir, mut app) = test_app();
        app.config.projects_root = PathBuf::from("/definitely/missing/root");
        app.handle_key(key(KeyCode::Char('c')));
        app.handle_key(key(KeyCode::Char('x')));
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::CreateProject(form)) => assert!(form.error.is_some()),
            other => panic!("expected form, got {other:?}"),
        }
    }

    #[test]
    fn security_prompt_deny_closes_modal() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.modal.is_none());
    }

    #[test]
    fn security_allow_without_session_warns() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.modal.is_none());
        let toast = app.toast.as_ref().expect("a warning toast");
        assert!(
            toast.text.contains("no active session"),
            "approval must not hide the write failure: {}",
            toast.text
        );
    }

    #[test]
    fn checkpoint_for_non_git_project_keeps_modal_with_error() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
            scroll: 0,
        }));
        app.handle_key(key(KeyCode::Char('c')));
        match &app.modal {
            Some(Modal::Security(prompt)) => assert!(prompt.error.is_some()),
            other => panic!("expected security modal, got {other:?}"),
        }
    }

    #[test]
    fn delete_snapshot_requires_confirmation() {
        let (_dir, mut app) = test_app();
        let paths = SnapshotStore::new(&app.config.snapshots_dir)
            .prepare("old")
            .unwrap();
        SnapshotStore::new(&app.config.snapshots_dir)
            .save_meta(&SnapshotMeta {
                name: "old".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: paths.dir,
                sandbox: None,
                project: None,
                size_bytes: 0,
                note: None,
            })
            .unwrap();
        app.refresh_snapshots();
        app.focus = Focus::Snapshots;
        app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(app.snapshots.is_empty());
        assert!(!app.config.snapshots_dir.join("old").exists());
    }

    #[test]
    fn footer_confirm_action_deletes_instead_of_opening_project() {
        let (_dir, mut app) = test_app();
        let paths = SnapshotStore::new(&app.config.snapshots_dir)
            .prepare("old")
            .unwrap();
        SnapshotStore::new(&app.config.snapshots_dir)
            .save_meta(&SnapshotMeta {
                name: "old".into(),
                created_at: chrono::Utc::now(),
                vm_state: "Paused".into(),
                dir: paths.dir,
                sandbox: None,
                project: None,
                size_bytes: 0,
                note: None,
            })
            .unwrap();
        app.refresh_snapshots();
        app.focus = Focus::Snapshots;
        app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));
        app.handle_action(ActionId::Confirm);
        assert!(app.modal.is_none());
        assert!(app.snapshots.is_empty());
        assert!(!app.config.snapshots_dir.join("old").exists());
    }

    #[test]
    fn footer_buttons_track_mouse_hover() {
        let (_dir, mut app) = test_app();
        app.footer_hits = vec![FooterHit {
            rect: Rect::new(4, 10, 8, 1),
            action: ActionId::Open,
        }];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 6,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.hovered, Some(ActionId::Open));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 40,
            row: 20,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.hovered, None);

        app.hovered = Some(ActionId::Open);
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.hovered, None, "keyboard input clears the hover state");
    }

    #[test]
    fn footer_create_action_submits_without_discarding_input() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('c')));
        for c in "from-footer".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_action(ActionId::SubmitCreate);
        assert!(app.modal.is_none(), "{:?}", app.modal);
        assert!(app.config.projects_root.join("from-footer").is_dir());
    }

    #[test]
    fn snapshot_focus_supports_documented_new_shortcut() {
        let (_dir, mut app) = test_app();
        app.focus = Focus::Snapshots;
        app.handle_key(key(KeyCode::Char('n')));
        let toast = app.toast.as_ref().unwrap();
        assert!(
            toast.text.contains("hegn snapshot create"),
            "{}",
            toast.text
        );
    }

    #[test]
    fn terminal_scroll_footer_action_scrolls_instead_of_opening() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Terminal;
        app.terminal.push_bytes(b"one\ntwo\n");
        app.handle_action(ActionId::ScrollUp);
        assert!(app.scroll > 0);
        assert_eq!(app.screen, Screen::Terminal);
    }

    #[test]
    fn terminal_input_collects_and_clears_on_empty_submit() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Terminal;
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.input, "ls");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "l");
        app.input.clear();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.input.is_empty());
    }

    #[test]
    fn benign_submit_without_session_warns_instead_of_crashing() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Terminal;
        app.handle_key(key(KeyCode::Char('i')));
        for c in "ls".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(app.toast.is_some());
    }

    #[test]
    fn destructive_submit_opens_security_modal() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Terminal;
        app.handle_key(key(KeyCode::Char('i')));
        for c in "rm -rf /workspace".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::Security(prompt)) => {
                assert_eq!(prompt.command, "rm -rf /workspace");
                assert!(prompt.inspection.destructive);
            }
            other => panic!("expected security modal, got {other:?}"),
        }
    }

    #[test]
    fn raw_mode_toggles_and_hides_interception() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Terminal;
        app.handle_key(key(KeyCode::F(2)));
        assert_eq!(app.terminal_mode, TerminalMode::Raw);
        app.handle_key(key(KeyCode::F(2)));
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
    }

    #[test]
    fn terminal_starts_in_normal_mode_and_i_enters_insert() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Terminal;
        // Normal mode swallows printable keys instead of forwarding them.
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.input.is_empty());
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.terminal_mode, TerminalMode::Insert);
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "x");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.terminal_mode, TerminalMode::Normal);
        assert_eq!(app.screen, Screen::Terminal);
        // Esc in normal mode leaves the terminal.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn insert_mode_esc_does_not_leave_the_terminal() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Terminal;
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.terminal_mode, TerminalMode::Insert);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Terminal);
    }

    #[test]
    fn left_arrow_leaves_the_project_terminal_in_normal_mode() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        assert_eq!(app.screen, Screen::Terminal);

        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn left_arrow_in_insert_or_raw_mode_does_not_leave_the_terminal() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));

        app.terminal_mode = TerminalMode::Insert;
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Terminal);

        app.terminal_mode = TerminalMode::Raw;
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.screen, Screen::Terminal);
    }

    #[test]
    fn multi_agent_is_reachable_from_the_project_terminal() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        assert_eq!(app.screen, Screen::Terminal);
        let effects = app.handle_key(key(KeyCode::Char('M')));
        assert!(effects.is_empty());
        match &app.modal {
            Some(Modal::MultiAgent(form)) => assert_eq!(form.source, "fake"),
            other => panic!("expected multi-agent form, got {other:?}"),
        }
    }

    #[test]
    fn footer_help_action_opens_modal() {
        let (_dir, mut app) = test_app();
        app.handle_action(ActionId::Help);
        assert!(matches!(app.modal, Some(Modal::Help(_))));
        app.handle_action(ActionId::HelpSearch);
        assert!(matches!(&app.modal, Some(Modal::Help(help)) if help.searching));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(&app.modal, Some(Modal::Help(help)) if !help.searching));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn help_search_matches_all_terms_case_insensitively() {
        let mut help = HelpState::default();
        assert!(help.matches("Projects", "c", "create project"));

        help.query = "CHECKPOINT".into();
        assert!(help.matches("Security", "c", "create git checkpoint, then allow"));
        assert!(!help.matches("Snapshots", "Enter", "restore selected snapshot"));

        help.query = "stop snapshot".into();
        assert!(
            !help.matches("Sandboxes", "S", "stop selected sandbox"),
            "every query term must match"
        );

        help.query = "terminal".into();
        assert!(
            help.matches("Terminal", "F2", "toggle raw passthrough"),
            "the section title is searchable too"
        );
    }

    #[test]
    fn help_search_mode_captures_typing_and_clears_in_steps() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Help(HelpState::default()));
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(&app.modal, Some(Modal::Help(help)) if help.searching));

        for c in "sq".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let Some(Modal::Help(help)) = &app.modal else {
            panic!("typing must not close the help modal");
        };
        assert_eq!(help.query, "sq");

        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Esc));
        let Some(Modal::Help(help)) = &app.modal else {
            panic!("the first Esc stops searching");
        };
        assert!(!help.searching);
        assert_eq!(help.query, "s");

        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(&app.modal, Some(Modal::Help(help)) if help.query.is_empty()));

        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn mouse_scroll_moves_selection() {
        let (_dir, mut app) = test_app();
        app.set_projects(vec![fake_project("a"), fake_project("b")]);
        let event = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(event);
        assert_eq!(app.selected, 1);
    }

    fn help_scroll(app: &mut App) -> usize {
        match &app.modal {
            Some(Modal::Help(help)) => help.scroll,
            other => panic!("expected the help modal, got {other:?}"),
        }
    }

    #[test]
    fn help_scrolls_with_keys_and_clamps_to_the_track() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Help(HelpState::default()));
        app.modal_scroll = Some(ScrollHit {
            rect: Rect::new(10, 2, 1, 10),
            max: 6,
            page: 10,
            offset: 0,
        });

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(help_scroll(&mut app), 1);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(help_scroll(&mut app), 6, "clamped to the track maximum");
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(help_scroll(&mut app), 0);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(help_scroll(&mut app), 0, "cannot scroll above the top");
    }

    #[test]
    fn modal_scrollbar_hovers_and_drags() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Help(HelpState::default()));
        app.modal_scroll = Some(ScrollHit {
            rect: Rect::new(40, 2, 1, 11),
            max: 10,
            page: 5,
            offset: 0,
        });
        let event = |kind, row| MouseEvent {
            kind,
            column: 40,
            row,
            modifiers: KeyModifiers::NONE,
        };

        app.handle_mouse(event(MouseEventKind::Moved, 4));
        assert!(app.scroll_hover, "the track highlights under the pointer");

        app.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), 7));
        assert!(app.scroll_dragging);
        assert_eq!(help_scroll(&mut app), 5, "mid-track maps to mid-offset");

        app.handle_mouse(event(MouseEventKind::Drag(MouseButton::Left), 2));
        assert_eq!(help_scroll(&mut app), 0);

        app.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 2));
        assert!(!app.scroll_dragging);
    }

    #[test]
    fn encode_key_supports_ctrl_and_arrows() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(ctrl_c), Some(vec![0x03]));
        assert_eq!(encode_key(key(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(2))), Some(b"\x1bOQ".to_vec()));
        let alt_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(encode_key(alt_b), Some(vec![0x1b, b'b']));
        // A non-ASCII char must not be truncated into an ASCII control code.
        let ctrl_non_ascii = KeyEvent::new(KeyCode::Char('Ń'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(ctrl_non_ascii), None);
        let ctrl_bracket = KeyEvent::new(KeyCode::Char('['), KeyModifiers::CONTROL);
        assert_eq!(encode_key(ctrl_bracket), Some(vec![0x1b]));
    }

    fn fake_sandbox(app: &App, name: &str, workspace: PathBuf) -> SandboxSummary {
        SandboxSummary {
            name: name.into(),
            project: app.config.projects_root.join("demo"),
            workspace,
            harness: Harness::Shell,
            ephemeral: false,
            source_sandbox: None,
            state: "stopped".into(),
            vm_pid: None,
            serial_log: None,
            config_path: app.config.sandboxes_dir.join(name).join("hegn.toml"),
            vcpus: 2,
            memory_mib: 2048,
            error: None,
        }
    }

    /// Install a sandbox list and select the matching fake project, so the
    /// project-scoped sandbox filter has something to work with.
    fn install_sandboxes(app: &mut App, sandboxes: Vec<SandboxSummary>) {
        if let Some(project) = sandboxes.first().map(|sandbox| sandbox.project.clone()) {
            let name = project
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| project.display().to_string());
            app.projects = vec![Project {
                name,
                path: project,
                git: GitSummary::default(),
                last_opened: None,
            }];
            app.visible = vec![0];
            app.selected = 0;
            app.sandbox_project = None;
        }
        app.sandboxes = sandboxes;
        app.refresh_sandbox_filter();
    }

    #[test]
    fn sandbox_focus_and_delete_double_confirmation() {
        let (_dir, mut app) = test_app();
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.focus, Focus::Sandboxes);
        let sandbox = fake_sandbox(&app, "demo", inside);
        install_sandboxes(&mut app, vec![sandbox]);

        app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(
            app.modal,
            Some(Modal::DeleteSandbox(crate::app::DeleteSandboxModal {
                stage: crate::app::DeleteSandboxStage::Confirm,
                ..
            }))
        ));
        // First confirmation moves to the file question.
        app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            app.modal,
            Some(Modal::DeleteSandbox(crate::app::DeleteSandboxModal {
                stage: crate::app::DeleteSandboxStage::Files,
                ..
            }))
        ));
        let effects = app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::DeleteSandbox {
                sandbox,
                delete_files: true
            }] if sandbox == "demo"
        ));
    }

    #[test]
    fn projects_focus_delete_targets_the_project_sandbox() {
        let (_dir, mut app) = test_app();
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        let sandbox = fake_sandbox(&app, "demo", inside);
        install_sandboxes(&mut app, vec![sandbox]);
        app.focus = Focus::Projects;

        app.handle_key(key(KeyCode::Char('d')));
        let Some(Modal::DeleteSandbox(modal)) = &app.modal else {
            panic!("expected a delete confirmation");
        };
        assert_eq!(modal.sandbox.name, "demo");
        assert_eq!(modal.stage, DeleteSandboxStage::Confirm);
    }

    #[test]
    fn project_delete_without_a_vm_only_warns() {
        let (_dir, mut app) = test_app();
        app.projects = vec![Project {
            name: "empty".into(),
            path: app.config.projects_root.join("empty"),
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.sandboxes.clear();
        app.focus = Focus::Projects;

        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.modal.is_none());
    }

    #[test]
    fn keeping_files_emits_a_keep_effect() {
        let (_dir, mut app) = test_app();
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        app.focus = Focus::Sandboxes;
        let sandbox = fake_sandbox(&app, "demo", inside);
        install_sandboxes(&mut app, vec![sandbox]);
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('y')));
        let effects = app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::DeleteSandbox {
                sandbox,
                delete_files: false
            }] if sandbox == "demo"
        ));
    }

    #[test]
    fn config_menus_open_for_global_and_sandbox() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::F(4)));
        match &app.modal {
            Some(Modal::Config(form)) => {
                assert_eq!(form.kind, ConfigKind::Global);
                assert_eq!(form.value(ConfigField::Harness), "shell");
                assert!(form.selected_field().is_some());
            }
            other => panic!("expected global settings, got {other:?}"),
        }
        app.modal = None;
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        app.focus = Focus::Sandboxes;
        let sandbox = fake_sandbox(&app, "demo", inside.clone());
        let stored = crate::sandbox_config::SandboxConfig {
            name: "demo".into(),
            project: sandbox.project.clone(),
            workspace: inside,
            harness: Harness::Shell,
            ..crate::sandbox_config::SandboxConfig::default()
        };
        crate::sandbox_config::SandboxStore::new(app.config.clone())
            .save(&stored)
            .unwrap();
        install_sandboxes(&mut app, vec![sandbox]);
        app.handle_key(key(KeyCode::F(5)));
        match &app.modal {
            Some(Modal::Config(form)) => assert_eq!(form.kind, ConfigKind::Sandbox),
            other => panic!("expected sandbox settings, got {other:?}"),
        }
    }

    #[test]
    fn config_profiles_tab_opens_the_profile_editor() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::F(4)));
        app.handle_key(key(KeyCode::Char(']')));
        match &app.modal {
            Some(Modal::Config(form)) => {
                assert_eq!(form.page(), ConfigPage::Profiles);
                assert_eq!(
                    form.selected_field(),
                    Some(ConfigField::Profile(Harness::Opencode))
                );
            }
            other => panic!("expected settings modal, got {other:?}"),
        }
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::OpenProfileEditor(Harness::Opencode)]
        ));
    }

    #[test]
    fn config_modal_changes_values_with_keys_and_mouse() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::F(4)));
        let harness_row = match &app.modal {
            Some(Modal::Config(form)) => form.selected_row(),
            _ => panic!("expected settings modal"),
        };
        // The renderer has not run in this test, so install a synthetic row
        // rectangle and click it.
        app.config_rows = vec![RowHit {
            rect: Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 1,
            },
            index: harness_row,
        }];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        match &app.modal {
            Some(Modal::Config(form)) => {
                assert_eq!(form.value(ConfigField::Harness), "OpenCode");
            }
            other => panic!("expected settings modal, got {other:?}"),
        }

        // Esc guards unsaved edits: the first press warns, the second discards.
        let effects = app.handle_key(key(KeyCode::Esc));
        assert!(effects.is_empty());
        assert!(app.modal.is_some());
        let effects = app.handle_key(key(KeyCode::Esc));
        assert!(effects.is_empty());
        assert!(app.modal.is_none());
        assert_eq!(app.config.harness, Harness::Shell);
    }

    #[test]
    fn multi_agent_form_submits_clone_effect() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        app.handle_key(key(KeyCode::Char('M')));
        assert!(matches!(app.modal, Some(Modal::MultiAgent(_))));
        let effects = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::CloneSandbox {
                count: 2,
                start: true,
                ..
            }]
        ));
    }

    #[test]
    fn multi_agent_hint_actions_drive_the_form() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        app.handle_key(key(KeyCode::Char('M')));
        app.handle_action(ActionId::MultiAgentMore);
        match &app.modal {
            Some(Modal::MultiAgent(form)) => assert_eq!(form.count, 3),
            other => panic!("expected multi-agent modal, got {other:?}"),
        }
        app.handle_action(ActionId::ToggleCloneStart);
        match &app.modal {
            Some(Modal::MultiAgent(form)) => assert!(!form.start),
            other => panic!("expected multi-agent modal, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_list_is_scoped_to_the_selected_project_and_includes_clones() {
        let (_dir, mut app) = test_app();
        let demo = app.config.projects_root.join("demo");
        let other = app.config.projects_root.join("other");

        let mut base = fake_sandbox(&app, "demo", demo.join("workspace"));
        base.project = demo.clone();
        let mut clone = fake_sandbox(&app, "demo-1", demo.join("clone-1"));
        clone.project = demo.clone();
        clone.ephemeral = true;
        clone.source_sandbox = Some("demo".into());
        let mut other_sandbox = fake_sandbox(&app, "other", other.join("workspace"));
        other_sandbox.project = other.clone();

        let demo_project = Project {
            name: "demo".into(),
            path: demo.clone(),
            git: GitSummary::default(),
            last_opened: None,
        };
        let other_project = Project {
            name: "other".into(),
            path: other.clone(),
            ..demo_project.clone()
        };
        app.set_projects(vec![demo_project, other_project]);
        app.sandboxes = vec![base, clone, other_sandbox];

        app.selected = app
            .visible
            .iter()
            .position(|&index| app.projects[index].path == demo)
            .unwrap();
        app.refresh_sandbox_filter();
        assert_eq!(app.sandbox_visible.len(), 2);
        let names: Vec<&str> = app
            .sandbox_visible
            .iter()
            .map(|&index| app.sandboxes[index].name.as_str())
            .collect();
        assert_eq!(names, ["demo", "demo-1"]);
        assert!(
            app.sandbox_visible
                .iter()
                .any(|&index| app.sandboxes[index].ephemeral),
            "multi-agent clones must be attachable from the list"
        );

        app.selected = app
            .visible
            .iter()
            .position(|&index| app.projects[index].path == other)
            .unwrap();
        app.refresh_sandbox_filter();
        assert_eq!(app.sandbox_visible.len(), 1);
        assert_eq!(app.selected_sandbox().unwrap().name, "other");
    }

    #[test]
    fn stop_shortcut_only_works_with_sandbox_focus() {
        let (_dir, mut app) = test_app();
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        let sandbox = fake_sandbox(&app, "demo", inside);
        install_sandboxes(&mut app, vec![sandbox]);
        app.focus = Focus::Projects;
        let effects = app.handle_key(key(KeyCode::Char('S')));
        assert!(
            effects.is_empty(),
            "S must not stop a sandbox from projects"
        );
        app.focus = Focus::Sandboxes;
        let effects = app.handle_key(key(KeyCode::Char('S')));
        assert!(matches!(
            effects.as_slice(),
            [Effect::StopSandbox { sandbox }] if sandbox == "demo"
        ));
    }

    #[test]
    fn refresh_does_not_clear_the_starting_state() {
        let (_dir, mut app) = test_app();
        app.sandbox.state = SandboxState::Starting;
        app.refresh_sandboxes();
        assert_eq!(app.sandbox.state, SandboxState::Starting);
    }

    #[test]
    fn sandbox_status_reflects_config() {
        let (_dir, app) = test_app();
        assert_eq!(app.sandbox.state, SandboxState::Stopped);
        assert_eq!(app.sandbox.backend, "cloud-hypervisor");
        assert_eq!(app.sandbox.vcpus, app.config.vcpus);
    }

    #[test]
    fn missing_vm_never_falls_back_to_host_work() {
        let (_dir, mut app) = test_app();
        app.fail_start("missing VM assets");
        assert!(app.session.is_none());
        assert_eq!(app.sandbox.state, SandboxState::Failed);
        assert!(!app.session_active());
        assert!(app.toast.as_ref().unwrap().text.contains("VM start failed"));
    }

    #[test]
    fn open_requests_a_sandbox_and_resets_the_terminal_mode() {
        let (_dir, mut app) = test_app();
        let mut project = fake_project("demo");
        project.path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project.path).unwrap();
        app.set_projects(vec![project.clone()]);
        app.terminal_mode = TerminalMode::Raw;
        let effects = app.open_selected();
        assert_eq!(
            app.terminal_mode,
            TerminalMode::Normal,
            "the terminal mode must reset for each VM"
        );
        assert_eq!(app.sandbox.state, SandboxState::Starting);
        assert!(app.session.is_none());
        match effects.as_slice() {
            [
                Effect::StartProjectSandbox {
                    project: p,
                    harness,
                },
            ] => {
                assert_eq!(p, &project.path);
                assert_eq!(*harness, app.config.harness);
            }
            other => panic!("expected StartProjectSandbox, got {other:?}"),
        }
    }

    #[test]
    fn a_second_start_is_rejected_while_the_first_is_booting() {
        let (_dir, mut app) = test_app();
        app.sandbox.state = SandboxState::Starting;
        let effects = app.open_selected();
        assert!(effects.is_empty());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("already starting"))
        );
    }

    #[test]
    fn h_and_left_open_the_import_modal_from_the_project_list() {
        let (_dir, mut app) = test_app();
        app.handle_key(key(KeyCode::Char('h')));
        assert!(matches!(app.modal, Some(Modal::ImportProject(_))));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());

        app.handle_key(key(KeyCode::Left));
        assert!(matches!(app.modal, Some(Modal::ImportProject(_))));
    }

    #[test]
    fn import_form_creates_a_symlink_and_adds_the_project() {
        let (dir, mut app) = test_app();
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();

        app.handle_key(key(KeyCode::Char('h')));
        for ch in source.to_string_lossy().chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(app.modal.is_none(), "successful import closes the modal");
        let link = app.config.projects_root.join("demo");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(app.projects.iter().any(|project| project.name == "demo"));
        let toast = app.toast.as_ref().expect("import toast");
        assert!(toast.text.contains("imported"), "{}", toast.text);
    }

    #[test]
    fn importing_hegn_state_is_rejected_in_the_form() {
        let (_dir, mut app) = test_app();
        std::fs::create_dir_all(&app.config.state_dir).unwrap();
        let state_path = app.config.state_dir.to_string_lossy().into_owned();
        app.handle_key(key(KeyCode::Char('h')));
        for ch in state_path.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::ImportProject(form)) => {
                let error = form.error.as_deref().unwrap_or_default();
                assert!(error.contains("overlaps hegn state"), "{error}");
            }
            other => panic!("expected import modal with an error, got {other:?}"),
        }
    }

    #[test]
    fn failures_open_the_doctor_modal_and_d_requests_a_download() {
        let (dir, mut app) = test_app();
        app.config.kernel_image = dir.path().join("missing-kernel");
        app.config.rootfs_image = dir.path().join("missing-rootfs");
        app.fail_start("missing VM assets");
        match &app.modal {
            Some(Modal::Doctor(modal)) => {
                assert!(!modal.report.ok);
                assert!(modal.report.can_download());
                assert!(
                    modal
                        .error
                        .as_deref()
                        .is_some_and(|e| e.contains("missing VM"))
                );
            }
            other => panic!("expected doctor modal, got {other:?}"),
        }
        let effects = app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(effects.as_slice(), [Effect::DownloadImages]));
        assert!(matches!(
            app.modal,
            Some(Modal::Doctor(DoctorModal {
                downloading: true,
                ..
            }))
        ));

        app.handle_key(key(KeyCode::Esc));
        app.handle_action(ActionId::Doctor);
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));
    }

    #[test]
    fn doctor_setup_requires_explicit_confirmation() {
        let (_dir, mut app) = test_app();
        app.config.kernel_image = app.config.projects_root.join("missing-kernel");
        app.config.rootfs_image = app.config.projects_root.join("missing-rootfs");
        app.fail_start("missing VM assets");
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));

        // The setup key opens a confirmation; nothing runs yet.
        let effects = app.handle_key(key(KeyCode::Char('s')));
        assert!(effects.is_empty());
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));

        // Enter must not confirm; `y` is explicit.
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));
        let effects = app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(effects.as_slice(), [Effect::SetupEnvironment]));
    }

    #[test]
    fn attaching_a_console_enters_the_terminal_view() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        assert_eq!(app.screen, Screen::Terminal);
        assert_eq!(app.sandbox.state, SandboxState::Running);
        assert!(app.session_active());
    }

    #[test]
    fn reopening_the_same_project_through_a_symlink_reuses_the_console() {
        let (dir, mut app) = test_app();
        let real = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("demo-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut project = fake_project("demo");
        project.path = link;
        app.set_projects(vec![project]);

        app.attach_console(Box::new(FakeConsole::new(&real)));
        let effects = app.open_selected();
        assert!(effects.is_empty(), "must not boot a second VM");
        assert_eq!(app.screen, Screen::Terminal);
    }

    #[test]
    fn fail_start_never_falls_back_to_a_host_shell() {
        let (_dir, mut app) = test_app();
        app.fail_start("boom");
        assert!(app.session.is_none());
        assert_eq!(app.sandbox.state, SandboxState::Failed);
        assert!(!app.session_active());
    }
}
