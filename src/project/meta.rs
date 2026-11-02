//! Per-project `hegn.toml` metadata: reading, validated writing and name
//! validation for the create flow.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::model::ProjectMeta;

pub const PROJECT_META_FILE: &str = "hegn.toml";
/// Older hegn versions wrote the project settings to `.hegn.toml`.
const LEGACY_PROJECT_META_FILE: &str = ".hegn.toml";

pub fn read_meta(path: &Path) -> ProjectMeta {
    for name in [PROJECT_META_FILE, LEGACY_PROJECT_META_FILE] {
        if let Some(meta) = fs::read_to_string(path.join(name))
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
        {
            return meta;
        }
    }
    ProjectMeta::default()
}

/// Read metadata without hiding a malformed file.
///
/// [`read_meta`] falls back to defaults so a broken `hegn.toml` cannot block
/// the rest of the TUI. Callers that rewrite the file use this variant instead:
/// silently replacing a hand-written config because it failed to parse would
/// destroy the user's settings.
pub fn try_read_meta(path: &Path) -> Result<ProjectMeta> {
    for name in [PROJECT_META_FILE, LEGACY_PROJECT_META_FILE] {
        let file = path.join(name);
        match fs::read_to_string(&file) {
            Ok(raw) => {
                return toml::from_str(&raw)
                    .with_context(|| format!("failed to parse {}", file.display()));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", file.display()));
            }
        }
    }
    Ok(ProjectMeta::default())
}

pub fn write_meta(path: &Path, meta: &ProjectMeta) -> Result<()> {
    let raw = toml::to_string_pretty(meta)?;
    let target = path.join(PROJECT_META_FILE);
    // Namespace `hegn.toml` is shared with the guest through virtiofs, so a
    // compromised agent can plant a symlink. Write a fresh sibling with
    // O_NOFOLLOW and rename it over the name: the rename replaces a planted
    // link instead of following it, and a crash never leaves a partial file.
    let tmp = target.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(raw.as_bytes())
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()?;
        fs::rename(&tmp, &target)
            .with_context(|| format!("failed to replace {}", target.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Validated project name (prevents path escapes in the create flow).
pub fn validate_project_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("project name cannot be empty");
    }
    if name.len() > 80 {
        bail!("project name is too long");
    }
    if name.starts_with('.') || name.contains('/') || name.contains('\\') {
        bail!("project name must not contain path separators or start with '.'");
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
    {
        bail!("project name may only contain letters, digits, '-', '_', '.' and spaces");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        assert!(validate_project_name("my-project").is_ok());
        assert!(validate_project_name("../escape").is_err());
        assert!(validate_project_name(".hidden").is_err());
        assert!(validate_project_name("").is_err());
        assert!(validate_project_name("bad;rm").is_err());
    }

    #[test]
    fn project_meta_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let meta = ProjectMeta {
            harness: Some(crate::config::Harness::Codex),
            packages: vec!["nodejs".into(), "git".into()],
            template: Some("rust".into()),
            launch: Some("review".into()),
            integration: Some(crate::agent::launch::Integration::Tmux),
            auto_start: Some(false),
        };
        write_meta(dir.path(), &meta).unwrap();
        let loaded = read_meta(dir.path());
        assert!(dir.path().join("hegn.toml").is_file());
        assert_eq!(loaded.harness, Some(crate::config::Harness::Codex));
        assert_eq!(loaded.packages.len(), 2);
        assert_eq!(loaded.launch.as_deref(), Some("review"));
        assert_eq!(
            loaded.integration,
            Some(crate::agent::launch::Integration::Tmux)
        );
        assert_eq!(loaded.auto_start, Some(false));
        assert_eq!(read_meta(Path::new("/missing")).packages.len(), 0);
    }

    #[test]
    fn project_meta_write_replaces_a_planted_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"keep me").unwrap();
        symlink(&victim, project.join(PROJECT_META_FILE)).unwrap();

        write_meta(&project, &ProjectMeta::default()).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
        let meta = std::fs::symlink_metadata(project.join(PROJECT_META_FILE)).unwrap();
        assert!(meta.file_type().is_file());
    }

    #[test]
    fn malformed_meta_is_lenient_at_read_time_but_rejected_for_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PROJECT_META_FILE), "harness = [\n").unwrap();

        assert_eq!(read_meta(dir.path()).harness, None);
        let err = try_read_meta(dir.path()).unwrap_err().to_string();
        assert!(err.contains("failed to parse"), "{err}");
    }

    #[test]
    fn project_meta_reads_legacy_dotfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hegn.toml"),
            "harness = \"codex\"\npackages = [\"git\"]\n",
        )
        .unwrap();
        let loaded = read_meta(dir.path());
        assert_eq!(loaded.harness, Some(crate::config::Harness::Codex));
        assert_eq!(loaded.packages, vec!["git"]);
    }
}
