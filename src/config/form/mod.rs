//! Interactive settings editor behind the `F4`/`F5` configuration modal.
//!
//! The modal edits a working copy of the global `config.toml` or an instance's
//! `hegn.toml`. Nothing touches disk until the user saves, and every change is
//! validated against the same schema the CLI uses. Keyboard handling lives here
//! so navigation, editing and persistence are unit-testable without a terminal;
//! [`crate::tui::ui`] only draws the rows and records their hit rectangles.
//!
//! Row tables live in [`rows`], working state and navigation in [`state`],
//! inline editing in [`edit`], text parsing in [`parse`] and disk interaction
//! in [`persist`].

mod edit;
mod parse;
mod persist;
mod rows;
mod state;

#[cfg(test)]
mod test_support;

pub use self::persist::Applied;
pub use self::rows::{
    ConfigField, ConfigKind, ConfigPage, ConfigRow, FieldKind, pages, row_of, rows, rows_for,
};
pub use self::state::{ConfigForm, FormOutcome};

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::config::form::test_support::{global_config, key};
    use crate::config::form::{ConfigField, ConfigForm, ConfigPage};

    #[test]
    fn local_llm_page_edits_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut form = ConfigForm::global(global_config(dir.path()), path.clone());
        assert_eq!(
            form.pages(),
            &[
                ConfigPage::General,
                ConfigPage::Profiles,
                ConfigPage::LocalLlm
            ]
        );
        assert_eq!(form.page(), ConfigPage::General);

        form.handle_key(key(crossterm::event::KeyCode::Char(']')));
        assert_eq!(form.page(), ConfigPage::Profiles);
        form.handle_key(key(crossterm::event::KeyCode::Char(']')));
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

        form.handle_key(key(crossterm::event::KeyCode::Char('[')));
        assert_eq!(form.page(), ConfigPage::Profiles);
        form.handle_key(key(crossterm::event::KeyCode::Char('[')));
        assert_eq!(form.page(), ConfigPage::General);
    }
}
