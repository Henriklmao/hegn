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
    OpenLazygit(PathBuf),
    RefreshProjects,
    RestoreSnapshot(SnapshotMeta),
    DownloadImages,
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
    DownloadImages,
    Allow,
    Checkpoint,
    Deny,
    MultiAgentMore,
    MultiAgentFewer,
    HarnessPrev,
    HarnessNext,
    ToggleCloneStart,
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

#[derive(Debug, Clone)]
pub struct PackageOption {
    pub name: &'static str,
    pub selected: bool,
}

pub const PACKAGE_CHOICES: [&str; 7] = [
    "git",
    "nodejs",
    "python3",
    "build-essential",
    "ripgrep",
    "fd-find",
    "curl",
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
            packages: PACKAGE_CHOICES
                .iter()
                .map(|name| PackageOption {
                    name,
                    selected: matches!(*name, "git" | "curl"),
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
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    DeleteSnapshot(String),
    RestoreSnapshot(Box<SnapshotMeta>),
}

#[derive(Debug, Clone)]
pub struct ConfirmPrompt {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone)]
pub struct DoctorModal {
    pub report: DoctorReport,
    pub downloading: bool,
    /// The failure that opened the modal, if any.
    pub error: Option<String>,
}

impl DoctorModal {
    pub fn new(report: DoctorReport) -> Self {
        Self {
            report,
            downloading: false,
            error: None,
        }
    }

    pub fn with_error(report: DoctorReport, error: impl Into<String>) -> Self {
        Self {
            report,
            downloading: false,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    Global,
    Sandbox,
}

#[derive(Debug, Clone)]
pub struct ConfigModal {
    pub title: String,
    pub kind: ConfigKind,
    pub path: PathBuf,
    pub content: String,
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
}

#[derive(Debug, Clone)]
pub struct MultiAgentForm {
    pub source: String,
    pub count: usize,
    pub harness: Harness,
    pub start: bool,
    pub error: Option<String>,
}

impl MultiAgentForm {
    pub fn new(source: String, harness: Harness) -> Self {
        Self {
            source,
            count: 2,
            harness,
            start: true,
            error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Modal {
    CreateProject(CreateProjectForm),
    ImportProject(ImportProjectForm),
    Doctor(DoctorModal),
    Security(SecurityPrompt),
    Confirm(ConfirmPrompt),
    Config(ConfigModal),
    DeleteSandbox(DeleteSandboxModal),
    MultiAgent(MultiAgentForm),
    Help,
    Error { title: String, message: String },
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
    pub sandbox_selected: usize,
    pub sandbox_rows: Vec<RowHit>,
    pub project_rows: Vec<RowHit>,
    pub session: Option<Box<dyn TerminalConsole>>,
    pub terminal: TerminalBuffer,
    pub input: String,
    pub terminal_mode: TerminalMode,
    pub scroll: usize,
    pub toast: Option<Toast>,
    pub footer_hits: Vec<FooterHit>,
    /// Footer button under the mouse pointer, for the inverted hover style.
    pub hovered: Option<ActionId>,
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
            sandbox_selected: 0,
            sandbox_rows: Vec::new(),
            project_rows: Vec::new(),
            session: None,
            terminal: TerminalBuffer::default(),
            input: String::new(),
            terminal_mode: TerminalMode::Normal,
            scroll: 0,
            toast: None,
            footer_hits: Vec::new(),
            hovered: None,
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
        self.sandboxes.get(self.sandbox_selected)
    }

    /// Refresh the sandbox list from disk/process state. Cheap enough for the
    /// UI tick; it only reads files and `/proc`.
    pub fn refresh_sandboxes(&mut self) {
        let selected = self.selected_sandbox().map(|sandbox| sandbox.name.clone());
        let registry = crate::sandbox::SandboxRegistry::new(self.config.clone());
        if let Ok(sandboxes) = registry.summaries() {
            self.sandboxes = sandboxes;
        }
        if let Some(name) = selected
            && let Some(index) = self
                .sandboxes
                .iter()
                .position(|sandbox| sandbox.name == name)
        {
            self.sandbox_selected = index;
        }
        self.sandbox_selected = self
            .sandbox_selected
            .min(self.sandboxes.len().saturating_sub(1));
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
            }
            Focus::Sandboxes => {
                if self.sandboxes.is_empty() {
                    return;
                }
                let max = self.sandboxes.len() - 1;
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
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
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
                self.modal = Some(Modal::Help);
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
                Focus::Projects => Vec::new(),
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
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
            Modal::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
                ) {
                    // closed
                } else {
                    self.modal = Some(Modal::Help);
                }
                Vec::new()
            }
            Modal::Error { .. } => Vec::new(),
            Modal::Confirm(prompt) => self.handle_confirm_key(key, prompt),
            Modal::Config(modal) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Vec::new(),
                KeyCode::Char('e') | KeyCode::Enter => vec![Effect::OpenEditor(modal.path)],
                KeyCode::Char('r') => match modal.kind {
                    ConfigKind::Global => self.open_global_config(),
                    ConfigKind::Sandbox => self.open_sandbox_config(),
                },
                _ => {
                    self.modal = Some(Modal::Config(modal));
                    Vec::new()
                }
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
                    self.modal = Some(Modal::Doctor(modal));
                    Vec::new()
                }
                KeyCode::Char('d') if modal.report.can_download() && !modal.downloading => {
                    modal.downloading = true;
                    self.modal = Some(Modal::Doctor(modal));
                    vec![Effect::DownloadImages]
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
        let path = Config::config_path();
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| toml::to_string_pretty(&self.config).unwrap_or_default());
        self.modal = Some(Modal::Config(ConfigModal {
            title: "global config".into(),
            kind: ConfigKind::Global,
            path,
            content,
        }));
        Vec::new()
    }

    fn open_sandbox_config(&mut self) -> Vec<Effect> {
        let Some(sandbox) = self.selected_sandbox().cloned() else {
            self.toast("select a sandbox first", ToastLevel::Warn);
            return Vec::new();
        };
        let store = crate::sandbox_config::SandboxStore::new(self.config.clone());
        let content = store
            .load(&sandbox.name)
            .and_then(|config| toml::to_string_pretty(&config).map_err(Into::into))
            .unwrap_or_else(|err| format!("# failed to load config: {err}\n"));
        self.modal = Some(Modal::Config(ConfigModal {
            title: format!("sandbox {} config", sandbox.name),
            kind: ConfigKind::Sandbox,
            path: sandbox.config_path,
            content,
        }));
        Vec::new()
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
            Ok(config) => {
                self.security_engine = SecurityEngine::new(config.security);
                self.config = config;
            }
            Err(err) => self.toast(format!("config reload failed: {err}"), ToastLevel::Error),
        }
        self.refresh_sandboxes();
        self.refresh_snapshots();
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
            .sandboxes
            .iter()
            .position(|sandbox| sandbox.name == console.sandbox_name())
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
                self.modal = Some(Modal::Help);
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
                Some(Modal::Config(modal)) => {
                    self.modal = None;
                    vec![Effect::OpenEditor(modal.path)]
                }
                Some(other) => {
                    self.modal = Some(other);
                    Vec::new()
                }
                None => Vec::new(),
            },
            ActionId::ReloadConfig => {
                let kind = match &self.modal {
                    Some(Modal::Config(modal)) => Some(modal.kind),
                    _ => None,
                };
                match kind {
                    Some(ConfigKind::Global) => self.open_global_config(),
                    Some(ConfigKind::Sandbox) => self.open_sandbox_config(),
                    None => Vec::new(),
                }
            }
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
                Vec::new()
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
                Vec::new()
            }
            MouseEventKind::ScrollUp => {
                if self.screen == Screen::Terminal {
                    self.scroll = self.scroll.saturating_add(3).min(self.terminal.len());
                } else {
                    self.select_next(-1);
                }
                Vec::new()
            }
            MouseEventKind::ScrollDown => {
                if self.screen == Screen::Terminal {
                    self.scroll = self.scroll.saturating_sub(3);
                } else {
                    self.select_next(1);
                }
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
                if let Some(row) = self
                    .project_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .copied()
                {
                    self.focus = Focus::Projects;
                    self.selected = row.index;
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
        app.sandboxes = vec![fake_sandbox(&app, "demo", inside)];
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
        }));
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));

        app.modal = Some(Modal::Security(SecurityPrompt {
            command: "rm -rf /workspace".into(),
            project: PathBuf::from("/tmp"),
            inspection: app.security_engine().inspect("rm -rf /workspace", None),
            error: None,
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
        assert!(matches!(app.modal, Some(Modal::Help)));
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

    #[test]
    fn sandbox_focus_and_delete_double_confirmation() {
        let (_dir, mut app) = test_app();
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.focus, Focus::Sandboxes);
        app.sandboxes = vec![fake_sandbox(&app, "demo", inside)];

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
    fn keeping_files_emits_a_keep_effect() {
        let (_dir, mut app) = test_app();
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        app.focus = Focus::Sandboxes;
        app.sandboxes = vec![fake_sandbox(&app, "demo", inside)];
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
            Some(Modal::Config(modal)) => {
                assert_eq!(modal.kind, crate::app::ConfigKind::Global);
                assert!(modal.content.contains("cloud_hypervisor_bin"));
            }
            other => panic!("expected global config, got {other:?}"),
        }
        app.modal = None;
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        app.focus = Focus::Sandboxes;
        app.sandboxes = vec![fake_sandbox(&app, "demo", inside)];
        app.handle_key(key(KeyCode::F(5)));
        assert!(matches!(
            app.modal,
            Some(Modal::Config(crate::app::ConfigModal {
                kind: crate::app::ConfigKind::Sandbox,
                ..
            }))
        ));
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
    fn stop_shortcut_only_works_with_sandbox_focus() {
        let (_dir, mut app) = test_app();
        let inside = app.config.sandboxes_dir.join("demo").join("workspace");
        app.sandboxes = vec![fake_sandbox(&app, "demo", inside)];
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
