//! Working state of the settings modal: constructors, navigation, value
//! stepping and keyboard dispatch.
//!
//! Inline text editing lives in [`super::edit`], persistence in
//! [`super::persist`] and the row tables in [`super::rows`].

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{BackendKind, Config, Harness, NetworkMode, RootfsFormat, SecurityMode};
use crate::shared::math::cycle_value;
use crate::vm::instance_config::{CONFIG_FILE, InstanceConfig};

use super::rows::{
    ConfigField, ConfigKind, ConfigPage, ConfigRow, FieldKind, endpoints_value, first_field,
    list_value, on_off, pages, rows_for,
};

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
    /// Open a harness profile template in `$EDITOR`.
    OpenProfile(Harness),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Draft {
    Global(Box<Config>),
    Instance(Box<InstanceConfig>),
}

#[derive(Debug, Clone)]
pub(super) struct EditState {
    pub(super) field: ConfigField,
    pub(super) buffer: String,
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
    pub(super) draft: Draft,
    pub(super) original: Draft,
    /// Global config the instance belongs to; used for derived paths and saving.
    pub(super) base: Box<Config>,
    page: ConfigPage,
    selected: usize,
    pub(super) edit: Option<EditState>,
    /// First `Esc` on a dirty draft only warns; the second one discards it.
    pub(super) discard_armed: bool,
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

