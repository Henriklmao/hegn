//! Profile validation and path expansion.
//!
//! Every value is checked before it reaches a root-owned guest script. Guest
//! targets are restricted to user and configuration locations, and sources may
//! not point at the filesystem root or the user's whole home directory.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

use super::{MAX_ENV_VALUE, MAX_TARGET_LEN};

/// Expand a leading `~` without touching the rest of the path.
pub fn expand_home(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
        home.join(rest)
    } else {
        path.to_path_buf()
    }
}

pub(super) fn validate_source(source: &Path) -> Result<()> {
    let expanded = expand_home(source);
    if expanded.as_os_str().is_empty() {
        bail!("profile source path cannot be empty");
    }
    if expanded == Path::new("/") {
        bail!("profile source cannot be the filesystem root");
    }
    if let Ok(canonical) = expanded.canonicalize()
        && canonical == Path::new("/")
    {
        bail!(
            "profile source {} resolves to the filesystem root",
            source.display()
        );
    }
    if let (Ok(canonical), Some(home)) = (expanded.canonicalize(), dirs::home_dir())
        && canonical == home
    {
        bail!(
            "profile source {} is the home directory; point it at a config subdirectory",
            source.display()
        );
    }
    if !expanded.is_absolute() {
        bail!(
            "profile source {} must be absolute or start with ~",
            source.display()
        );
    }
    if expanded.to_string_lossy().contains('\0') {
        bail!("profile source path contains a NUL byte");
    }
    Ok(())
}

pub(super) fn validate_guest_path(target: &str) -> Result<()> {
    if target.len() > MAX_TARGET_LEN {
        bail!("guest target {target:?} is too long");
    }
    if !target.starts_with('/') || target == "/" {
        bail!("guest target {target:?} must be an absolute path below /");
    }
    if target
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.')))
    {
        bail!("guest target {target:?} contains unsupported characters");
    }
    if target.contains("//") {
        bail!("guest target {target:?} contains an empty path component");
    }
    if target.ends_with('/') {
        bail!("guest target {target:?} must not end with a slash");
    }
    for component in Path::new(target).components() {
        match component {
            Component::RootDir | Component::Normal(_) => {}
            _ => bail!("guest target {target:?} contains '.' or '..'"),
        }
    }
    const ALLOWED_ROOTS: &[&str] = &[
        "/root/",
        "/home/",
        "/etc/",
        "/usr/local/",
        "/opt/",
        "/workspace/",
    ];
    if !ALLOWED_ROOTS.iter().any(|root| target.starts_with(root)) {
        bail!(
            "guest target {target:?} must stay under /root, /home, /etc, /usr/local, /opt or /workspace"
        );
    }
    Ok(())
}

pub(super) fn validate_mode(mode: &str) -> Result<()> {
    if !(3..=4).contains(&mode.len()) || !mode.chars().all(|c| matches!(c, '0'..='7')) {
        bail!("profile mode {mode:?} must be an octal mode such as \"0600\"");
    }
    Ok(())
}

pub(super) fn validate_env(name: &str, value: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("invalid environment variable name {name:?}");
    }
    if value.len() > MAX_ENV_VALUE {
        bail!("environment value for {name} is too long");
    }
    if value.contains(['\0', '\n', '\r']) {
        bail!("environment value for {name} must be a single line without NUL bytes");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::agent::profile::model::{Profile, ProfileFile};
    use crate::agent::profile::store::shell_quote;

    #[test]
    fn rejects_unsafe_targets_and_packages() {
        let mut profile = Profile::default();
        profile.packages = vec!["git;reboot".into()];
        assert!(profile.validate().is_err());
        profile.packages.clear();
        profile.files = vec![ProfileFile {
            source: PathBuf::from("~/.config/x"),
            target: "/usr/bin/x".into(),
            mode: None,
        }];
        assert!(profile.validate().is_err());
        profile.files[0].target = "/root/.config/x".into();
        assert!(profile.validate().is_ok());
        profile.files[0].source = PathBuf::from("relative");
        assert!(profile.validate().is_err());
        profile.files[0].source = PathBuf::from("/");
        assert!(
            profile.validate().is_err(),
            "the filesystem root is not a profile source"
        );
    }

    #[test]
    fn rejects_unsafe_env_values() {
        let mut profile = Profile::default();
        profile
            .environment
            .insert("OPENAI_API_KEY".into(), "line\nbreak".into());
        assert!(profile.validate().is_err());
        profile.environment.insert("BAD-NAME".into(), "x".into());
        assert!(profile.validate().is_err());
        profile.environment.clear();
        profile
            .environment
            .insert("OPENAI_API_KEY".into(), "sk-'quoted'".into());
        assert!(profile.validate().is_ok());
        assert!(shell_quote("sk-'quoted'").contains(r#"'\''"#));
    }
}
