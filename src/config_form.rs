//! Interactive settings editor behind the `F4`/`F5` configuration modal.
//!
//! The modal edits a working copy of the global `config.toml` or a sandbox's
//! `hegn.toml`. Nothing touches disk until the user saves, and every change is
//! validated against the same schema the CLI uses. Keyboard handling lives here
//! so navigation, editing and persistence are unit-testable without a terminal;
//! [`crate::ui`] only draws the rows and records their hit rectangles.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{
    BackendKind, Config, Harness, LocalEndpoint, NetworkMode, RootfsFormat, SecurityMode,
    safe_package_name, validate_local_endpoints,
};
use crate::sandbox_config::{CONFIG_FILE, SandboxConfig, SandboxStore};

/// Which configuration file the modal edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    Global,
    Sandbox,
}

/// Pages (tabs) inside the settings modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPage {
    General,
    LocalLlm,
}

impl ConfigPage {
    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
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
    SandboxesDir,
    TemplatesDir,
    ProfilesDir,
    ExtraPackages,
    /// Host-loopback LLM endpoints exposed through the guarded network.
    LocalEndpoints,
    // Sandbox-only, read-only context.
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
            Self::SandboxesDir => "Sandboxes dir",
            Self::TemplatesDir => "Templates dir",
            Self::ProfilesDir => "Profiles dir",
            Self::ExtraPackages => "Extra packages",
            Self::LocalEndpoints => "Endpoints (name=port)",
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
            Self::Project | Self::Workspace | Self::RootfsPath | Self::NetworkIndex => {
                FieldKind::Locked
            }
            _ => FieldKind::Text,
        }
    }

    fn is_path(self) -> bool {
        matches!(
            self,
            Self::KernelImage
                | Self::RootfsImage
                | Self::CloudHypervisorBin
                | Self::VirtiofsdBin
                | Self::ProjectsRoot
                | Self::StateDir
                | Self::SnapshotsDir
                | Self::SandboxesDir
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
    ConfigRow::Field(ConfigField::SandboxesDir),
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

const SANDBOX_ROWS: &[ConfigRow] = &[
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
    ConfigRow::Group("Sandbox (read-only)"),
    ConfigRow::Field(ConfigField::Project),
    ConfigRow::Field(ConfigField::Workspace),
    ConfigRow::Field(ConfigField::RootfsPath),
    ConfigRow::Field(ConfigField::NetworkIndex),
];

pub fn rows(kind: ConfigKind) -> &'static [ConfigRow] {
    rows_for(kind, ConfigPage::General)
}

/// Pages available for a file kind. Endpoints are host-wide, so a sandbox's
/// config has only the general page.
pub fn pages(kind: ConfigKind) -> &'static [ConfigPage] {
    match kind {
        ConfigKind::Global => &[ConfigPage::General, ConfigPage::LocalLlm],
        ConfigKind::Sandbox => &[ConfigPage::General],
    }
}

pub fn rows_for(kind: ConfigKind, page: ConfigPage) -> &'static [ConfigRow] {
    match (kind, page) {
        (ConfigKind::Global, ConfigPage::General) => GENERAL_ROWS,
        (ConfigKind::Global, ConfigPage::LocalLlm) => LOCAL_LLM_ROWS,
        (ConfigKind::Sandbox, _) => SANDBOX_ROWS,
    }
}

/// What the app should do after a key press in the modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormOutcome {
    /// The form consumed the key; keep the modal open.
    Stay,
    Close,
    Save,
    /// Reload the working copy from disk, discarding unsaved changes.
    Revert,
    /// Close the modal and open the raw TOML in `$EDITOR`.
    OpenFile,
}

/// A successfully persisted draft.
#[derive(Debug, Clone)]
pub enum Applied {
    Global(Box<Config>),
    Sandbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Draft {
    Global(Box<Config>),
    Sandbox(Box<SandboxConfig>),
}

#[derive(Debug, Clone)]
struct EditState {
    field: ConfigField,
    buffer: String,
}

/// Working state of the settings modal.
#[derive(Debug, Clone)]
pub struct ConfigForm {
    pub kind: ConfigKind,
    pub title: String,
    pub path: PathBuf,
    /// Set when the last save or inline edit failed.
    pub error: Option<String>,
    /// Transient confirmation such as "saved".
    pub status: Option<String>,
    draft: Draft,
    original: Draft,
    /// Global config the sandbox belongs to; used for derived paths and saving.
    base: Box<Config>,
    page: ConfigPage,
    selected: usize,
    edit: Option<EditState>,
    /// First `Esc` on a dirty draft only warns; the second one discards it.
    discard_armed: bool,
}

impl ConfigForm {
    /// Open the global settings editor.
    pub fn global(config: Config, path: PathBuf) -> Self {
        Self::build(
            ConfigKind::Global,
            "global settings".to_string(),
            path,
            Draft::Global(Box::new(config.clone())),
            config,
        )
    }

