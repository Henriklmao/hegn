//! Materialize the profile bundle for an instance and sync skills in both
//! directions.
//!
//! Skills are always copied: the guest target links to the per-instance copy,
//! never to the host directory. Symlinks and special files are skipped in both
//! directions so a poisoned guest cannot redirect host writes.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::vm::instance_config::InstanceConfig;

use super::PROFILE_VERSION;
use super::fs::{copy_file_private, ensure_real_dir, remove_path, secure_dir};
use super::model::{ProfilePlan, ProfileShare, SyncBackReport, SyncReport};
use super::store::{ProfileLayout, ProfileState, ProfileStore, StoredFile, StoredSkill};
use super::validate::expand_home;

/// What `prepare` should do with existing per-instance skill copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refresh {
    /// Re-copy configuration files, keep existing skill copies.
    Start,
    /// Merge host changes into the existing skill copies.
    Skills,
}

/// Build the profile payload for an instance start.
pub fn prepare_for_start(config: &Config, instance: &InstanceConfig) -> Result<ProfilePlan> {
    materialize(config, instance, Refresh::Start)
}

/// Copy host skill changes into an existing per-instance bundle.
///
/// Host files win for files they contain; guest-only files are preserved. The
/// guest target is a link to this bundle, so a running VM sees the new files
/// immediately.
pub fn sync_skills(config: &Config, instance: &InstanceConfig) -> Result<SyncReport> {
    let store = ProfileStore::new(config);
    let profile = store.load(instance.harness)?;
    let mut report = SyncReport {
        instance: instance.name.clone(),
        harness: instance.harness,
        sources: 0,
        copied_files: 0,
        warnings: Vec::new(),
    };
    let layout = ProfileLayout::new(config, instance)?;
    let mut state = layout
        .load_state()
        .filter(|state| state.harness == instance.harness)
        .unwrap_or_else(|| ProfileState {
            version: PROFILE_VERSION,
            harness: instance.harness,
            files: Vec::new(),
            skills: Vec::new(),
        });
    let previous_skills = state.skills.clone();
    state.skills.clear();
    for skill in profile.skills.iter() {
        let source = expand_home(&skill.source);
        if !source.is_dir() {
            report
                .warnings
                .push(format!("skill source {} is missing", source.display()));
            let bundle = skill_bundle_id(&source, &skill.target);
            if layout.skills_dir().join(&bundle).exists()
                && let Some(previous) = previous_skills.iter().find(|s| s.bundle == bundle)
            {
                state.skills.push(previous.clone());
            }
            continue;
        }
        report.sources += 1;
        let bundle = skill_bundle_id(&source, &skill.target);
        let dest = layout.skills_dir().join(&bundle);
        let (copied, mut warnings) = merge_tree(&source, &dest, false)?;
        report.copied_files += copied;
        report.warnings.append(&mut warnings);
        upsert_skill(&mut state, &source, &skill.target, &bundle, skill.sync_back);
    }
    layout.save(&state, &profile)?;
    Ok(report)
}

/// Copy changed and new skill files from the instance bundle back to the host.
///
/// Only regular files are copied. Symlinks and special files are skipped, so a
/// poisoned guest cannot redirect a host write outside the configured skill
/// directory.
pub fn sync_skills_back(
    config: &Config,
    instance: &InstanceConfig,
    dry_run: bool,
) -> Result<SyncBackReport> {
    let layout = ProfileLayout::new(config, instance)?;
    let state = layout.load_state().with_context(|| {
        format!(
            "instance '{}' has no profile copy yet; start it once or run `hegn skills sync {}`",
            instance.name, instance.name
        )
    })?;
    if state.harness != instance.harness {
        bail!(
            "instance '{}' has a profile copy for harness '{}' but is configured for '{}'; \
             start it once to rebuild the profile",
            instance.name,
            state.harness.label(),
            instance.harness.label()
        );
    }
    let mut report = SyncBackReport {
        instance: instance.name.clone(),
        harness: state.harness,
        copied_files: 0,
        skipped_files: 0,
        dry_run,
        warnings: Vec::new(),
    };
    for skill in &state.skills {
        if !skill.sync_back {
            continue;
        }
        let bundle = layout.skills_dir().join(&skill.bundle);
        let real_dir = fs::symlink_metadata(&bundle)
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false);
        if !real_dir {
            report.warnings.push(format!(
                "refusing skill copy {} (not a real directory)",
                bundle.display()
            ));
            continue;
        }
        let source = expand_home(&skill.source);
        copy_back_tree(&bundle, &source, dry_run, &mut report)?;
    }
    Ok(report)
}

