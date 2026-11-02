//! Settings modals: the global, per-instance and per-project editors and their
//! save/revert/launch transitions.

use std::path::Path;

use crate::agent::launch::{self, Integration};
use crate::config::form::{Applied, ConfigPage};
use crate::config::{Config, Harness};
use crate::project::{self, ProjectMeta};
use crate::security::SecurityEngine;
use crate::session::state::StateStore;

use super::util::cycle_value;
use super::*;

impl App {
    pub(super) fn open_global_config(&mut self) -> Vec<Effect> {
        self.show_global_config_form(None);
        Vec::new()
    }

    pub(super) fn show_global_config_form(&mut self, status: Option<String>) {
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

    pub(super) fn open_instance_config(&mut self) -> Vec<Effect> {
        let Some(instance) = self
            .selected_instance()
            .map(|instance| instance.name.clone())
        else {
            self.toast("select an instance first", ToastLevel::Warn);
            return Vec::new();
        };
        self.show_instance_config_form(&instance, None);
        Vec::new()
    }

    pub(super) fn show_instance_config_form(&mut self, name: &str, status: Option<String>) {
        let store = crate::vm::instance_config::InstanceStore::new(self.config.clone());
        match store.load(name) {
            Ok(instance) => {
                let mut form = ConfigForm::instance(instance, self.config.clone());
                form.status = status;
                self.config_hover = None;
                self.modal = Some(Modal::Config(form));
            }
            Err(err) => self.toast(
                format!("could not load instance config: {err:#}"),
                ToastLevel::Error,
            ),
        }
    }

    /// Persist the settings modal and apply the result. The modal stays open so
    /// the user can keep adjusting; a failed save leaves the draft intact.
    pub(super) fn save_config_form(&mut self, mut form: ConfigForm) -> Vec<Effect> {
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
            Some(Applied::Instance) => {
                self.refresh_instances();
                self.modal = Some(Modal::Config(form));
                self.toast("instance settings saved", ToastLevel::Info);
                Vec::new()
            }
            None => {
                self.modal = Some(Modal::Config(form));
                Vec::new()
            }
        }
    }