    /// Open the editor for an existing instance.
    pub fn instance(instance: InstanceConfig, base: Config) -> Self {
        let path = base.instances_dir.join(&instance.name).join(CONFIG_FILE);
        let title = format!("instance '{}' settings", instance.name);
        Self::build(
            ConfigKind::Instance,
            title,
            path,
            Draft::Instance(Box::new(instance)),
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

    pub fn instance_name(&self) -> Option<&str> {
        match &self.draft {
            Draft::Instance(instance) => Some(instance.name.as_str()),
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
                ConfigField::InstancesDir => config.instances_dir.display().to_string(),
                ConfigField::TemplatesDir => config.templates_dir.display().to_string(),
                ConfigField::ProfilesDir => config.profiles_dir.display().to_string(),
                ConfigField::ExtraPackages => list_value(&config.extra_packages),
                ConfigField::LocalEndpoints => endpoints_value(&config.local_endpoints),
                ConfigField::Profile(harness) => self.profile_value(harness),
                _ => "—".to_string(),
            },
            Draft::Instance(instance) => match field {
                ConfigField::Harness => instance.harness.label().to_string(),
                ConfigField::Vcpus => instance.vcpus.to_string(),
                ConfigField::Memory => instance.memory_mib.to_string(),
                ConfigField::AutoBranch => on_off(instance.auto_branch),
                ConfigField::Security => instance.security.label().to_string(),
                ConfigField::SharedTag => instance.shared_tag.clone(),
                ConfigField::GuestMount => instance.guest_mount.clone(),
                ConfigField::ExtraPackages => list_value(&instance.extra_packages),
                ConfigField::Project => instance.project.display().to_string(),
                ConfigField::Workspace => instance.workspace.display().to_string(),
                ConfigField::RootfsPath => {
                    let rootfs = instance
                        .rootfs_image
                        .clone()
                        .unwrap_or_else(|| instance.rootfs_path(&self.base));
                    rootfs.display().to_string()
                }
                ConfigField::NetworkIndex => match instance.network_index {
                    Some(index) => format!("TAP {index}"),
                    None => "assigned on first start".to_string(),
                },
                ConfigField::Profile(harness) => self.profile_value(harness),
                _ => "—".to_string(),
            },
        }
    }

    /// Path (and state) of a harness profile template.
    fn profile_value(&self, harness: Harness) -> String {
        let config = match &self.draft {
            Draft::Global(config) => config.as_ref(),
            Draft::Instance(_) => self.base.as_ref(),
        };
        let path = crate::agent::profile::ProfileStore::new(config).path(harness);
        if path.is_file() {
            path.display().to_string()
        } else {
            format!("{} (missing)", path.display())
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
    pub fn activate(&mut self) -> FormOutcome {
        let Some(field) = self.selected_field() else {
            return FormOutcome::Stay;
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
            FieldKind::Action => {
                if let ConfigField::Profile(harness) = field {
                    return FormOutcome::OpenProfile(harness);
                }
            }
            FieldKind::Locked => {
                self.status = Some("read-only".into());
            }
        }
        FormOutcome::Stay
    }

    /// Primary mouse click on the selected row: changes the value directly.
    pub fn click(&mut self) -> FormOutcome {
        if self.edit.is_some() {
            return FormOutcome::Stay;
        }
        let Some(field) = self.selected_field() else {
            return FormOutcome::Stay;
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
            FieldKind::Action => {
                if let ConfigField::Profile(harness) = field {
                    return FormOutcome::OpenProfile(harness);
                }
            }
            FieldKind::Locked => {}
        }
        FormOutcome::Stay
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
            KeyCode::Enter => self.activate(),
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

    fn finish(&mut self, result: anyhow::Result<()>) {
        match result {
            Ok(()) => {
                self.error = None;
                self.status = None;
                self.discard_armed = false;
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    fn cycle(&mut self, field: ConfigField, dir: i8) -> anyhow::Result<()> {
        match &mut self.draft {
            Draft::Global(config) => match field {
                ConfigField::Harness => {
                    config.harness = cycle_value(&Harness::ALL, &config.harness, dir.into())
                }
                ConfigField::RootfsFormat => {
                    config.rootfs_format =
                        cycle_value(&RootfsFormat::ALL, &config.rootfs_format, dir.into())
                }
                ConfigField::Network => {
                    config.network = cycle_value(&NetworkMode::ALL, &config.network, dir.into())
                }
                ConfigField::Security => {
                    config.security = cycle_value(&SecurityMode::ALL, &config.security, dir.into())
                }
                ConfigField::Backend => {
                    config.backend = cycle_value(&BackendKind::ALL, &config.backend, dir.into())
                }
                _ => {}
            },
            Draft::Instance(instance) => match field {
                ConfigField::Harness => {
                    instance.harness = cycle_value(&Harness::ALL, &instance.harness, dir.into())
                }
                ConfigField::Security => {
                    instance.security =
                        cycle_value(&SecurityMode::ALL, &instance.security, dir.into())
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn step(&mut self, field: ConfigField, dir: i8) -> anyhow::Result<()> {
        let FieldKind::Integer { min, max, step } = field.kind() else {
            return Ok(());
        };
        let current = match (&self.draft, field) {
            (Draft::Global(config), ConfigField::Vcpus) => i64::from(config.vcpus),
            (Draft::Global(config), ConfigField::Memory) => i64::from(config.memory_mib),
            (Draft::Instance(instance), ConfigField::Vcpus) => i64::from(instance.vcpus),
            (Draft::Instance(instance), ConfigField::Memory) => i64::from(instance.memory_mib),
            _ => return Ok(()),
        };
        let next = (current + i64::from(dir.signum()) * step).clamp(min, max);
        self.set_integer(field, next)
    }

    fn set_integer(&mut self, field: ConfigField, value: i64) -> anyhow::Result<()> {
        match (&mut self.draft, field) {
            (Draft::Global(config), ConfigField::Vcpus) => config.vcpus = value as u8,
            (Draft::Global(config), ConfigField::Memory) => config.memory_mib = value as u32,
            (Draft::Instance(instance), ConfigField::Vcpus) => instance.vcpus = value as u8,
            (Draft::Instance(instance), ConfigField::Memory) => instance.memory_mib = value as u32,
            _ => anyhow::bail!("{} is not a number", field.label()),
        }
        Ok(())
    }

    fn bool_value(&self, field: ConfigField) -> bool {
        match (&self.draft, field) {
            (Draft::Global(config), ConfigField::AutoBranch) => config.auto_branch,
            (Draft::Global(config), ConfigField::CloudInit) => config.cloud_init,
            (Draft::Instance(instance), ConfigField::AutoBranch) => instance.auto_branch,
            _ => false,
        }
    }

    fn set_bool(&mut self, field: ConfigField, value: bool) -> anyhow::Result<()> {
        match (&mut self.draft, field) {
            (Draft::Global(config), ConfigField::AutoBranch) => config.auto_branch = value,
            (Draft::Global(config), ConfigField::CloudInit) => config.cloud_init = value,
            (Draft::Instance(instance), ConfigField::AutoBranch) => instance.auto_branch = value,
            _ => anyhow::bail!("{} is not a toggle", field.label()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::form::test_support::{global_config, key, select};

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
    fn profiles_page_lists_harnesses_and_opens_the_editor() {
        let dir = tempfile::tempdir().unwrap();
        let config = global_config(dir.path());
        let mut form = ConfigForm::global(config, dir.path().join("config.toml"));
        assert!(form.set_page(ConfigPage::Profiles));
        assert_eq!(
            form.selected_field(),
            Some(ConfigField::Profile(Harness::Opencode))
        );
        assert!(
            form.value(ConfigField::Profile(Harness::Opencode))
                .contains("opencode.toml")
        );
        assert_eq!(
            form.handle_key(key(KeyCode::Enter)),
            FormOutcome::OpenProfile(Harness::Opencode)
        );
        form.select_delta(1);
        assert_eq!(
            form.selected_field(),
            Some(ConfigField::Profile(Harness::ClaudeCode))
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
