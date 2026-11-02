//! Parsing and validation of the free-form text edited in the modal.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::{Config, LocalEndpoint, safe_package_name, validate_local_endpoints};

use super::rows::ConfigField;

pub(super) fn parse_local_endpoints(text: &str) -> Result<Vec<LocalEndpoint>> {
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

pub(super) fn parse_number(text: &str, min: i64, max: i64) -> Result<i64> {
    let value: i64 = text
        .trim()
        .parse()
        .with_context(|| format!("'{text}' is not a number"))?;
    if !(min..=max).contains(&value) {
        bail!("value must be between {min} and {max}");
    }
    Ok(value)
}

pub(super) fn validate_shared_tag(text: &str) -> Result<()> {
    if !crate::vm::cloud_init::safe_token(text) {
        bail!("shared tag may only contain letters, digits, '-', '_' and '.'");
    }
    if text.len() > 36 {
        bail!("shared tag must be at most 36 bytes");
    }
    Ok(())
}

pub(super) fn validate_guest_mount(text: &str) -> Result<()> {
    if !crate::vm::cloud_init::safe_mount(text) {
        bail!("guest mount must be an absolute path with simple components");
    }
    Ok(())
}

pub(super) fn parse_packages(text: &str) -> Result<Vec<String>> {
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

pub(super) fn expand_path(text: &str) -> PathBuf {
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

pub(super) fn set_global_path(
    config: &mut Config,
    field: ConfigField,
    path: PathBuf,
) -> Result<()> {
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
        ConfigField::InstancesDir => config.instances_dir = path,
        ConfigField::TemplatesDir => config.templates_dir = path,
        ConfigField::ProfilesDir => config.profiles_dir = path,
        _ => bail!("{} is not a path", field.label()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::form::ConfigForm;
    use crate::config::form::test_support::{global_config, select};

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
    fn invalid_endpoints_keep_the_editor_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut form =
            ConfigForm::global(global_config(dir.path()), dir.path().join("config.toml"));
        form.set_page(crate::config::form::ConfigPage::LocalLlm);
        form.activate();
        if let Some(edit) = form.edit.as_mut() {
            edit.buffer = "ollama=22".into();
        }
        assert!(!form.commit_edit());
        assert!(form.error.is_some());
        assert!(form.is_editing());
    }
}
