//! Persisting the modal's working copy to disk.

use anyhow::Context;

use crate::config::Config;
use crate::vm::instance_config::InstanceStore;

use super::state::{ConfigForm, Draft};

/// A successfully persisted draft.
#[derive(Debug, Clone)]
pub enum Applied {
    Global(Box<Config>),
    Instance,
}

impl ConfigForm {
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
            Draft::Instance(instance) => InstanceStore::new((*self.base).clone())
                .save(instance)
                .map(|()| Applied::Instance),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::Harness;
    use crate::config::form::ConfigForm;
    use crate::config::form::test_support::{global_config, select};
    use crate::config::form::{ConfigField, ConfigKind};
    use crate::vm::instance_config::InstanceStore;

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
            Applied::Instance => panic!("expected a global config"),
        }
        assert!(!form.dirty());
        assert_eq!(Config::load_from(&path).unwrap().harness, Harness::Opencode);
    }

    #[test]
    fn instance_form_saves_into_the_instance_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = global_config(dir.path());
        let store = InstanceStore::new(config.clone());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let instance = store
            .create_from_project(&project, Some("demo"), None, false)
            .unwrap();

        let mut form = ConfigForm::instance(instance, config);
        assert_eq!(form.kind, ConfigKind::Instance);
        assert_eq!(form.instance_name(), Some("demo"));
        select(&mut form, ConfigField::Vcpus);
        form.adjust(1);
        form.adjust(1);
        assert!(form.save().is_some());
        let reloaded = InstanceStore::new(global_config(dir.path()))
            .load("demo")
            .unwrap();
        assert_eq!(reloaded.vcpus, 4);
    }
}
