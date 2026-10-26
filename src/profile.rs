//! Per-harness guest profiles and the copy-only skill sync.
//!
//! A profile is a TOML file in `profiles_dir` (`~/.config/hegn/profiles/` by
//! default). `hegn config init` writes one template per harness so the user can
//! add packages, configuration files, API keys and skill directories without
//! editing the global config.
//!
//! The host is always the source of truth for configuration files. Skills are
//! **copied** into a private per-instance bundle and exported through a second
//! `virtiofsd`; the host's real skill directories are never mounted into the
//! guest. Changes made in the guest can be copied back explicitly with
//! `hegn skills sync-back`.
//!
//! ```text
//! <instances_dir>/<instance>/profile/
//!   local.json          host-side mapping (never exported)
//!   bundle/             exported to the guest at /run/hegn/profile
//!     manifest          tab-separated copy plan
//!     env.sh            exported environment
//!     files/<n>/…       copied configuration files
//!     skills/<n>/…      copied skills (guest edits land here)
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Harness, safe_package_name};
use crate::instance_config::InstanceConfig;

/// virtio-fs tag of the profile share. Reserved in `config.shared_tag`.
pub const PROFILE_SHARE_TAG: &str = "hegn-profile";
/// Mount point of the profile share inside the guest.
pub const PROFILE_GUEST_MOUNT: &str = "/run/hegn/profile";
/// Current profile schema version.
pub const PROFILE_VERSION: u32 = 1;

const BUNDLE_DIR: &str = "bundle";
const LOCAL_STATE: &str = "local.json";
const FILES_DIR: &str = "files";
const SKILLS_DIR: &str = "skills";
const MANIFEST: &str = "manifest";
const ENV_FILE: &str = "env.sh";
const MAX_ENV_VALUE: usize = 8192;
const MAX_TARGET_LEN: usize = 512;

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
    fn manifest_id(&self, manifest: &str, env: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(PROFILE_VERSION.to_le_bytes());
        hasher.update(manifest.as_bytes());
        hasher.update(env.as_bytes());
        hex::encode(hasher.finalize())
    }
}

/// What `prepare` should do with existing per-instance skill copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refresh {
    /// Re-copy configuration files, keep existing skill copies.
    Start,
    /// Merge host changes into the existing skill copies.
    Skills,
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

struct ProfileLayout {
    root: PathBuf,
    bundle_dir: PathBuf,
}

impl ProfileLayout {
    fn new(config: &Config, instance: &InstanceConfig) -> Result<Self> {
        let root = instance.dir(&config.instances_dir).join("profile");
        let bundle_dir = root.join(BUNDLE_DIR);
        secure_dir(&root)?;
        secure_dir(&bundle_dir)?;
        Ok(Self { root, bundle_dir })
    }

    fn files_dir(&self) -> PathBuf {
        self.bundle_dir.join(FILES_DIR)
    }

    fn skills_dir(&self) -> PathBuf {
        self.bundle_dir.join(SKILLS_DIR)
    }

    fn reset_files(&self) -> Result<()> {
        let files = self.files_dir();
        remove_path(&files)?;
        ensure_real_dir(&files)
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(LOCAL_STATE)
    }

