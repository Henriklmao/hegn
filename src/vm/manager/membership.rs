//! The `run/membership.json` manifest written at every successful boot.
//!
//! The manifest records exactly which host directories a running VM exported,
//! so `stop` can terminate every daemon, `attach` can warn when the registry
//! changed while the VM was running, and a future restore can reject a
//! snapshot whose device tree no longer matches.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::project::ProjectKind;

use super::plan::BootPlan;

pub const MEMBERSHIP_FILE: &str = "membership.json";
pub const MEMBERSHIP_VERSION: u32 = 1;

/// One exported project as recorded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipMember {
    pub name: String,
    pub path: PathBuf,
    pub tag: String,
    pub guest_path: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub kind: ProjectKind,
}

/// The exported membership of one running VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipManifest {
    pub version: u32,
    pub vm: String,
    pub primary: String,
    pub members: Vec<MembershipMember>,
}

impl MembershipManifest {
    pub fn from_plan(plan: &BootPlan) -> Self {
        Self {
            version: MEMBERSHIP_VERSION,
            vm: plan.vm.clone(),
            primary: plan.primary.clone(),
            members: plan
                .members
                .iter()
                .map(|member| MembershipMember {
                    name: member.name.clone(),
                    path: member.path.clone(),
                    tag: member.tag.clone(),
                    guest_path: member.guest_path.clone(),
                    read_only: member.read_only,
                    kind: member.kind,
                })
                .collect(),
        }
    }

    /// Read a manifest, refusing symlinks and foreign versions.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("refusing symlinked membership manifest {}", path.display());
            }
            Ok(metadata) if !metadata.is_file() => {
                bail!(
                    "membership manifest {} is not a regular file",
                    path.display()
                );
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("no VM membership manifest at {}", path.display());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to inspect {}", path.display()));
            }
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read membership manifest {}", path.display()))?;
        let manifest: Self = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse membership manifest {}", path.display()))?;
        if manifest.version != MEMBERSHIP_VERSION {
            bail!(
                "unsupported membership manifest version {} in {} (expected {MEMBERSHIP_VERSION})",
                manifest.version,
                path.display()
            );
        }
        Ok(manifest)
    }

    /// Write the manifest atomically with `0600`, never following a symlink.
    pub fn save(&self, path: &Path) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        let parent = path
            .parent()
            .with_context(|| format!("{} has no parent directory", path.display()))?;
        let tmp = parent.join(format!(
            ".{MEMBERSHIP_FILE}.tmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        let _ = fs::remove_file(&tmp);
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&tmp)
                .with_context(|| format!("failed to write {}", tmp.display()))?;
            file.write_all(raw.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, path)
                .with_context(|| format!("failed to install {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    pub fn paths(&self) -> Vec<&Path> {
        self.members
            .iter()
            .map(|member| member.path.as_path())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> MembershipManifest {
        MembershipManifest {
            version: MEMBERSHIP_VERSION,
            vm: "rust".into(),
            primary: "parser".into(),
            members: vec![
                MembershipMember {
                    name: "parser".into(),
                    path: PathBuf::from("/projects/parser"),
                    tag: "hegn-prj-0".into(),
                    guest_path: "/projects/parser".into(),
                    read_only: false,
                    kind: ProjectKind::External,
                },
                MembershipMember {
                    name: "cli".into(),
                    path: PathBuf::from("/projects/cli"),
                    tag: "hegn-prj-1".into(),
                    guest_path: "/projects/cli".into(),
                    read_only: true,
                    kind: ProjectKind::External,
                },
            ],
        }
    }

    #[test]
    fn manifest_round_trips_and_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MEMBERSHIP_FILE);
        manifest().save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(MembershipManifest::load(&path).unwrap(), manifest());

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\": 1"), "{raw}");
        assert!(raw.contains("\"vm\": \"rust\""), "{raw}");
        assert!(raw.contains("\"read_only\": true"), "{raw}");
    }

    #[test]
    fn manifest_refuses_symlinks_and_wrong_versions() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        fs::write(&target, serde_json::to_string(&manifest()).unwrap()).unwrap();
        let link = dir.path().join(MEMBERSHIP_FILE);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = MembershipManifest::load(&link).unwrap_err().to_string();
        assert!(err.contains("symlinked"), "{err}");

        let other = dir.path().join("other.json");
        fs::write(
            &other,
            r#"{"version":2,"vm":"rust","primary":"a","members":[]}"#,
        )
        .unwrap();
        let err = MembershipManifest::load(&other).unwrap_err().to_string();
        assert!(err.contains("unsupported"), "{err}");
    }

    #[test]
    fn missing_manifest_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = MembershipManifest::load(&dir.path().join(MEMBERSHIP_FILE))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no VM membership manifest"), "{err}");
    }
}
