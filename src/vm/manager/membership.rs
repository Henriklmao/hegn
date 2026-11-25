//! The `run/membership.json` manifest written at every successful boot.
//!
//! The manifest records exactly which host directories a running VM exported,
//! so `stop` can terminate every daemon, `attach` can warn when the registry
//! changed while the VM was running, and a future restore can reject a
//! snapshot whose device tree no longer matches.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::project::ProjectKind;
use crate::security::GuestWorkspacePolicy;

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

    /// Derive the guest workspace roots from the recorded membership.
    ///
    /// `base` is the configured `guest_projects_base` the VM was booted with.
    /// Each recorded `guest_path` must be exactly `<base>/<slug>`; a manifest
    /// written under a different base is rejected instead of classifying a
    /// live VM against paths it does not mount. `/workspace` always follows the
    /// recorded primary member and inherits its read-only flag.
    ///
    /// The manifest does not store slugs separately, so the slug is the single
    /// component below `base`. Nested or foreign paths and an empty or unknown
    /// primary are hard errors: a running VM must never fall back to a policy
    /// that guards the wrong roots.
    pub fn workspace_policy(&self, base: &str) -> Result<GuestWorkspacePolicy> {
        if self.members.is_empty() {
            bail!(
                "membership manifest for VM '{}' records no exported members",
                self.vm
            );
        }
        let mut policy = GuestWorkspacePolicy::new(base).with_context(|| {
            format!(
                "invalid guest projects base {base:?} for VM '{}' membership",
                self.vm
            )
        })?;
        let base = policy.base().to_string();
        let prefix = format!("{base}/");
        let mut names = HashSet::new();
        let mut primary_slug = None;
        for member in &self.members {
            if !names.insert(member.name.as_str()) {
                bail!(
                    "membership manifest for VM '{}' records duplicate project '{}'",
                    self.vm,
                    member.name
                );
            }
            let slug = member
                .guest_path
                .strip_prefix(&prefix)
                .filter(|slug| !slug.is_empty() && !slug.contains('/'))
                .with_context(|| {
                    format!(
                        "membership member '{}' guest path {} is not a direct mount below {base}",
                        member.name, member.guest_path
                    )
                })?;
            policy.add_member(slug, member.read_only).with_context(|| {
                format!(
                    "invalid guest workspace membership for project '{}'",
                    member.name
                )
            })?;
            if member.name == self.primary {
                primary_slug = Some(slug.to_string());
            }
        }
        let primary = primary_slug.with_context(|| {
            format!(
                "membership manifest for VM '{}' names primary project '{}' which is not an \
                 exported member",
                self.vm, self.primary
            )
        })?;
        policy.set_primary(&primary)?;
        Ok(policy)
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

    #[test]
    fn workspace_policy_matches_recorded_members_and_primary() {
        let policy = manifest().workspace_policy("/projects").unwrap();
        assert_eq!(policy.base(), "/projects");
        assert_eq!(policy.primary(), Some("parser"));
        assert_eq!(
            policy.roots(),
            vec!["/projects/cli", "/projects/parser", "/workspace"]
        );
        assert!(policy.contains("/projects/parser/src"));
        assert!(policy.contains("/projects/cli/.env"));
        assert!(policy.is_read_only_root("/projects/cli"));
        assert!(!policy.is_read_only_root("/projects/parser"));
        assert!(!policy.is_read_only_root("/workspace"));
        assert!(!policy.contains("/projects/parser-other"));
        assert!(!policy.contains("/projects/parser2"));
        assert!(!policy.contains("/projects"));
    }

    #[test]
    fn workspace_policy_follows_a_read_only_primary() {
        let mut manifest = manifest();
        manifest.primary = "cli".into();
        let policy = manifest.workspace_policy("/projects").unwrap();
        assert_eq!(policy.primary(), Some("cli"));
        assert!(policy.is_read_only_root("/workspace"));
        assert!(policy.is_read_only_root("/workspace/src"));
    }

    #[test]
    fn workspace_policy_rejects_an_empty_manifest() {
        let mut manifest = manifest();
        manifest.members.clear();
        let err = manifest
            .workspace_policy("/projects")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no exported members"), "{err}");
    }

    #[test]
    fn workspace_policy_rejects_an_unknown_or_legacy_primary() {
        let mut unknown = manifest();
        unknown.primary = "missing".into();
        let err = unknown
            .workspace_policy("/projects")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not an exported member"), "{err}");
        assert!(err.contains("missing"), "{err}");

        let mut legacy = manifest();
        legacy.primary = String::new();
        let err = legacy
            .workspace_policy("/projects")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not an exported member"), "{err}");
    }

    #[test]
    fn workspace_policy_rejects_foreign_nested_and_duplicate_members() {
        for guest_path in [
            "/elsewhere/parser",
            "/projects",
            "/projects/",
            "/projects/nested/parser",
            "/projects/parser/..",
        ] {
            let mut manifest = manifest();
            manifest.members[0].guest_path = guest_path.into();
            let err = manifest
                .workspace_policy("/projects")
                .unwrap_err()
                .to_string();
            assert!(err.contains("not a direct mount"), "{guest_path:?}: {err}");
        }

        let mut duplicate_slug = manifest();
        duplicate_slug.members[1].guest_path = "/projects/parser".into();
        let err = format!(
            "{:#}",
            duplicate_slug.workspace_policy("/projects").unwrap_err()
        );
        assert!(
            err.contains("duplicate") || err.contains("Duplicate"),
            "{err}"
        );

        let mut duplicate_name = manifest();
        duplicate_name.members[1].name = "parser".into();
        let err = duplicate_name
            .workspace_policy("/projects")
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate project"), "{err}");
    }

    #[test]
    fn workspace_policy_rejects_a_manifest_written_under_another_base() {
        let err = manifest()
            .workspace_policy("/legacy-workspace")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a direct mount"), "{err}");
    }
}