    fn load_state(&self) -> Option<ProfileState> {
        let raw = fs::read_to_string(self.state_path()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn save(&self, state: &ProfileState, profile: &Profile) -> Result<()> {
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

    fn manifest_contents(&self, state: &ProfileState) -> Result<String> {
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

    fn env_contents(&self, profile: &Profile) -> String {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProfileState {
    version: u32,
    harness: Harness,
    files: Vec<StoredFile>,
    skills: Vec<StoredSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredFile {
    source: PathBuf,
    target: String,
    mode: String,
    bundle: String,
    kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSkill {
    source: PathBuf,
    target: String,
    bundle: String,
    sync_back: bool,
}

/// The shell script installed in the guest to apply the profile bundle.
pub fn apply_script() -> String {
    format!(
        r#"#!/bin/bash
set -euo pipefail
PROFILE={mount}
[ -f "$PROFILE/manifest" ] || exit 0
while IFS=$'\t' read -r kind bundle target mode; do
  [ -n "$kind" ] || continue
  case "$kind" in
    f)
      src="$PROFILE/files/$bundle"
      [ -f "$src" ] || continue
      if [ -L "$target" ]; then rm -f "$target"; fi
      mkdir -p "$(dirname "$target")"
      install -m "${{mode:-0600}}" "$src" "$target"
      ;;
    d)
      src="$PROFILE/files/$bundle"
      [ -d "$src" ] || continue
      if [ -L "$target" ]; then rm -f "$target"; fi
      install -d -m "${{mode:-0700}}" "$target"
      cp -a "$src/." "$target/"
      ;;
    s)
      src="$PROFILE/skills/$bundle"
      [ -d "$src" ] || continue
      mkdir -p "$(dirname "$target")"
      if [ -L "$target" ] || [ -f "$target" ]; then rm -f "$target"; fi
      if [ -d "$target" ]; then rm -rf "$target"; fi
      ln -s "$src" "$target"
      ;;
  esac
done < "$PROFILE/manifest"
if [ -f "$PROFILE/env.sh" ]; then
  mkdir -p /etc/profile.d
  install -m 0600 "$PROFILE/env.sh" /etc/profile.d/hegn-profile.sh
fi
"#,
        mount = PROFILE_GUEST_MOUNT
    )
}

fn default_skills(harness: Harness) -> Vec<ProfileSkill> {
    let pairs: &[(&str, &str)] = match harness {
        Harness::Shell => &[],
        Harness::Opencode => &[
            ("~/.config/opencode/skills", "/root/.config/opencode/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::ClaudeCode => &[
            ("~/.claude/skills", "/root/.claude/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::Codex => &[
            ("~/.codex/skills", "/root/.codex/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::Pi => &[
            ("~/.pi/agent/skills", "/root/.pi/agent/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::PrimeAgent => &[
            ("~/.prime/agent/skills", "/root/.prime/agent/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::Aider => &[],
        Harness::GeminiCli => &[
            ("~/.gemini/skills", "/root/.gemini/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
    };
    pairs
        .iter()
        .map(|(source, target)| ProfileSkill {
            source: PathBuf::from(source),
            target: (*target).to_string(),
            sync_back: true,
        })
        .collect()
}

fn template(harness: Harness) -> String {
    let label = harness.label();
    let home = harness_home(harness);
    let file_examples = file_examples(harness);
    let skill_blocks = default_skills(harness)
        .into_iter()
        .map(|skill| {
            format!(
                "[[skills]]\nsource = {:?}\ntarget = {:?}\nsync_back = true\n",
                skill.source.to_string_lossy(),
                skill.target
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"# hegn harness profile: {label}
#
# Written by `hegn config init`. Edit freely; this file may contain API keys.
# Host paths may start with `~`. Guest targets must be absolute and stay under
# /root, /home, /etc, /usr/local, /opt or /workspace.
#
# `hegn skills sync <instance>` copies host skill changes into the instance copy.
# `hegn skills sync-back <instance>` copies guest changes back to the host.
version = {PROFILE_VERSION}

# Extra apt packages installed on first boot, on top of the harness preset.
packages = []

# Environment variables exported to login shells and the agent process.
[environment]
# OPENAI_API_KEY = "sk-..."
# ANTHROPIC_API_KEY = "sk-..."

# Configuration files copied into the guest on every start. Add entries by
# removing the `#` markers. API keys belong in files such as auth.json or in
# the [environment] table above.
{file_examples}
# Copy skills from this machine into the guest. Skills are copied, never
# linked; the guest target points at the per-instance copy. Missing source
# directories are skipped.
{skill_blocks}
# Guest home for {label}: {home}
"#
    )
}

fn harness_home(harness: Harness) -> &'static str {
    match harness {
        Harness::Opencode | Harness::GeminiCli => "~/.config",
        Harness::ClaudeCode => "~/.claude",
        Harness::Codex => "~/.codex",
        Harness::Pi => "~/.pi/agent",
        Harness::PrimeAgent => "~/.prime/agent",
        Harness::Aider => "~/.aider.conf.yml",
        Harness::Shell => "~",
    }
}

fn file_examples(harness: Harness) -> String {
    let examples: &[(&str, &str)] = match harness {
        Harness::Opencode => &[
            (
                "~/.config/opencode/opencode.json",
                "/root/.config/opencode/opencode.json",
            ),
            (
                "~/.config/opencode/auth.json",
                "/root/.config/opencode/auth.json",
            ),
        ],
        Harness::ClaudeCode => &[
            ("~/.claude/settings.json", "/root/.claude/settings.json"),
            ("~/.claude.json", "/root/.claude.json"),
        ],
        Harness::Codex => &[
            ("~/.codex/config.toml", "/root/.codex/config.toml"),
            ("~/.codex/auth.json", "/root/.codex/auth.json"),
        ],
        Harness::Pi => &[
            ("~/.pi/agent/models.json", "/root/.pi/agent/models.json"),
            ("~/.pi/agent/auth.json", "/root/.pi/agent/auth.json"),
        ],
        Harness::PrimeAgent => &[
            (
                "~/.prime/agent/models.json",
                "/root/.prime/agent/models.json",
            ),
            (
                "~/.prime/agent/settings.json",
                "/root/.prime/agent/settings.json",
            ),
        ],
        Harness::Aider => &[("~/.aider.conf.yml", "/root/.aider.conf.yml")],
        Harness::GeminiCli => &[
            ("~/.gemini/settings.json", "/root/.gemini/settings.json"),
            (
                "~/.gemini/oauth_creds.json",
                "/root/.gemini/oauth_creds.json",
            ),
        ],
        Harness::Shell => &[],
    };
    let mut out = String::new();
    for (source, target) in examples {
        out.push_str(&format!(
            "# [[files]]\n# source = {source:?}\n# target = {target:?}\n# mode = \"0600\"\n"
        ));
    }
    out
}

/// Expand a leading `~` without touching the rest of the path.
pub fn expand_home(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
        home.join(rest)
    } else {
        path.to_path_buf()
    }
}

fn validate_source(source: &Path) -> Result<()> {
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

fn validate_guest_path(target: &str) -> Result<()> {
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

fn validate_mode(mode: &str) -> Result<()> {
    if !(3..=4).contains(&mode.len()) || !mode.chars().all(|c| matches!(c, '0'..='7')) {
        bail!("profile mode {mode:?} must be an octal mode such as \"0600\"");
    }
    Ok(())
}

fn validate_env(name: &str, value: &str) -> Result<()> {
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

fn secure_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        bail!("refusing symlinked profile directory {}", path.display());
    }
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure {}", path.display()))?;
    Ok(())
}

/// Ensure `path` is a real directory owned by hegn. A guest can replace a
/// per-instance bundle subdirectory with a symlink; following it on the host
/// would let the guest redirect writes outside the instance. Remove such a
/// plant and recreate a private directory.
fn ensure_real_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        remove_path(path)?;
    }
    secure_dir(path)
}

fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        Ok(_) => {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    Ok(())
}

fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", path.display()))?;
    Ok(())
}

fn copy_file_private(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        secure_dir(parent)?;
    }
    let _ = fs::remove_file(dest);
    let source_metadata =
        fs::metadata(source).with_context(|| format!("failed to inspect {}", source.display()))?;
    let mut input = fs::File::open(source)
        .with_context(|| format!("failed to open profile source {}", source.display()))?;
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
    }
    output.sync_all()?;
    // Keep executable skill helpers usable while never widening the owner
    // permissions (the profile directory itself is 0700).
    let mode = (source_metadata.permissions().mode() & 0o755) | 0o600;
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", dest.display()))?;
    Ok(())
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
    use super::*;
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
    fn templates_are_valid_and_contain_no_missing_files() {
        for harness in Harness::ALL {
            if harness == Harness::Shell {
                continue;
            }
            let raw = template(harness);
            let profile: Profile = toml::from_str(&raw).unwrap();
            profile.validate().unwrap();
            assert!(profile.files.is_empty(), "{raw}");
            assert!(!raw.contains("[[files]]\nsource"), "{raw}");
        }
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
    fn apply_script_is_valid_bash() {
        let script = apply_script();
        let output = std::process::Command::new("bash")
            .arg("-n")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("bash is required for the profile apply script");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn manifest_id_changes_with_the_payload() {
        let profile = Profile::default();
        let a = profile.manifest_id("f\t0\t/x\t0600\n", "");
        let b = profile.manifest_id("f\t1\t/x\t0600\n", "");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
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
}