fn copy_back_tree(
    bundle: &Path,
    destination: &Path,
    dry_run: bool,
    report: &mut SyncBackReport,
) -> Result<()> {
    for entry in
        fs::read_dir(bundle).with_context(|| format!("failed to read {}", bundle.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            report.warnings.push(format!(
                "skipped symlink {} (only regular files are synced back)",
                from.display()
            ));
            report.skipped_files += 1;
        } else if metadata.is_dir() {
            if !dry_run {
                secure_dir(&to)?;
            }
            copy_back_tree(&from, &to, dry_run, report)?;
        } else if metadata.is_file() {
            if let Ok(existing) = fs::symlink_metadata(&to)
                && existing.file_type().is_symlink()
            {
                report
                    .warnings
                    .push(format!("refusing to replace host symlink {}", to.display()));
                report.skipped_files += 1;
                continue;
            }
            if same_contents(&from, &to)? {
                report.skipped_files += 1;
                continue;
            }
            report.copied_files += 1;
            if dry_run {
                continue;
            }
            if let Some(parent) = to.parent() {
                secure_dir(parent)?;
            }
            copy_file_private(&from, &to)?;
            let mode = metadata.permissions().mode() & 0o755;
            let mode = if mode == 0 { 0o600 } else { mode };
            let _ = fs::set_permissions(&to, fs::Permissions::from_mode(mode));
        } else {
            report.warnings.push(format!(
                "skipped special file {} (only regular files are synced back)",
                from.display()
            ));
            report.skipped_files += 1;
        }
    }
    Ok(())
}

