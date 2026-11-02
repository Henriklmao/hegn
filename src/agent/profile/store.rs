//! Filesystem store for profile templates and the per-instance bundle layout.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Harness};
use crate::vm::instance_config::InstanceConfig;

use super::fs::{ensure_real_dir, remove_path, secure_dir, write_private};
use super::model::Profile;
use super::template::template;
use super::{BUNDLE_DIR, ENV_FILE, FILES_DIR, LOCAL_STATE, MANIFEST, SKILLS_DIR};

/// Filesystem store for profile templates.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    pub fn new(config: &Config) -> Self {
        Self {
            dir: config.profiles_dir.clone(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, harness: Harness) -> PathBuf {
        self.dir.join(format!("{}.toml", harness.slug()))
    }

    /// Write a template for every agent harness that does not have one yet.
    ///
    /// Existing files are never touched: profiles hold user secrets.
    pub fn write_templates(&self) -> Result<Vec<PathBuf>> {
        secure_dir(&self.dir)?;
        let mut written = Vec::new();
        for harness in Harness::ALL {
            if harness == Harness::Shell {
                continue;
            }
            let path = self.path(harness);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    continue;
                }
                Ok(_) => bail!(
                    "profile path {} exists but is not a regular file",
                    path.display()
                ),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to inspect {}", path.display()));
                }
            }
            write_private(&path, template(harness).as_bytes())
                .with_context(|| format!("failed to write {}", path.display()))?;
            written.push(path);
        }
        Ok(written)
    }

    /// Whether at least one agent harness has no profile template yet.
    pub fn needs_templates(&self) -> bool {
        Harness::ALL
            .into_iter()
            .filter(|harness| *harness != Harness::Shell)
            .any(|harness| {
                !fs::symlink_metadata(self.path(harness))
                    .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                    .unwrap_or(false)
            })
    }

    /// Load a profile. A missing file yields the harness defaults, so upgrades
    /// keep working before the user runs `hegn config init`.
    pub fn load(&self, harness: Harness) -> Result<Profile> {
        let path = self.path(harness);
        if !path.exists() {
            return Ok(Profile::for_harness(harness));
        }
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && metadata.file_type().is_symlink()
        {
            bail!("refusing symlinked profile {}", path.display());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read profile {}", path.display()))?;
        let profile: Profile = toml::from_str(&raw)
            .with_context(|| format!("failed to parse profile {}", path.display()))?;
        profile
            .validate()
            .with_context(|| format!("invalid profile {}", path.display()))?;
        Ok(profile)
    }
}

pub(super) struct ProfileLayout {
    root: PathBuf,
    pub(super) bundle_dir: PathBuf,
}

impl ProfileLayout {
    pub(super) fn new(config: &Config, instance: &InstanceConfig) -> Result<Self> {
        let root = instance.dir(&config.instances_dir).join("profile");
        let bundle_dir = root.join(BUNDLE_DIR);
        secure_dir(&root)?;
        secure_dir(&bundle_dir)?;
        Ok(Self { root, bundle_dir })
    }

    pub(super) fn files_dir(&self) -> PathBuf {
        self.bundle_dir.join(FILES_DIR)
    }

    pub(super) fn skills_dir(&self) -> PathBuf {
        self.bundle_dir.join(SKILLS_DIR)
    }

    pub(super) fn reset_files(&self) -> Result<()> {
        let files = self.files_dir();
        remove_path(&files)?;
        ensure_real_dir(&files)
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(LOCAL_STATE)
    }

    pub(super) fn load_state(&self) -> Option<ProfileState> {
        let raw = fs::read_to_string(self.state_path()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub(super) fn save(&self, state: &ProfileState, profile: &Profile) -> Result<()> {
        ensure_real_dir(&self.files_dir())?;
        ensure_real_dir(&self.skills_dir())?;
        let manifest = self.manifest_contents(state)?;
        let env = self.env_contents(profile);
        write_private(&self.bundle_dir.join(MANIFEST), manifest.as_bytes())?;
        write_private(&self.bundle_dir.join(ENV_FILE), env.as_bytes())?;
        let raw = serde_json::to_vec_pretty(state)?;
        write_private(&self.state_path(), &raw)?;
        Ok(())
    }

    pub(super) fn manifest_contents(&self, state: &ProfileState) -> Result<String> {
        let mut manifest = String::new();
        for file in &state.files {
            let bundle = self.files_dir().join(&file.bundle);
            if !bundle.exists() {
                continue;
            }
            manifest.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                file.kind, file.bundle, file.target, file.mode
            ));
        }
        for skill in &state.skills {
            let bundle = self.skills_dir().join(&skill.bundle);
            if !bundle.exists() {
                continue;
            }
            manifest.push_str(&format!("s\t{}\t{}\t0700\n", skill.bundle, skill.target));
        }
        Ok(manifest)
    }

    pub(super) fn env_contents(&self, profile: &Profile) -> String {
        if profile.environment.is_empty() {
            return String::new();
        }
        let mut env = String::from("# Generated by hegn from the harness profile.\n");
        for (name, value) in &profile.environment {
            env.push_str("export ");
            env.push_str(name);
            env.push('=');
            env.push_str(&shell_quote(value));
            env.push('\n');
        }
        env
    }
}

pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ProfileState {
    pub(super) version: u32,
    pub(super) harness: Harness,
    pub(super) files: Vec<StoredFile>,
    pub(super) skills: Vec<StoredSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredFile {
    pub(super) source: PathBuf,
    pub(super) target: String,
    pub(super) mode: String,
    pub(super) bundle: String,
    pub(super) kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredSkill {
    pub(super) source: PathBuf,
    pub(super) target: String,
    pub(super) bundle: String,
    pub(super) sync_back: bool,
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.profiles_dir = dir.join("profiles");
        config.instances_dir = dir.join("instances");
        config.projects_root = dir.join("projects");
        fs::create_dir_all(&config.profiles_dir).unwrap();
        config
    }

    #[test]
    fn writes_private_templates_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let store = ProfileStore::new(&config);
        let written = store.write_templates().unwrap();
        assert_eq!(written.len(), Harness::ALL.len() - 1);
        for path in &written {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // A user edit must survive a second `config init`.
        let opencode = store.path(Harness::Opencode);
        fs::write(&opencode, b"version = 1\npackages = [\"jq\"]\n").unwrap();
        assert!(store.write_templates().unwrap().is_empty());
        assert_eq!(
            fs::read(&opencode).unwrap(),
            b"version = 1\npackages = [\"jq\"]\n"
        );
    }

    #[test]
    fn refuses_symlinked_profile_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let store = ProfileStore::new(&config);
        let target = dir.path().join("real-profile.toml");
        fs::write(&target, b"version = 1\n").unwrap();
        let link = store.path(Harness::Opencode);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = store.load(Harness::Opencode).unwrap_err().to_string();
        assert!(err.contains("refusing symlinked profile"), "{err}");
        let err = store.write_templates().unwrap_err().to_string();
        assert!(
            err.contains("not a regular file") || err.contains("refusing symlinked"),
            "{err}"
        );
        assert_eq!(fs::read(&target).unwrap(), b"version = 1\n");
    }

    #[test]
    fn load_rejects_profiles_with_unsafe_values() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let store = ProfileStore::new(&config);
        fs::create_dir_all(&config.profiles_dir).unwrap();
        fs::write(
            store.path(Harness::Opencode),
            b"version = 1\npackages = [\"git;reboot\"]\n",
        )
        .unwrap();
        let err = store.load(Harness::Opencode).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("invalid profile"), "{text}");
        assert!(text.contains("invalid package name"), "{text}");
    }
}