    /// Reload the settings modal from disk, discarding unsaved changes.
    pub(super) fn revert_config_form(&mut self, mut form: ConfigForm) -> Vec<Effect> {
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
            ConfigKind::Instance => {
                match form.instance_name().map(str::to_string) {
                    Some(name) => self.show_instance_config_form(
                        &name,
                        Some("reverted to the file on disk".into()),
                    ),
                    None => {
                        form.error = Some("instance name is missing".into());
                        self.modal = Some(Modal::Config(form));
                    }
                }
                Vec::new()
            }
        }
    }

    /// Install a saved global configuration in the running UI.
    pub(super) fn apply_global_config(&mut self, config: Config) {
        self.security_engine = SecurityEngine::new(config.security);
        self.store = StateStore::load(config.state_dir.join("state.json"));
        self.config = config;
        self.instance.vcpus = self.config.vcpus;
        self.instance.memory_mib = self.config.memory_mib;
        self.instance.virtiofs = if self.config.vm_assets_ready() {
            "configured"
        } else {
            "missing VM"
        };
        self.refresh_snapshots();
        self.refresh_instances();
    }

    /// Open the per-project settings editor for the project screen.
    pub(super) fn open_project_config(&mut self) -> Vec<Effect> {
        let Some(project) = self.open_project.clone() else {
            self.toast(
                "open a project first — its settings live in hegn.toml",
                ToastLevel::Warn,
            );
            return Vec::new();
        };
        let meta = match project::try_read_meta(&project) {
            Ok(meta) => meta,
            Err(err) => {
                self.toast(
                    format!("cannot edit project settings: {err:#}"),
                    ToastLevel::Error,
                );
                return Vec::new();
            }
        };
        self.project_meta = meta.clone();
        self.modal = Some(Modal::ProjectConfig(ProjectConfigForm::new(project, &meta)));
        Vec::new()
    }

    /// Change the value under the cursor in the project settings form.
    pub(super) fn cycle_project_config(&self, form: &mut ProjectConfigForm, delta: isize) {
        match form.row {
            0 => {
                let mut values = vec![
                    None,
                    Some(launch::HARNESS.to_string()),
                    Some(launch::SHELL.to_string()),
                ];
                values.extend(
                    self.config
                        .launch
                        .commands
                        .iter()
                        .map(|command| Some(command.name.clone())),
                );
                form.launch = cycle_value(&values, &form.launch, delta);
            }
            1 => {
                let values: Vec<Option<Integration>> = std::iter::once(None)
                    .chain(Integration::ALL.map(Some))
                    .collect();
                form.integration = cycle_value(&values, &form.integration, delta);
            }
            2 => {
                let values = [None, Some(true), Some(false)];
                form.auto_start = cycle_value(&values, &form.auto_start, delta);
            }
            3 => {
                let values: Vec<Option<Harness>> = std::iter::once(None)
                    .chain(Harness::ALL.map(Some))
                    .collect();
                form.harness = cycle_value(&values, &form.harness, delta);
            }
            _ => {}
        }
    }

    /// Write the form values to the project's `hegn.toml`.
    pub(super) fn persist_project_config(form: &ProjectConfigForm) -> anyhow::Result<ProjectMeta> {
        // Refuse to rewrite a file that no longer parses: `read_meta` would
        // silently return defaults and wipe the user's hand-written settings.
        let mut meta = project::try_read_meta(&form.project)?;
        meta.harness = form.harness;
        meta.launch = form.launch.clone();
        meta.integration = form.integration;
        meta.auto_start = form.auto_start;
        project::write_meta(&form.project, &meta)?;
        Ok(meta)
    }

    pub(super) fn apply_project_meta(&mut self, project: &Path, meta: ProjectMeta) {
        if self.open_project.as_deref() == Some(project) {
            self.project_meta = meta;
        }
    }

    pub(super) fn save_project_config(&mut self, form: ProjectConfigForm) -> Vec<Effect> {
        match Self::persist_project_config(&form) {
            Ok(meta) => {
                self.apply_project_meta(&form.project, meta);
                self.modal = None;
                self.toast(
                    format!(
                        "project settings saved to {}",
                        form.project.join(project::PROJECT_META_FILE).display()
                    ),
                    ToastLevel::Info,
                );
                Vec::new()
            }
            Err(err) => {
                let mut form = form;
                form.error = Some(format!("could not save hegn.toml: {err}"));
                self.modal = Some(Modal::ProjectConfig(form));
                Vec::new()
            }
        }
    }

    /// Save the settings and launch the selected command in the chosen
    /// integration (`embedded`, `external`, `tmux`, or `herdr`).
    pub(super) fn launch_project_config(&mut self, form: ProjectConfigForm) -> Vec<Effect> {
        let meta = match Self::persist_project_config(&form) {
            Ok(meta) => meta,
            Err(err) => {
                let mut form = form;
                form.error = Some(format!("could not save hegn.toml: {err}"));
                self.modal = Some(Modal::ProjectConfig(form));
                return Vec::new();
            }
        };
        self.apply_project_meta(&form.project, meta);
        self.modal = None;

        let Some(instance) = self.selected_instance().cloned() else {
            self.toast(
                "start the project first; the command runs inside its VM",
                ToastLevel::Warn,
            );
            return Vec::new();
        };
        let spec = match launch::resolve(
            &self.config,
            instance.harness,
            form.launch.as_deref(),
            form.integration,
        ) {
            Ok(spec) => spec,
            Err(err) => {
                self.toast(format!("launch failed: {err}"), ToastLevel::Error);
                return Vec::new();
            }
        };
        vec![Effect::LaunchAgent {
            spec,
            instance: instance.name,
            project: instance.workspace,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn project_settings_cycle_the_integration_and_save_hegn_toml() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let instance = fake_instance(&app, "demo", project.join("workspace"));
        install_instances(&mut app, vec![instance]);
        app.screen = Screen::Project;
        app.open_project = Some(project.clone());

        app.handle_key(key(KeyCode::F(6)));
        assert!(matches!(app.modal, Some(Modal::ProjectConfig(_))));

        // Row 0 is the launch command; row 1 is the integration.
        app.handle_key(key(KeyCode::Down));
        for _ in 0..4 {
            app.handle_key(key(KeyCode::Right));
        }
        match &app.modal {
            Some(Modal::ProjectConfig(form)) => {
                assert_eq!(form.row, 1);
                assert_eq!(form.integration, Some(Integration::Tmux));
            }
            other => panic!("expected project config, got {other:?}"),
        }

        app.handle_key(key(KeyCode::Enter));
        assert!(app.modal.is_none(), "saving closes the form");
        let meta = project::read_meta(&project);
        assert_eq!(meta.integration, Some(Integration::Tmux));
        assert!(project.join(project::PROJECT_META_FILE).is_file());
        assert_eq!(app.project_meta.integration, Some(Integration::Tmux));
    }

    #[test]
    fn project_settings_open_from_the_focused_terminal() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        app.screen = Screen::Project;
        app.open_project = Some(project);
        app.terminal_focus = true;
        app.terminal_mode = TerminalMode::Insert;

        app.handle_key(key(KeyCode::F(6)));
        assert!(matches!(app.modal, Some(Modal::ProjectConfig(_))));
    }

    #[test]
    fn project_settings_refuse_to_overwrite_a_malformed_hegn_toml() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let file = project.join(project::PROJECT_META_FILE);
        std::fs::write(&file, "harness = [\n").unwrap();
        app.screen = Screen::Project;
        app.open_project = Some(project);

        app.handle_key(key(KeyCode::F(6)));
        assert!(app.modal.is_none(), "the editor must not open");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "harness = [\n");
    }

    #[test]
    fn project_settings_save_and_launch_returns_a_launch_effect() {
        let (_dir, mut app) = test_app();
        let project = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        let instance = fake_instance(&app, "demo", project.join("workspace"));
        install_instances(&mut app, vec![instance]);
        app.screen = Screen::Project;
        app.open_project = Some(project.clone());
        app.config.launch.default = Some(launch::HARNESS.to_string());

        app.handle_key(key(KeyCode::F(6)));
        app.handle_key(key(KeyCode::Down));
        for _ in 0..4 {
            app.handle_key(key(KeyCode::Right));
        }
        let effects = app.handle_key(key(KeyCode::Char('r')));
        let [Effect::LaunchAgent { spec, instance, .. }] = effects.as_slice() else {
            panic!("expected one launch effect, got {effects:?}");
        };
        assert_eq!(instance, "demo");
        assert_eq!(spec.integration, Integration::Tmux);
        assert_eq!(
            project::read_meta(&project).integration,
            Some(Integration::Tmux)
        );
    }

    #[test]
    fn config_menus_open_for_global_and_instance() {
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
        let inside = app.config.instances_dir.join("demo").join("workspace");
        app.focus = Focus::Instances;
        let instance = fake_instance(&app, "demo", inside.clone());
        let stored = crate::vm::instance_config::InstanceConfig {
            name: "demo".into(),
            project: instance.project.clone(),
            workspace: inside,
            harness: Harness::Shell,
            ..crate::vm::instance_config::InstanceConfig::default()
        };
        crate::vm::instance_config::InstanceStore::new(app.config.clone())
            .save(&stored)
            .unwrap();
        install_instances(&mut app, vec![instance]);
        app.handle_key(key(KeyCode::F(5)));
        match &app.modal {
            Some(Modal::Config(form)) => assert_eq!(form.kind, ConfigKind::Instance),
            other => panic!("expected instance settings, got {other:?}"),
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
}