    /// Open the editor for an existing sandbox.
    pub fn sandbox(sandbox: SandboxConfig, base: Config) -> Self {
        let path = base.sandboxes_dir.join(&sandbox.name).join(CONFIG_FILE);
        let title = format!("sandbox '{}' settings", sandbox.name);
        Self::build(
            ConfigKind::Sandbox,
            title,
            path,
            Draft::Sandbox(Box::new(sandbox)),
            base,
        )
    }

    fn build(kind: ConfigKind, title: String, path: PathBuf, draft: Draft, base: Config) -> Self {
        let page = ConfigPage::General;
        let selected = first_field(kind, page);
        Self {
            kind,
            title,
            path,
            error: None,
            status: None,
            original: draft.clone(),
            draft,
            base: Box::new(base),
            page,
            selected,
            edit: None,
            discard_armed: false,
        }
    }

    pub fn rows(&self) -> &'static [ConfigRow] {
        rows_for(self.kind, self.page)
    }

    pub fn page(&self) -> ConfigPage {
        self.page
    }

    pub fn pages(&self) -> &'static [ConfigPage] {
        pages(self.kind)
    }

    /// Switch pages, committing an open inline edit first. Returns `false`
    /// when the edit is invalid and the page must stay.
    pub fn set_page(&mut self, page: ConfigPage) -> bool {
        if !self.pages().contains(&page) || page == self.page {
            return page == self.page;
        }
        if self.edit.is_some() && !self.commit_edit() {
            return false;
        }
        self.page = page;
        self.selected = first_field(self.kind, page);
        true
    }

    /// Cycle through the pages with `[`/`]`.
    pub fn cycle_page(&mut self, delta: isize) {
        let pages = self.pages();
        let Some(index) = pages.iter().position(|page| *page == self.page) else {
            return;
        };
        let next = (index as isize + delta).rem_euclid(pages.len() as isize) as usize;
        self.set_page(pages[next]);
    }

    pub fn selected_row(&self) -> usize {
        self.selected
    }

    pub fn selected_field(&self) -> Option<ConfigField> {
        self.field_at(self.selected)
    }

    pub fn field_at(&self, row: usize) -> Option<ConfigField> {
        match self.rows().get(row) {
            Some(ConfigRow::Field(field)) => Some(*field),
            _ => None,
        }
    }

    pub fn is_editing(&self) -> bool {
        self.edit.is_some()
    }

    /// The in-progress text for a row, including the cursor marker.
    pub fn editing_buffer(&self, field: ConfigField) -> Option<&str> {
        self.edit
            .as_ref()
            .filter(|edit| edit.field == field)
            .map(|edit| edit.buffer.as_str())
    }

    pub fn dirty(&self) -> bool {
        self.draft != self.original
    }

    pub fn sandbox_name(&self) -> Option<&str> {
        match &self.draft {
            Draft::Sandbox(sandbox) => Some(sandbox.name.as_str()),
            Draft::Global(_) => None,
        }
    }

    /// The current display value for a field.
    pub fn value(&self, field: ConfigField) -> String {
        match &self.draft {
            Draft::Global(config) => match field {
                ConfigField::Harness => config.harness.label().to_string(),
                ConfigField::Vcpus => config.vcpus.to_string(),
                ConfigField::Memory => config.memory_mib.to_string(),
                ConfigField::AutoBranch => on_off(config.auto_branch),
                ConfigField::RootfsFormat => config.rootfs_format.as_str().to_string(),
                ConfigField::RootDevice => config.root_device.clone(),
                ConfigField::CloudInit => on_off(config.cloud_init),
                ConfigField::KernelImage => config.kernel_image.display().to_string(),
                ConfigField::RootfsImage => config.rootfs_image.display().to_string(),
                ConfigField::SharedTag => config.shared_tag.clone(),
                ConfigField::GuestMount => config.guest_mount.clone(),
                ConfigField::Network => config.network.label().to_string(),
                ConfigField::Security => config.security.label().to_string(),
                ConfigField::Backend => config.backend.label().to_string(),
                ConfigField::CloudHypervisorBin => {
                    config.cloud_hypervisor_bin.display().to_string()
                }
                ConfigField::VirtiofsdBin => config.virtiofsd_bin.display().to_string(),
                ConfigField::ProjectsRoot => config.projects_root.display().to_string(),
                ConfigField::StateDir => config.state_dir.display().to_string(),
                ConfigField::SnapshotsDir => config.snapshots_dir.display().to_string(),
                ConfigField::SandboxesDir => config.sandboxes_dir.display().to_string(),
                ConfigField::TemplatesDir => config.templates_dir.display().to_string(),
                ConfigField::ProfilesDir => config.profiles_dir.display().to_string(),
                ConfigField::ExtraPackages => list_value(&config.extra_packages),
                ConfigField::LocalEndpoints => endpoints_value(&config.local_endpoints),
                _ => "—".to_string(),
            },
            Draft::Sandbox(sandbox) => match field {
                ConfigField::Harness => sandbox.harness.label().to_string(),
                ConfigField::Vcpus => sandbox.vcpus.to_string(),
                ConfigField::Memory => sandbox.memory_mib.to_string(),
                ConfigField::AutoBranch => on_off(sandbox.auto_branch),
                ConfigField::Security => sandbox.security.label().to_string(),
                ConfigField::SharedTag => sandbox.shared_tag.clone(),
                ConfigField::GuestMount => sandbox.guest_mount.clone(),
                ConfigField::ExtraPackages => list_value(&sandbox.extra_packages),
                ConfigField::Project => sandbox.project.display().to_string(),
                ConfigField::Workspace => sandbox.workspace.display().to_string(),
                ConfigField::RootfsPath => {
                    let rootfs = sandbox
                        .rootfs_image
                        .clone()
                        .unwrap_or_else(|| sandbox.rootfs_path(&self.base));
                    rootfs.display().to_string()
                }
                ConfigField::NetworkIndex => match sandbox.network_index {
                    Some(index) => format!("TAP {index}"),
                    None => "assigned on first start".to_string(),
                },
                _ => "—".to_string(),
            },
        }
    }

    /// Move the selection to a row, skipping group headers and notes.
    ///
    /// An active inline edit is committed first; a failed commit keeps the
    /// cursor where it is so the invalid text can be fixed.
    pub fn select_row(&mut self, row: usize) -> bool {
        if !matches!(self.rows().get(row), Some(ConfigRow::Field(_))) {
            return false;
        }
        if self.edit.is_some()
            && self.edit.as_ref().map(|e| e.field) != self.field_at(row)
            && !self.commit_edit()
        {
            return false;
        }
        self.selected = row;
        true
    }

    pub fn select_delta(&mut self, delta: isize) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let step = if delta < 0 { -1 } else { 1 };
        let mut remaining = delta.abs().max(1);
        let mut index = self.selected;
        while remaining > 0 {
            let mut candidate = index as isize + step;
            while candidate >= 0 && (candidate as usize) < rows.len() {
                if matches!(rows[candidate as usize], ConfigRow::Field(_)) {
                    break;
                }
                candidate += step;
            }
            if candidate < 0 || candidate as usize >= rows.len() {
                return;
            }
            index = candidate as usize;
            remaining -= 1;
        }
        self.selected = index;
    }

    pub fn select_first(&mut self) {
        self.selected = first_field(self.kind, self.page);
    }

    pub fn select_last(&mut self) {
        if let Some(index) = self
            .rows()
            .iter()
            .rposition(|row| matches!(row, ConfigRow::Field(_)))
        {
            self.selected = index;
        }
    }

    /// Arrow-key change: cycle choices, step numbers, set booleans.
    pub fn adjust(&mut self, dir: i8) {
        if let Some(field) = self.selected_field() {
            self.adjust_field(field, dir);
        }
    }

    /// Apply a step to a specific field (used by the mouse stepper hit boxes).
    pub fn adjust_field(&mut self, field: ConfigField, dir: i8) {
        if self.edit.is_some() && !self.commit_edit() {
            return;
        }
        self.error = None;
        let result = match field.kind() {
            FieldKind::Choice => self.cycle(field, dir),
            FieldKind::Integer { .. } => self.step(field, dir),
            FieldKind::Bool => self.set_bool(field, dir > 0),
            _ => Ok(()),
        };
        self.finish(result);
    }

    /// `Enter` on the selected row.
    pub fn activate(&mut self) {
        let Some(field) = self.selected_field() else {
            return;
        };
        self.error = None;
        match field.kind() {
            FieldKind::Choice => {
                let result = self.cycle(field, 1);
                self.finish(result);
            }
            FieldKind::Bool => {
                let value = !self.bool_value(field);
                let result = self.set_bool(field, value);
                self.finish(result);
            }
            FieldKind::Integer { .. } | FieldKind::Text => self.begin_edit(field),
            FieldKind::Locked => {
                self.status = Some("read-only".into());
            }
        }
    }

    /// Primary mouse click on the selected row: changes the value directly.
    pub fn click(&mut self) {
        if self.edit.is_some() {
            return;
        }
        let Some(field) = self.selected_field() else {
            return;
        };
        self.error = None;
        match field.kind() {
            FieldKind::Choice => {
                let result = self.cycle(field, 1);
                self.finish(result);
            }
            FieldKind::Integer { .. } => {
                let result = self.step(field, 1);
                self.finish(result);
            }
            FieldKind::Bool => {
                let value = !self.bool_value(field);
                let result = self.set_bool(field, value);
                self.finish(result);
            }
            FieldKind::Text => self.begin_edit(field),
            FieldKind::Locked => {}
        }
    }

    /// `Enter` while an inline edit is open.
    fn begin_edit(&mut self, field: ConfigField) {
        if !matches!(field.kind(), FieldKind::Text | FieldKind::Integer { .. }) {
            return;
        }
        self.edit = Some(EditState {
            field,
            buffer: match field {
                ConfigField::ExtraPackages => match &self.draft {
                    Draft::Global(config) => config.extra_packages.join(", "),
                    Draft::Sandbox(sandbox) => sandbox.extra_packages.join(", "),
                },
                ConfigField::LocalEndpoints => match &self.draft {
                    Draft::Global(config) => config
                        .local_endpoints
                        .iter()
                        .map(|endpoint| format!("{}={}", endpoint.name, endpoint.port))
                        .collect::<Vec<_>>()
                        .join(", "),
                    Draft::Sandbox(_) => String::new(),
                },
                _ => self.value(field),
            },
        });
        self.status = None;
    }

    /// Commit the open inline edit. Returns `false` when the value is invalid
    /// and the editor stays open with an error message.
    pub fn commit_edit(&mut self) -> bool {
        let Some(edit) = self.edit.take() else {
            return true;
        };
        match self.apply_text(edit.field, edit.buffer.trim()) {
            Ok(()) => {
                self.error = None;
                self.discard_armed = false;
                true
            }
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                self.edit = Some(edit);
                false
            }
        }
    }

    pub fn cancel_edit(&mut self) {
        self.edit = None;
        self.error = None;
    }

    /// Handle a key press while the modal is open.
    pub fn handle_key(&mut self, key: KeyEvent) -> FormOutcome {
        if self.edit.is_some() {
            return self.handle_edit_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if !self.dirty() || self.discard_armed {
                    FormOutcome::Close
                } else {
                    self.discard_armed = true;
                    self.error = Some("unsaved changes — press Esc again to discard".to_string());
                    FormOutcome::Stay
                }
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.select_delta(-1);
                FormOutcome::Stay
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.select_delta(1);
                FormOutcome::Stay
            }
            KeyCode::PageUp => {
                self.select_delta(-5);
                FormOutcome::Stay
            }
            KeyCode::PageDown => {
                self.select_delta(5);
                FormOutcome::Stay
            }
            KeyCode::Home => {
                self.select_first();
                FormOutcome::Stay
            }
            KeyCode::End => {
                self.select_last();
                FormOutcome::Stay
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.adjust(-1);
                FormOutcome::Stay
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.adjust(1);
                FormOutcome::Stay
            }
            KeyCode::Char(' ') => {
                if self.selected_field().is_some_and(|field| {
                    matches!(field.kind(), FieldKind::Bool | FieldKind::Choice)
                }) {
                    self.activate();
                }
                FormOutcome::Stay
            }
            KeyCode::Enter => {
                self.activate();
                FormOutcome::Stay
            }
            KeyCode::Char('s') if ctrl || key.modifiers.is_empty() => FormOutcome::Save,
            KeyCode::Char('r') if !ctrl && !alt => FormOutcome::Revert,
            KeyCode::Char('e') if !ctrl && !alt => {
                if self.dirty() {
                    self.error = Some("save or revert before editing the raw file".into());
                    FormOutcome::Stay
                } else {
                    FormOutcome::OpenFile
                }
            }
            KeyCode::Char('[') if !ctrl && !alt => {
                self.cycle_page(-1);
                FormOutcome::Stay
            }
            KeyCode::Char(']') if !ctrl && !alt => {
                self.cycle_page(1);
                FormOutcome::Stay
            }
            _ => FormOutcome::Stay,
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> FormOutcome {
        match key.code {
            KeyCode::Esc => self.cancel_edit(),
            KeyCode::Enter => {
                self.commit_edit();
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return FormOutcome::Save;
            }
            KeyCode::Backspace => {
                if let Some(edit) = self.edit.as_mut() {
                    edit.buffer.pop();
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if self.commit_edit() {
                    self.select_delta(-1);
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                if self.commit_edit() {
                    self.select_delta(1);
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(edit) = self.edit.as_mut() {
                    edit.buffer.push(c);
                }
            }
            _ => {}
        }
        FormOutcome::Stay
    }

    /// Persist the working copy. On failure the modal stays open with `error`
    /// set and the draft untouched.
    pub fn save(&mut self) -> Option<Applied> {
        if self.edit.is_some() && !self.commit_edit() {
            return None;
        }
        let result = match &self.draft {
            Draft::Global(config) => config
                .save_to(&self.path)
                .with_context(|| format!("failed to save {}", self.path.display()))
                .map(|()| Applied::Global(config.clone())),
            Draft::Sandbox(sandbox) => SandboxStore::new((*self.base).clone())
                .save(sandbox)
                .map(|()| Applied::Sandbox),
        };
        match result {
            Ok(applied) => {
                self.original = self.draft.clone();
                self.error = None;
                self.status = Some("saved".into());
                self.discard_armed = false;
                Some(applied)
            }
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                self.status = None;
                None
            }
        }
    }

    fn finish(&mut self, result: Result<()>) {
        match result {
            Ok(()) => {
                self.error = None;
                self.status = None;
                self.discard_armed = false;
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    fn cycle(&mut self, field: ConfigField, dir: i8) -> Result<()> {
        match &mut self.draft {
            Draft::Global(config) => match field {
                ConfigField::Harness => {
                    config.harness = cycle_value(&Harness::ALL, config.harness, dir)
                }
                ConfigField::RootfsFormat => {
                    config.rootfs_format =
                        cycle_value(&RootfsFormat::ALL, config.rootfs_format, dir)
                }
                ConfigField::Network => {
                    config.network = cycle_value(&NetworkMode::ALL, config.network, dir)
                }
                ConfigField::Security => {
                    config.security = cycle_value(&SecurityMode::ALL, config.security, dir)
                }
                ConfigField::Backend => {
                    config.backend = cycle_value(&BackendKind::ALL, config.backend, dir)
                }
                _ => {}
            },
            Draft::Sandbox(sandbox) => match field {
                ConfigField::Harness => {
                    sandbox.harness = cycle_value(&Harness::ALL, sandbox.harness, dir)
                }
                ConfigField::Security => {
                    sandbox.security = cycle_value(&SecurityMode::ALL, sandbox.security, dir)
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn step(&mut self, field: ConfigField, dir: i8) -> Result<()> {
        let FieldKind::Integer { min, max, step } = field.kind() else {
            return Ok(());
        };
        let current = match (&self.draft, field) {
            (Draft::Global(config), ConfigField::Vcpus) => i64::from(config.vcpus),
            (Draft::Global(config), ConfigField::Memory) => i64::from(config.memory_mib),
            (Draft::Sandbox(sandbox), ConfigField::Vcpus) => i64::from(sandbox.vcpus),
            (Draft::Sandbox(sandbox), ConfigField::Memory) => i64::from(sandbox.memory_mib),
            _ => return Ok(()),
        };
        let next = (current + i64::from(dir.signum()) * step).clamp(min, max);
        self.set_integer(field, next)
    }

    fn set_integer(&mut self, field: ConfigField, value: i64) -> Result<()> {
        match (&mut self.draft, field) {
            (Draft::Global(config), ConfigField::Vcpus) => config.vcpus = value as u8,
            (Draft::Global(config), ConfigField::Memory) => config.memory_mib = value as u32,
            (Draft::Sandbox(sandbox), ConfigField::Vcpus) => sandbox.vcpus = value as u8,
            (Draft::Sandbox(sandbox), ConfigField::Memory) => sandbox.memory_mib = value as u32,
            _ => bail!("{} is not a number", field.label()),
        }
        Ok(())
    }

    fn bool_value(&self, field: ConfigField) -> bool {
        match (&self.draft, field) {
            (Draft::Global(config), ConfigField::AutoBranch) => config.auto_branch,
            (Draft::Global(config), ConfigField::CloudInit) => config.cloud_init,
            (Draft::Sandbox(sandbox), ConfigField::AutoBranch) => sandbox.auto_branch,
            _ => false,
        }
    }

    fn set_bool(&mut self, field: ConfigField, value: bool) -> Result<()> {
        match (&mut self.draft, field) {
            (Draft::Global(config), ConfigField::AutoBranch) => config.auto_branch = value,
            (Draft::Global(config), ConfigField::CloudInit) => config.cloud_init = value,
            (Draft::Sandbox(sandbox), ConfigField::AutoBranch) => sandbox.auto_branch = value,
            _ => bail!("{} is not a toggle", field.label()),
        }
        Ok(())
    }

    /// Validate and assign free-form text.
    fn apply_text(&mut self, field: ConfigField, text: &str) -> Result<()> {
        if text.is_empty()
            && !matches!(
                field,
                ConfigField::ExtraPackages | ConfigField::LocalEndpoints
            )
        {
            bail!("{} cannot be empty", field.label());
        }
        match (&mut self.draft, field) {
            (Draft::Global(config), ConfigField::Vcpus) => {
                config.vcpus = parse_number(text, 1, 32)? as u8;
            }
            (Draft::Global(config), ConfigField::Memory) => {
                config.memory_mib = parse_number(text, 128, 262_144)? as u32;
            }
            (Draft::Global(config), ConfigField::RootDevice) => {
                if !text.starts_with("/dev/") || text.contains(',') {
                    bail!("root device must look like /dev/vda1 and must not contain a comma");
                }
                config.root_device = text.to_string();
            }
            (Draft::Global(config), ConfigField::SharedTag) => {
                validate_shared_tag(text)?;
                config.shared_tag = text.to_string();
            }
            (Draft::Global(config), ConfigField::GuestMount) => {
                validate_guest_mount(text)?;
                config.guest_mount = text.to_string();
            }
            (Draft::Global(config), ConfigField::ExtraPackages) => {
                config.extra_packages = parse_packages(text)?;
            }
            (Draft::Global(config), ConfigField::LocalEndpoints) => {
                config.local_endpoints = parse_local_endpoints(text)?;
            }
            (Draft::Global(config), _) if field.is_path() => {
                set_global_path(config, field, expand_path(text))?;
            }
            (Draft::Sandbox(sandbox), ConfigField::Vcpus) => {
                sandbox.vcpus = parse_number(text, 1, 32)? as u8;
            }
            (Draft::Sandbox(sandbox), ConfigField::Memory) => {
                sandbox.memory_mib = parse_number(text, 128, 262_144)? as u32;
            }
            (Draft::Sandbox(sandbox), ConfigField::SharedTag) => {
                validate_shared_tag(text)?;
                sandbox.shared_tag = text.to_string();
            }
            (Draft::Sandbox(sandbox), ConfigField::GuestMount) => {
                validate_guest_mount(text)?;
                sandbox.guest_mount = text.to_string();
            }
            (Draft::Sandbox(sandbox), ConfigField::ExtraPackages) => {
                sandbox.extra_packages = parse_packages(text)?;
            }
            _ => bail!("{} cannot be edited", field.label()),
        }
        Ok(())
    }
}

fn first_field(kind: ConfigKind, page: ConfigPage) -> usize {
    rows_for(kind, page)
        .iter()
        .position(|row| matches!(row, ConfigRow::Field(_)))
        .unwrap_or(0)
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

fn list_value(items: &[String]) -> String {
    if items.is_empty() {
        "—".to_string()
    } else {
        items.join(", ")
    }
}

fn endpoints_value(endpoints: &[LocalEndpoint]) -> String {
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

fn parse_local_endpoints(text: &str) -> Result<Vec<LocalEndpoint>> {
    let mut endpoints = Vec::new();
    for item in text
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let (name, port) = item
            .split_once('=')
            .with_context(|| format!("endpoint '{item}' must look like name=port"))?;
        let port: u16 = port
            .trim()
            .parse()
            .with_context(|| format!("endpoint '{}' has an invalid port", name.trim()))?;
        endpoints.push(LocalEndpoint {
            name: name.trim().to_string(),
            port,
        });
    }
    validate_local_endpoints(&endpoints)?;
    Ok(endpoints)
}

fn cycle_value<T: Copy + PartialEq>(items: &[T], current: T, dir: i8) -> T {
    let index = items.iter().position(|item| *item == current).unwrap_or(0);
    let len = items.len();
    if len == 0 {
        return current;
    }
    let next = if dir < 0 {
        (index + len - 1) % len
    } else {
        (index + 1) % len
    };
    items[next]
}

fn parse_number(text: &str, min: i64, max: i64) -> Result<i64> {
    let value: i64 = text
        .trim()
        .parse()
        .with_context(|| format!("'{text}' is not a number"))?;
    if !(min..=max).contains(&value) {
        bail!("value must be between {min} and {max}");
    }
    Ok(value)
}

fn validate_shared_tag(text: &str) -> Result<()> {
    if !crate::cloud_init::safe_token(text) {
        bail!("shared tag may only contain letters, digits, '-', '_' and '.'");
    }
    if text.len() > 36 {
        bail!("shared tag must be at most 36 bytes");
    }
    Ok(())
}

fn validate_guest_mount(text: &str) -> Result<()> {
    if !crate::cloud_init::safe_mount(text) {
        bail!("guest mount must be an absolute path with simple components");
    }
    Ok(())
}

fn parse_packages(text: &str) -> Result<Vec<String>> {
    let mut packages = Vec::new();
    for package in text.split([',', ' ', '\t']).map(str::trim) {
        if package.is_empty() {
            continue;
        }
        if !safe_package_name(package) {
            bail!("invalid package name: {package:?}");
        }
        if !packages.iter().any(|existing| existing == package) {
            packages.push(package.to_string());
        }
    }
    Ok(packages)
}

fn expand_path(text: &str) -> PathBuf {
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(text));
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(text)
}

fn set_global_path(config: &mut Config, field: ConfigField, path: PathBuf) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{} cannot be empty", field.label());
    }
    match field {
        ConfigField::KernelImage => config.kernel_image = path,
        ConfigField::RootfsImage => config.rootfs_image = path,
        ConfigField::CloudHypervisorBin => config.cloud_hypervisor_bin = path,
        ConfigField::VirtiofsdBin => config.virtiofsd_bin = path,
        ConfigField::ProjectsRoot => config.projects_root = path,
        ConfigField::StateDir => config.state_dir = path,
        ConfigField::SnapshotsDir => config.snapshots_dir = path,
        ConfigField::SandboxesDir => config.sandboxes_dir = path,
        ConfigField::TemplatesDir => config.templates_dir = path,
        ConfigField::ProfilesDir => config.profiles_dir = path,
        _ => bail!("{} is not a path", field.label()),
    }
    Ok(())
}

/// Find the display row for a field. Used by tests and by callers that only
/// know the semantic setting they want to focus.
pub fn row_of(kind: ConfigKind, field: ConfigField) -> Option<usize> {
    rows(kind)
        .iter()
        .position(|row| matches!(row, ConfigRow::Field(candidate) if *candidate == field))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::path::Path;

    fn global_config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.projects_root = dir.join("projects");
        config.state_dir = dir.join("state");
        config.snapshots_dir = dir.join("snapshots");
        config.sandboxes_dir = dir.join("sandboxes");
        config.templates_dir = dir.join("templates");
        config.profiles_dir = dir.join("profiles");
        config
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn select(form: &mut ConfigForm, field: ConfigField) {
        let row = row_of(form.kind, field).unwrap();
        assert!(form.select_row(row));
    }

    #[test]
    fn navigation_skips_headers_and_notes() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        assert_eq!(form.selected_field(), Some(ConfigField::Harness));
        form.select_delta(-1);
        assert_eq!(
            form.selected_field(),
            Some(ConfigField::Harness),
            "the first field stays selected at the top"
        );
        form.select_last();
        assert_eq!(form.selected_field(), Some(ConfigField::ExtraPackages));
        form.select_delta(1);
        assert_eq!(form.selected_field(), Some(ConfigField::ExtraPackages));
    }

    #[test]
    fn choices_and_numbers_change_with_the_arrow_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        form.adjust(1);
        assert_eq!(form.value(ConfigField::Harness), "OpenCode");
        form.adjust(-1);
        assert_eq!(form.value(ConfigField::Harness), "shell");

        select(&mut form, ConfigField::Vcpus);
        form.adjust(1);
        assert_eq!(form.value(ConfigField::Vcpus), "3");
        form.adjust(-1);
        form.adjust(-1);
        assert_eq!(form.value(ConfigField::Vcpus), "1");

        select(&mut form, ConfigField::Memory);
        form.adjust(1);
        assert_eq!(form.value(ConfigField::Memory), "2304");
    }

    #[test]
    fn toggles_flip_on_enter() {
        let dir = tempfile::tempdir().unwrap();
        let config = global_config(dir.path());
        let expected = !config.auto_branch;
        let mut form = ConfigForm::global(config, dir.path().join("config.toml"));
        select(&mut form, ConfigField::AutoBranch);
        form.activate();
        assert_eq!(form.value(ConfigField::AutoBranch), on_off(expected));
    }

    #[test]
    fn escape_requires_confirmation_when_the_draft_is_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        assert_eq!(form.handle_key(key(KeyCode::Esc)), FormOutcome::Close);

        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        form.adjust(1);
        assert_eq!(form.handle_key(key(KeyCode::Esc)), FormOutcome::Stay);
        assert!(form.error.is_some());
        assert_eq!(form.handle_key(key(KeyCode::Esc)), FormOutcome::Close);
    }

    #[test]
    fn invalid_inline_text_keeps_the_editor_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        select(&mut form, ConfigField::SharedTag);
        form.activate();
        for ch in "bad tag".chars() {
            form.handle_key(key(KeyCode::Char(ch)));
        }
        form.handle_key(key(KeyCode::Enter));
        assert!(form.is_editing());
        assert!(form.error.is_some());
        assert_eq!(form.value(ConfigField::SharedTag), "workspace");

        form.cancel_edit();
        assert!(!form.is_editing());
        assert!(!form.dirty());
    }

    #[test]
    fn saving_writes_the_global_config_and_clears_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut form = ConfigForm::global(global_config(dir.path()), path.clone());
        form.adjust(1);
        assert!(form.dirty());
        let applied = form.save().expect("save succeeds");
        match applied {
            Applied::Global(config) => assert_eq!(config.harness, Harness::Opencode),
            Applied::Sandbox => panic!("expected a global config"),
        }
        assert!(!form.dirty());
        assert_eq!(Config::load_from(&path).unwrap().harness, Harness::Opencode);
    }

    #[test]
    fn editing_a_path_expands_a_leading_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        select(&mut form, ConfigField::ProjectsRoot);
        form.activate();
        if let Some(edit) = form.edit.as_mut() {
            edit.buffer = "~/agent-projects".into();
        }
        form.commit_edit();
        let expected = dirs::home_dir().unwrap().join("agent-projects");
        assert_eq!(
            form.value(ConfigField::ProjectsRoot),
            expected.display().to_string()
        );
    }

    #[test]
    fn packages_are_split_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        select(&mut form, ConfigField::ExtraPackages);
        form.activate();
        if let Some(edit) = form.edit.as_mut() {
            edit.buffer = "ripgrep, fd-find fd-find".into();
        }
        form.commit_edit();
        assert_eq!(form.value(ConfigField::ExtraPackages), "ripgrep, fd-find");

        select(&mut form, ConfigField::ExtraPackages);
        form.activate();
        if let Some(edit) = form.edit.as_mut() {
            edit.buffer = "-rm".into();
        }
        form.commit_edit();
        assert!(form.error.is_some());
        assert!(form.is_editing());
    }

    #[test]
    fn sandbox_form_saves_into_the_sandbox_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = global_config(dir.path());
        let store = SandboxStore::new(config.clone());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let sandbox = store
            .create_from_project(&project, Some("demo"), None, false)
            .unwrap();

        let mut form = ConfigForm::sandbox(sandbox, config);
        assert_eq!(form.kind, ConfigKind::Sandbox);
        assert_eq!(form.sandbox_name(), Some("demo"));
        select(&mut form, ConfigField::Vcpus);
        form.adjust(1);
        form.adjust(1);
        assert!(form.save().is_some());
        let reloaded = SandboxStore::new(global_config(dir.path()))
            .load("demo")
            .unwrap();
        assert_eq!(reloaded.vcpus, 4);
    }

    #[test]
    fn local_llm_page_edits_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut form = ConfigForm::global(global_config(dir.path()), path.clone());
        assert_eq!(form.pages(), &[ConfigPage::General, ConfigPage::LocalLlm]);
        assert_eq!(form.page(), ConfigPage::General);

        form.handle_key(key(KeyCode::Char(']')));
        assert_eq!(form.page(), ConfigPage::LocalLlm);
        assert_eq!(form.selected_field(), Some(ConfigField::LocalEndpoints));

        form.activate();
        assert_eq!(
            form.editing_buffer(ConfigField::LocalEndpoints),
            Some(""),
            "an empty list starts with an empty buffer, not the display dash"
        );
        if let Some(edit) = form.edit.as_mut() {
            edit.buffer = "ollama=11434, lmstudio=1234".into();
        }
        assert!(form.commit_edit(), "valid endpoints are accepted");
        assert_eq!(
            form.value(ConfigField::LocalEndpoints),
            "ollama=11434, lmstudio=1234"
        );
        assert!(form.save().is_some());
        let saved = Config::load_from(&path).unwrap();
        assert_eq!(saved.endpoint_ports(), vec![1234, 11434]);

        form.handle_key(key(KeyCode::Char('[')));
        assert_eq!(form.page(), ConfigPage::General);
    }

    #[test]
    fn invalid_endpoints_keep_the_editor_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        form.set_page(ConfigPage::LocalLlm);
        form.activate();
        if let Some(edit) = form.edit.as_mut() {
            edit.buffer = "ollama=22".into();
        }
        assert!(!form.commit_edit());
        assert!(form.error.is_some());
        assert!(form.is_editing());
    }

    #[test]
    fn sandbox_form_has_no_endpoint_page() {
        let dir = tempfile::tempdir().unwrap();
        let config = global_config(dir.path());
        let store = SandboxStore::new(config.clone());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let sandbox = store
            .create_from_project(&project, Some("demo"), None, false)
            .unwrap();
        let form = ConfigForm::sandbox(sandbox, config);
        assert_eq!(form.pages(), &[ConfigPage::General]);
        assert_eq!(form.value(ConfigField::LocalEndpoints), "—");
    }

    #[test]
    fn save_key_commits_an_open_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut form = ConfigForm::global(global_config(dir.path()), path.clone());
        select(&mut form, ConfigField::SharedTag);
        form.activate();
        form.handle_key(key(KeyCode::Backspace));
        for ch in "agent".chars() {
            form.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(
            form.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            FormOutcome::Save
        );
        assert!(form.save().is_some());
        assert_eq!(
            Config::load_from(&path).unwrap().shared_tag,
            "workspacagent"
        );
    }

    #[test]
    fn raw_file_edit_requires_a_clean_draft() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        assert_eq!(
            form.handle_key(key(KeyCode::Char('e'))),
            FormOutcome::OpenFile
        );
        form.adjust(1);
        assert_eq!(form.handle_key(key(KeyCode::Char('e'))), FormOutcome::Stay);
        assert!(form.error.is_some());
    }
}