fn materialize(
    config: &Config,
    instance: &InstanceConfig,
    refresh: Refresh,
) -> Result<ProfilePlan> {
    let store = ProfileStore::new(config);
    let profile = store.load(instance.harness)?;
    let layout = ProfileLayout::new(config, instance)?;
    let previous = layout
        .load_state()
        .filter(|state| state.harness == instance.harness);
    let mut state = if refresh == Refresh::Skills {
        previous.clone()
    } else {
        None
    }
    .unwrap_or_else(|| ProfileState {
        version: PROFILE_VERSION,
        harness: instance.harness,
        files: Vec::new(),
        skills: Vec::new(),
    });
    let mut plan = ProfilePlan {
        packages: profile.packages.clone(),
        share: None,
        warnings: Vec::new(),
    };

    if refresh == Refresh::Start {
        // Configuration files are refreshed from the host on every start. The
        // previous bundle is discarded so deleted files disappear from the
        // guest copy as well.
        layout.reset_files()?;
        state.files.clear();
        for (index, file) in profile.files.iter().enumerate() {
            let source = expand_home(&file.source);
            let metadata = match fs::metadata(&source) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    plan.warnings
                        .push(format!("file source {} is missing", source.display()));
                    continue;
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to inspect {}", source.display()));
                }
            };
            let bundle = index.to_string();
            let dest = layout.files_dir().join(&bundle);
            let (kind, mode) = if metadata.is_dir() {
                ("d", file.mode.clone().unwrap_or_else(|| "0700".into()))
            } else if metadata.is_file() {
                ("f", file.mode.clone().unwrap_or_else(|| "0600".into()))
            } else {
                plan.warnings.push(format!(
                    "skipped {} (only regular files and directories are copied)",
                    source.display()
                ));
                continue;
            };
            let (copied, mut warnings) = if kind == "d" {
                copy_tree(&source, &dest, false)?
            } else {
                copy_file_private(&source, &dest)?;
                (1, Vec::new())
            };
            plan.warnings.append(&mut warnings);
            if copied == 0 && kind == "d" {
                plan.warnings.push(format!(
                    "directory {} contains no regular files, skipped",
                    source.display()
                ));
                let _ = fs::remove_dir_all(&dest);
                continue;
            }
            state.files.push(StoredFile {
                source,
                target: file.target.clone(),
                mode,
                bundle,
                kind: kind.to_string(),
            });
        }
    }

    // Skills are seeded once. On later starts the per-instance copy is kept so
    // guest-created skills survive a reboot and can be synced back.
    for skill in profile.skills.iter() {
        let source = expand_home(&skill.source);
        let bundle = skill_bundle_id(&source, &skill.target);
        let dest = layout.skills_dir().join(&bundle);
        if !source.is_dir() {
            if refresh == Refresh::Skills {
                plan.warnings
                    .push(format!("skill source {} is missing", source.display()));
            }
            // Keep a previous mapping when only the host source disappeared
            // temporarily; the per-instance copy may still hold guest edits.
            if dest.exists()
                && let Some(previous) = previous
                    .as_ref()
                    .and_then(|state| state.skills.iter().find(|s| s.bundle == bundle))
            {
                state.skills.push(previous.clone());
            }
            continue;
        }
        if refresh == Refresh::Skills {
            let (copied, mut warnings) = merge_tree(&source, &dest, false)?;
            if copied > 0 {
                plan.warnings.push(format!(
                    "copied {copied} file(s) from {} into the instance skill copy",
                    source.display()
                ));
            }
            plan.warnings.append(&mut warnings);
        } else if !dest.exists() {
            let (_, mut warnings) = copy_tree(&source, &dest, false)?;
            plan.warnings.append(&mut warnings);
        }
        upsert_skill(&mut state, &source, &skill.target, &bundle, skill.sync_back);
    }

    layout.save(&state, &profile)?;
    let env = layout.env_contents(&profile);
    let manifest = layout.manifest_contents(&state)?;
    let has_payload = !state.files.is_empty() || !state.skills.is_empty() || !env.is_empty();
    if has_payload {
        plan.share = Some(ProfileShare {
            bundle_dir: layout.bundle_dir.clone(),
            manifest_id: profile.manifest_id(&manifest, &env),
        });
    }
    Ok(plan)
}

/// Stable bundle directory name for a skill source. Reordering entries in the
/// profile must not make a previous skill copy look like the new source.
fn skill_bundle_id(source: &Path, target: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(source.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(target.as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

fn upsert_skill(
    state: &mut ProfileState,
    source: &Path,
    target: &str,
    bundle: &str,
    sync_back: bool,
) {
    if let Some(existing) = state.skills.iter_mut().find(|s| s.bundle == bundle) {
        existing.source = source.to_path_buf();
        existing.target = target.to_string();
        existing.sync_back = sync_back;
        return;
    }
    state.skills.push(StoredSkill {
        source: source.to_path_buf(),
        target: target.to_string(),
        bundle: bundle.to_string(),
        sync_back,
    });
}

fn copy_tree(source: &Path, dest: &Path, merge: bool) -> Result<(usize, Vec<String>)> {
    if !merge {
        remove_path(dest)?;
    }
    ensure_real_dir(dest)?;
    let mut copied = 0;
    let mut warnings = Vec::new();
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            warnings.push(format!(
                "skipped symlink {} (profiles copy files, they never link them)",
                from.display()
            ));
        } else if metadata.is_dir() {
            let (child_copied, mut child_warnings) = copy_tree(&from, &to, merge)?;
            copied += child_copied;
            warnings.append(&mut child_warnings);
        } else if metadata.is_file() {
            copy_file_private(&from, &to)?;
            copied += 1;
        } else {
            warnings.push(format!(
                "skipped special file {} (only regular files are copied)",
                from.display()
            ));
        }
    }
    Ok((copied, warnings))
}

/// Copy host files into an existing bundle without deleting guest-only files.
fn merge_tree(source: &Path, dest: &Path, dry_run: bool) -> Result<(usize, Vec<String>)> {
    if dry_run {
        return Ok((0, Vec::new()));
    }
    if !dest.exists() {
        return copy_tree(source, dest, false);
    }
    ensure_real_dir(dest)?;
    let mut copied = 0;
    let mut warnings = Vec::new();
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            warnings.push(format!(
                "skipped symlink {} (skills are copied, never linked)",
                from.display()
            ));
        } else if metadata.is_dir() {
            let (child_copied, mut child_warnings) = merge_tree(&from, &to, false)?;
            copied += child_copied;
            warnings.append(&mut child_warnings);
        } else if metadata.is_file() {
            if same_contents(&from, &to)? {
                continue;
            }
            copy_file_private(&from, &to)?;
            copied += 1;
        } else {
            warnings.push(format!(
                "skipped special file {} (only regular files are copied)",
                from.display()
            ));
        }
    }
    Ok((copied, warnings))
}

