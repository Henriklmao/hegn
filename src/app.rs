//! TUI application state and event handling.
//!
//! All state transitions live here and are covered by unit tests; [`crate::ui`]
//! is a pure renderer and [`crate::tui`] owns the terminal loop. This separation
//! makes the dashboard testable with ratatui's `TestBackend`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::config::{Config, Harness, SecurityMode};
use crate::console::TerminalConsole;
use crate::project::{self, GitSummary, Project, ProjectMeta};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Projects,
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
    /// Boot (or attach to) a Firecracker VM. A failure is a hard error; there is
    /// no host-process fallback.
    StartSandbox {
        project: PathBuf,
        harness: Harness,
    },
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
        let backend = "firecracker";
        let virtiofs = if config.firecracker_ready() {
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
    Confirm,
    Cancel,
    NextField,
    SubmitCreate,
    Edit,
    Lazygit,
    Refresh,
    Help,
    Quit,
    Back,
    ToggleRaw,
    ScrollUp,
    NewSnapshot,
    DeleteSnapshot,
    Restore,
    Allow,
    Checkpoint,
    Deny,
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
pub enum Modal {
    CreateProject(CreateProjectForm),
    Security(SecurityPrompt),
    Confirm(ConfirmPrompt),
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
    pub project_rows: Vec<RowHit>,
    pub session: Option<Box<dyn TerminalConsole>>,
    pub terminal: TerminalBuffer,
    pub input: String,
    pub raw_mode: bool,
    pub scroll: usize,
    pub toast: Option<Toast>,
    pub footer_hits: Vec<FooterHit>,
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
            project_rows: Vec::new(),
            session: None,
            terminal: TerminalBuffer::default(),
            input: String::new(),
            raw_mode: false,
            scroll: 0,
            toast: None,
            footer_hits: Vec::new(),
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
            .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
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

    pub fn focus_next(&mut self) {
        self.focus = match self.focus {
            Focus::Projects => Focus::Snapshots,
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
            KeyCode::Char('c') => {
                self.modal = Some(Modal::CreateProject(CreateProjectForm::new(
                    self.config.harness,
                )));
                Vec::new()
            }
            KeyCode::Char('e') => self.effect_for_selected(Effect::OpenEditor),
            KeyCode::Char('g') => self.effect_for_selected(Effect::OpenLazygit),
            KeyCode::Char('r') => vec![Effect::RefreshProjects],
            KeyCode::Char('d') => {
                if self.focus == Focus::Snapshots {
                    self.confirm_delete_snapshot()
                } else {
                    Vec::new()
                }
            }
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
            self.raw_mode = !self.raw_mode;
            self.toast(
                if self.raw_mode {
                    "raw passthrough enabled — input interception off"
                } else {
                    "intercepted input enabled"
                },
                if self.raw_mode {
                    ToastLevel::Warn
                } else {
                    ToastLevel::Info
                },
            );
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            self.screen = Screen::Dashboard;
            return Vec::new();
        }

        if self.raw_mode {
            if key.code == KeyCode::Esc {
                self.screen = Screen::Dashboard;
                return Vec::new();
            }
            if let Some(bytes) = encode_key(key) {
                self.write_bytes(&bytes);
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Dashboard;
            }
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
        }
        Vec::new()
    }

    /// Inspect and submit the terminal input buffer.
    pub fn submit_input(&mut self) -> Vec<Effect> {
        let command = self.input.clone();
        if command.trim().is_empty() {
            self.input.clear();
            return Vec::new();
        }
        self.input.clear();

        if self.raw_mode {
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
            Modal::Security(prompt) => self.handle_security_key(key, prompt),
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

    fn handle_confirm_key(&mut self, key: KeyEvent, prompt: ConfirmPrompt) -> Vec<Effect> {
        let confirmed = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
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
            KeyCode::Char('a') | KeyCode::Enter => {
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

    /// Open the selected project in a Firecracker VM.
    ///
    /// This only *requests* the VM start; the terminal loop owns the runtime and
    /// performs the boot. A missing VM is a hard error, never a local shell.
    pub fn open_selected(&mut self) -> Vec<Effect> {
        if self.focus == Focus::Snapshots {
            return self.confirm_restore_snapshot();
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
        self.raw_mode = false;
        self.session_closed_reported = false;
        self.sandbox.state = SandboxState::Starting;
        self.sandbox.started_at = None;
        self.sandbox.note = Some(format!(
            "booting Firecracker VM for {}",
            project.display_path()
        ));
        vec![Effect::StartSandbox {
            project: project.path.clone(),
            harness,
        }]
    }

    /// Install a console for a successfully started/attached VM.
    pub fn attach_console(&mut self, console: Box<dyn TerminalConsole>) {
        self.sandbox.state = SandboxState::Running;
        self.sandbox.backend = "firecracker";
        self.sandbox.note = Some(format!(
            "{} in {}",
            console.label(),
            console.project().display()
        ));
        self.sandbox.started_at = Some(Instant::now());
        self.sandbox.socket = Some(self.config.run_dir().join("firecracker.sock"));
        self.session = Some(console);
        self.screen = Screen::Terminal;
        self.raw_mode = false;
        self.session_closed_reported = false;
        self.toast("attached to VM serial console", ToastLevel::Info);
    }

    /// Report a hard VM start/attach failure. No host fallback is attempted.
    pub fn fail_start(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.session = None;
        self.sandbox.state = SandboxState::Failed;
        self.sandbox.started_at = None;
        self.sandbox.note = Some(error.clone());
        self.toast(format!("VM start failed: {error}"), ToastLevel::Error);
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
            ActionId::Confirm => {
                if matches!(self.modal, Some(Modal::Confirm(_))) {
                    self.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::Cancel => {
                if matches!(self.modal, Some(Modal::Confirm(_))) {
                    self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                } else {
                    Vec::new()
                }
            }
            ActionId::NextField => {
                if matches!(self.modal, Some(Modal::CreateProject(_))) {
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
            ActionId::Edit => self.effect_for_selected(Effect::OpenEditor),
            ActionId::Lazygit => self.effect_for_selected(Effect::OpenLazygit),
            ActionId::Refresh => vec![Effect::RefreshProjects],
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
            ActionId::ScrollUp => {
                if self.screen == Screen::Terminal {
                    self.scroll = self.scroll.saturating_add(5).min(self.terminal.len());
                }
                Vec::new()
            }
            ActionId::Restore => self.confirm_restore_snapshot(),
            ActionId::DeleteSnapshot => self.confirm_delete_snapshot(),
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
            ActionId::Close => {
                self.modal = None;
                Vec::new()
            }
        }
    }

    /// Mouse handling for footer clicks, row selection and scrolling.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Vec<Effect> {
        match mouse.kind {
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
        assert_eq!(app.focus, Focus::Snapshots);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Projects);
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
                snapshot_path: paths.snapshot_path,
                mem_file_path: paths.mem_file_path,
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
                snapshot_path: paths.snapshot_path,
                mem_file_path: paths.mem_file_path,
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
        assert!(app.raw_mode);
        app.handle_key(key(KeyCode::F(2)));
        assert!(!app.raw_mode);
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

    #[test]
    fn sandbox_status_reflects_config() {
        let (_dir, app) = test_app();
        assert_eq!(app.sandbox.state, SandboxState::Stopped);
        assert_eq!(app.sandbox.backend, "firecracker");
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
    fn open_requests_a_sandbox_and_resets_raw_mode() {
        let (_dir, mut app) = test_app();
        let mut project = fake_project("demo");
        project.path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project.path).unwrap();
        app.set_projects(vec![project.clone()]);
        app.raw_mode = true;
        let effects = app.open_selected();
        assert!(!app.raw_mode, "raw mode must reset for each VM");
        assert_eq!(app.sandbox.state, SandboxState::Starting);
        assert!(app.session.is_none());
        match effects.as_slice() {
            [
                Effect::StartSandbox {
                    project: p,
                    harness,
                },
            ] => {
                assert_eq!(p, &project.path);
                assert_eq!(*harness, app.config.harness);
            }
            other => panic!("expected StartSandbox, got {other:?}"),
        }
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
