//! Shared fixtures for the settings-form unit tests.

#![allow(clippy::field_reassign_with_default)]

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;

use super::rows::{ConfigField, row_of};
use super::state::ConfigForm;

/// A global config whose mutable directories live under `dir`.
pub(super) fn global_config(dir: &Path) -> Config {
    let mut config = Config::default();
    config.projects_root = dir.join("projects");
    config.state_dir = dir.join("state");
    config.snapshots_dir = dir.join("snapshots");
    config.instances_dir = dir.join("instances");
    config.templates_dir = dir.join("templates");
    config.profiles_dir = dir.join("profiles");
    config
}

/// A key event without modifiers.
pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Move the selection to the row of `field`.
pub(super) fn select(form: &mut ConfigForm, field: ConfigField) {
    let row = row_of(form.kind, field).unwrap();
    assert!(form.select_row(row));
}
