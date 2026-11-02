//! Profile payload types: the on-disk TOML schema, the exported share and the
//! reports returned by the skill sync commands.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Harness, safe_package_name};

use super::PROFILE_VERSION;
use super::template::default_skills;
use super::validate::{validate_env, validate_guest_path, validate_mode, validate_source};

/// A file or directory copied from the host into the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileFile {
    /// Host path. `~` expands to the user's home directory.
    pub source: PathBuf,
    /// Absolute guest path. Only user/config locations are accepted.
    pub target: String,
    /// Guest mode such as `"0600"`. Defaults are chosen from the source type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// A host skill directory copied into the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSkill {
    /// Host skill directory. `~` expands to the user's home directory.
    pub source: PathBuf,
    /// Absolute guest directory. The guest target links to the per-instance
    /// copy, never to the host directory.
    pub target: String,
    /// Permit `hegn skills sync-back` to copy guest changes to the host.
    #[serde(default = "default_true")]
    pub sync_back: bool,
}

fn default_true() -> bool {
    true
}

/// A per-harness guest profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub version: u32,
    /// Extra apt packages installed on first boot, on top of the harness preset.
    pub packages: Vec<String>,
    /// Host files copied into the guest on every start.
    pub files: Vec<ProfileFile>,
    /// Host skill directories copied into the guest.
    pub skills: Vec<ProfileSkill>,
    /// Environment variables exported to login shells and the agent process.
    pub environment: BTreeMap<String, String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: PROFILE_VERSION,
            packages: Vec::new(),
            files: Vec::new(),
            skills: Vec::new(),
            environment: BTreeMap::new(),
        }
    }
}

impl Profile {
    /// Default profile for a harness. Only skill locations are pre-filled;
    /// configuration files stay commented out in the generated templates so a
    /// missing file never breaks a start.
    pub fn for_harness(harness: Harness) -> Self {
        Self {
            skills: default_skills(harness),
            ..Self::default()
        }
    }

    /// Validate every value before it reaches a root-owned guest script.
    pub fn validate(&self) -> Result<()> {
        if self.version != PROFILE_VERSION {
            bail!(
                "unsupported profile version {} (expected {PROFILE_VERSION})",
                self.version
            );
        }
        for package in &self.packages {
            if !safe_package_name(package) {
                bail!("invalid package name in profile: {package:?}");
            }
        }
        for file in &self.files {
            validate_source(&file.source)?;
            validate_guest_path(&file.target)?;
            if let Some(mode) = &file.mode {
                validate_mode(mode)?;
            }
        }
        for skill in &self.skills {
            validate_source(&skill.source)?;
            validate_guest_path(&skill.target)?;
        }
        for (name, value) in &self.environment {
            validate_env(name, value)?;
        }
        Ok(())
    }

    /// A stable digest of everything that changes the guest-facing payload.
    pub(super) fn manifest_id(&self, manifest: &str, env: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(PROFILE_VERSION.to_le_bytes());
        hasher.update(manifest.as_bytes());
        hasher.update(env.as_bytes());
        hex::encode(hasher.finalize())
    }
}

/// The exported profile share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileShare {
    /// Host directory exported through a second `virtiofsd`.
    pub bundle_dir: PathBuf,
    /// Hash of the manifest and environment file, used by the cloud-init id.
    pub manifest_id: String,
}

/// Result of preparing a profile for an instance start.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProfilePlan {
    /// Packages contributed by the profile.
    pub packages: Vec<String>,
    /// Share to attach, if the profile actually carries files, skills or env.
    pub share: Option<ProfileShare>,
    /// Non-fatal problems (missing optional sources, skipped symlinks, …).
    pub warnings: Vec<String>,
}

/// Report from `hegn skills sync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    pub instance: String,
    pub harness: Harness,
    pub sources: usize,
    pub copied_files: usize,
    pub warnings: Vec<String>,
}

/// Report from `hegn skills sync-back`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncBackReport {
    pub instance: String,
    pub harness: Harness,
    pub copied_files: usize,
    pub skipped_files: usize,
    pub dry_run: bool,
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_id_changes_with_the_payload() {
        let profile = Profile::default();
        let a = profile.manifest_id("f\t0\t/x\t0600\n", "");
        let b = profile.manifest_id("f\t1\t/x\t0600\n", "");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }
}