fn same_contents(a: &Path, b: &Path) -> Result<bool> {
    let a_meta = match fs::symlink_metadata(b) {
        Ok(meta) if meta.is_file() => meta,
        _ => return Ok(false),
    };
    let b_meta = fs::metadata(a)?;
    if a_meta.len() != b_meta.len() {
        return Ok(false);
    }
    Ok(fs::read(a)? == fs::read(b)?)
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::profile::model::ProfileShare;
    use crate::agent::profile::store::ProfileStore;
    use crate::config::Harness;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.profiles_dir = dir.join("profiles");
        config.instances_dir = dir.join("instances");
        config.projects_root = dir.join("projects");
        fs::create_dir_all(&config.profiles_dir).unwrap();
        config
    }

    fn instance(config: &Config, harness: Harness) -> InstanceConfig {
        let project = config.projects_root.join("demo");
        fs::create_dir_all(&project).unwrap();
        let instance = InstanceConfig {
            name: "demo".into(),
            project: project.clone(),
            workspace: project,
            harness,
            ..InstanceConfig::default()
        };
        instance.validate().unwrap();
        instance
    }

    fn first_skill_dir(share: &ProfileShare) -> PathBuf {
        let mut entries: Vec<_> = fs::read_dir(share.bundle_dir.join("skills"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.remove(0)
    }

    #[test]
    fn prepare_copies_files_skills_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        store.write_templates().unwrap();
        let profile = format!(
            r#"version = 1
packages = ["jq"]
[[files]]
source = {file:?}
target = "/root/.config/opencode/opencode.json"
mode = "0600"
[[skills]]
source = {skills:?}
target = "/root/.config/opencode/skills"
sync_back = true
[environment]
OPENAI_API_KEY = "sk-test"
"#,
            file = dir.path().join("opencode.json").to_string_lossy(),
            skills = dir.path().join("host-skills").to_string_lossy(),
        );
        fs::write(store.path(Harness::Opencode), profile).unwrap();
        fs::write(dir.path().join("opencode.json"), b"{}").unwrap();
        fs::create_dir_all(dir.path().join("host-skills/demo")).unwrap();
        fs::write(dir.path().join("host-skills/demo/SKILL.md"), b"# demo").unwrap();

        let plan = prepare_for_start(&config, &instance).unwrap();
        assert_eq!(plan.packages, vec!["jq"]);
        let share = plan.share.expect("profile share");
        assert!(share.bundle_dir.join("manifest").is_file());
        let manifest = fs::read_to_string(share.bundle_dir.join("manifest")).unwrap();
        assert!(manifest.contains("/root/.config/opencode/opencode.json"));
        assert!(manifest.contains("/root/.config/opencode/skills"));
        let env = fs::read_to_string(share.bundle_dir.join("env.sh")).unwrap();
        assert!(env.contains("OPENAI_API_KEY='sk-test'"));
        let skills = first_skill_dir(&share);
        assert!(skills.join("demo/SKILL.md").is_file());

        // A new guest skill survives a normal restart.
        let guest_skill = skills.join("guest/SKILL.md");
        fs::create_dir_all(guest_skill.parent().unwrap()).unwrap();
        fs::write(&guest_skill, b"# guest").unwrap();
        prepare_for_start(&config, &instance).unwrap();
        assert!(guest_skill.is_file());
    }

    #[test]
    fn skill_sync_and_sync_back_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        store.write_templates().unwrap();
        let host_skills = dir.path().join("host-skills");
        fs::create_dir_all(host_skills.join("demo")).unwrap();
        fs::write(host_skills.join("demo/SKILL.md"), b"# host").unwrap();
        let profile = format!(
            "version = 1\n[[skills]]\nsource = {source:?}\ntarget = \"/root/.config/opencode/skills\"\nsync_back = true\n",
            source = host_skills.to_string_lossy(),
        );
        fs::write(store.path(Harness::Opencode), profile).unwrap();
        let plan = prepare_for_start(&config, &instance).unwrap();
        let share = plan.share.unwrap();
        let skills = first_skill_dir(&share);

        // Guest creates a new skill in the copied tree.
        let guest = skills.join("guest/SKILL.md");
        fs::create_dir_all(guest.parent().unwrap()).unwrap();
        fs::write(&guest, b"# guest").unwrap();

        let report = sync_skills_back(&config, &instance, false).unwrap();
        assert_eq!(report.copied_files, 1);
        assert_eq!(
            fs::read(host_skills.join("guest/SKILL.md")).unwrap(),
            b"# guest"
        );

        // Host change is merged into the instance copy without deleting the
        // guest-created skill.
        fs::write(host_skills.join("demo/SKILL.md"), b"# host-v2").unwrap();
        let report = sync_skills(&config, &instance).unwrap();
        assert!(report.copied_files >= 1);
        assert_eq!(
            fs::read(skills.join("demo/SKILL.md")).unwrap(),
            b"# host-v2"
        );
        assert!(skills.join("guest/SKILL.md").is_file());
    }

    #[test]
    fn symlinks_are_never_copied_into_the_guest() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let host_skills = dir.path().join("host-skills");
        fs::create_dir_all(&host_skills).unwrap();
        fs::write(dir.path().join("secret"), b"host").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), host_skills.join("link")).unwrap();
        let profile = format!(
            "version = 1\n[[skills]]\nsource = {source:?}\ntarget = \"/root/.config/opencode/skills\"\n",
            source = host_skills.to_string_lossy(),
        );
        fs::write(store.path(Harness::Opencode), profile).unwrap();
        let plan = prepare_for_start(&config, &instance).unwrap();
        assert!(plan.warnings.iter().any(|w| w.contains("symlink")));
    }

    #[test]
    fn sync_back_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let host_skills = dir.path().join("host-skills");
        fs::create_dir_all(&host_skills).unwrap();
        let profile = format!(
            "version = 1\n[[skills]]\nsource = {source:?}\ntarget = \"/root/.config/opencode/skills\"\n",
            source = host_skills.to_string_lossy(),
        );
        fs::write(store.path(Harness::Opencode), profile).unwrap();
        let plan = prepare_for_start(&config, &instance).unwrap();
        let share = plan.share.unwrap();
        let skills = first_skill_dir(&share);
        std::os::unix::fs::symlink("/etc/passwd", skills.join("evil")).unwrap();
        let report = sync_skills_back(&config, &instance, false).unwrap();
        assert_eq!(report.copied_files, 0);
        assert!(report.warnings.iter().any(|w| w.contains("symlink")));
        assert!(!host_skills.join("evil").exists());
    }

    #[test]
    fn planted_bundle_symlinks_cannot_redirect_host_writes() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let host_skills = dir.path().join("host-skills");
        fs::create_dir_all(host_skills.join("demo")).unwrap();
        fs::write(host_skills.join("demo/SKILL.md"), b"# host").unwrap();
        let profile = format!(
            "version = 1\n[[skills]]\nsource = {source:?}\ntarget = \"/root/.config/opencode/skills\"\n",
            source = host_skills.to_string_lossy(),
        );
        fs::write(store.path(Harness::Opencode), profile).unwrap();
        let plan = prepare_for_start(&config, &instance).unwrap();
        let share = plan.share.unwrap();

        // A compromised guest replaces its skill copy with a symlink to a
        // host directory outside the instance.
        let trap = dir.path().join("trap");
        fs::create_dir_all(&trap).unwrap();
        fs::write(trap.join("do-not-touch"), b"host").unwrap();
        let skill_copy = first_skill_dir(&share);
        fs::remove_dir_all(&skill_copy).unwrap();
        std::os::unix::fs::symlink(&trap, &skill_copy).unwrap();

        sync_skills(&config, &instance).unwrap();
        let metadata = fs::symlink_metadata(&skill_copy).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        assert_eq!(fs::read(trap.join("do-not-touch")).unwrap(), b"host");

        // The sync-back path also refuses the planted link.
        fs::remove_dir_all(&skill_copy).unwrap();
        std::os::unix::fs::symlink(&trap, &skill_copy).unwrap();
        let report = sync_skills_back(&config, &instance, false).unwrap();
        assert_eq!(report.copied_files, 0);
        assert!(report.warnings.iter().any(|w| w.contains("real directory")));
        assert_eq!(fs::read(trap.join("do-not-touch")).unwrap(), b"host");
    }

    #[test]
    fn missing_sources_are_warnings_not_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let profile = "version = 1\n[[files]]\nsource = \"/definitely/missing\"\ntarget = \"/root/.config/opencode/x\"\n";
        fs::write(store.path(Harness::Opencode), profile).unwrap();
        let plan = prepare_for_start(&config, &instance).unwrap();
        assert!(plan.warnings.iter().any(|w| w.contains("missing")));
        assert!(plan.share.is_none());
    }

    fn make_fifo(path: &Path) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;
        let raw = CString::new(path.as_os_str().as_bytes()).unwrap();
        let result = unsafe { libc::mkfifo(raw.as_ptr(), 0o600) };
        assert_eq!(result, 0, "failed to create fifo at {}", path.display());
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_fifo());
    }

    fn skill_profile(host_skills: &Path) -> String {
        format!(
            "version = 1\n[[skills]]\nsource = {source:?}\ntarget = \"/root/.config/opencode/skills\"\nsync_back = true\n",
            source = host_skills.to_string_lossy(),
        )
    }

    #[test]
    fn copying_skips_symlinks_and_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let host_skills = dir.path().join("host-skills");
        fs::create_dir_all(host_skills.join("demo")).unwrap();
        fs::write(host_skills.join("demo/SKILL.md"), b"# demo").unwrap();
        fs::write(dir.path().join("secret"), b"host").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), host_skills.join("link")).unwrap();
        make_fifo(&host_skills.join("pipe"));
        fs::write(store.path(Harness::Opencode), skill_profile(&host_skills)).unwrap();

        let plan = prepare_for_start(&config, &instance).unwrap();
        let share = plan.share.expect("profile share");
        let skills = first_skill_dir(&share);
        assert!(skills.join("demo/SKILL.md").is_file());
        assert!(!skills.join("link").exists());
        assert!(!skills.join("pipe").exists());
        assert!(
            plan.warnings.iter().any(|w| w.contains("symlink")),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.warnings.iter().any(|w| w.contains("special file")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn sync_back_skips_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let host_skills = dir.path().join("host-skills");
        fs::create_dir_all(host_skills.join("demo")).unwrap();
        fs::write(host_skills.join("demo/SKILL.md"), b"# host").unwrap();
        fs::write(store.path(Harness::Opencode), skill_profile(&host_skills)).unwrap();
        let plan = prepare_for_start(&config, &instance).unwrap();
        let skills = first_skill_dir(&plan.share.unwrap());
        make_fifo(&skills.join("pipe"));

        let report = sync_skills_back(&config, &instance, false).unwrap();
        assert_eq!(report.copied_files, 0);
        assert!(report.skipped_files >= 1);
        assert!(
            report.warnings.iter().any(|w| w.contains("special file")),
            "{:?}",
            report.warnings
        );
        assert!(!host_skills.join("pipe").exists());
    }

    #[test]
    fn sync_back_dry_run_reports_changes_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let host_skills = dir.path().join("host-skills");
        fs::create_dir_all(host_skills.join("demo")).unwrap();
        fs::write(host_skills.join("demo/SKILL.md"), b"# host").unwrap();
        fs::write(store.path(Harness::Opencode), skill_profile(&host_skills)).unwrap();
        let plan = prepare_for_start(&config, &instance).unwrap();
        let skills = first_skill_dir(&plan.share.unwrap());
        fs::write(skills.join("demo/SKILL.md"), b"# guest").unwrap();

        let dry = sync_skills_back(&config, &instance, true).unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.copied_files, 1);
        assert_eq!(
            fs::read(host_skills.join("demo/SKILL.md")).unwrap(),
            b"# host"
        );

        let real = sync_skills_back(&config, &instance, false).unwrap();
        assert!(!real.dry_run);
        assert_eq!(
            fs::read(host_skills.join("demo/SKILL.md")).unwrap(),
            b"# guest"
        );
    }

    #[test]
    fn sync_back_refuses_symlinked_destination_directories() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let host_skills = dir.path().join("host-skills");
        fs::create_dir_all(host_skills.join("seed")).unwrap();
        fs::write(host_skills.join("seed/SKILL.md"), b"# host").unwrap();
        fs::write(store.path(Harness::Opencode), skill_profile(&host_skills)).unwrap();
        let plan = prepare_for_start(&config, &instance).unwrap();
        let skills = first_skill_dir(&plan.share.unwrap());
        fs::create_dir_all(skills.join("sub")).unwrap();
        fs::write(skills.join("sub/SKILL.md"), b"# guest").unwrap();

        let trap = dir.path().join("trap");
        fs::create_dir_all(&trap).unwrap();
        fs::write(trap.join("do-not-touch"), b"host").unwrap();
        std::os::unix::fs::symlink(&trap, host_skills.join("sub")).unwrap();

        let err = sync_skills_back(&config, &instance, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing symlinked profile directory"),
            "{err:#}"
        );
        assert_eq!(fs::read(trap.join("do-not-touch")).unwrap(), b"host");
        assert!(!trap.join("SKILL.md").exists());
    }

    #[test]
    fn the_profile_bundle_and_state_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(&config, Harness::Opencode);
        let store = ProfileStore::new(&config);
        let source = dir.path().join("opencode.json");
        fs::write(&source, b"{}").unwrap();
        let profile = format!(
            "version = 1\n[[files]]\nsource = {source:?}\ntarget = \"/root/.config/opencode/opencode.json\"\n[environment]\nTOKEN = \"secret\"\n",
            source = source.to_string_lossy(),
        );
        fs::write(store.path(Harness::Opencode), profile).unwrap();

        let plan = prepare_for_start(&config, &instance).unwrap();
        let share = plan.share.expect("profile share");
        let root = share.bundle_dir.parent().unwrap();
        for path in [
            share.bundle_dir.clone(),
            share.bundle_dir.join("files"),
            share.bundle_dir.join("skills"),
        ] {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} must be private", path.display());
        }
        for path in [
            root.join("local.json"),
            share.bundle_dir.join("manifest"),
            share.bundle_dir.join("env.sh"),
        ] {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} must be private", path.display());
        }
    }
}
