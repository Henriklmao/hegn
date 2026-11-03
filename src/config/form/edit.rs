//! Inline text editing inside the modal: beginning, committing and cancelling
//! a field edit, plus the keystrokes typed while the editor is open.

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::parse::{
    expand_path, parse_local_endpoints, parse_number, parse_packages, set_global_path,
    validate_guest_mount, validate_shared_tag,
};
use super::rows::{ConfigField, FieldKind};
use super::state::{ConfigForm, Draft, EditState, FormOutcome};

impl ConfigForm {
    /// `Enter` while an inline edit is open.
    pub(super) fn begin_edit(&mut self, field: ConfigField) {
        if !matches!(field.kind(), FieldKind::Text | FieldKind::Integer { .. }) {
            return;
        }
        self.edit = Some(EditState {
            field,
            buffer: match field {
                ConfigField::ExtraPackages => match &self.draft {
                    Draft::Global(config) => config.extra_packages.join(", "),
                    Draft::Instance(instance) => instance.extra_packages.join(", "),
                },
                ConfigField::LocalEndpoints => match &self.draft {
                    Draft::Global(config) => config
                        .local_endpoints
                        .iter()
                        .map(|endpoint| format!("{}={}", endpoint.name, endpoint.port))
                        .collect::<Vec<_>>()
                        .join(", "),
                    Draft::Instance(_) => String::new(),
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

    pub(super) fn handle_edit_key(&mut self, key: KeyEvent) -> FormOutcome {
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
            (Draft::Instance(instance), ConfigField::Vcpus) => {
                instance.vcpus = parse_number(text, 1, 32)? as u8;
            }
            (Draft::Instance(instance), ConfigField::Memory) => {
                instance.memory_mib = parse_number(text, 128, 262_144)? as u32;
            }
            (Draft::Instance(instance), ConfigField::SharedTag) => {
                validate_shared_tag(text)?;
                instance.shared_tag = text.to_string();
            }
            (Draft::Instance(instance), ConfigField::GuestMount) => {
                validate_guest_mount(text)?;
                instance.guest_mount = text.to_string();
            }
            (Draft::Instance(instance), ConfigField::ExtraPackages) => {
                instance.extra_packages = parse_packages(text)?;
            }
            _ => bail!("{} cannot be edited", field.label()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::form::test_support::{global_config, key, select};

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
}
