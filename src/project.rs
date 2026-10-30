//! Project discovery, removal, git summaries and fuzzy filtering for the picker.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Directories that are never treated as projects.
const SKIP_DIRS: &[&str] = &["target", "node_modules", ".git", ".cache", "venv", ".venv"];

/// Snapshot of `git status` for a project directory.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitSummary {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
    pub ahead: usize,
    pub behind: usize,
    pub error: Option<String>,
}

impl GitSummary {
    pub fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.untracked > 0
    }

    /// Short one-line badge for the project list.
    pub fn badge(&self) -> String {
        if !self.is_repo {
            return "not a repo".to_string();
        }
        let branch = self.branch.as_deref().unwrap_or("detached");
        let mut out = branch.to_string();
        if self.is_dirty() {
            out.push('*');
        }
        if self.ahead > 0 {
            out.push_str(&format!(" ↑{}", self.ahead));
        }
        if self.behind > 0 {
            out.push_str(&format!(" ↓{}", self.behind));
        }
        out
    }
}

/// A project shown in the picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub git: GitSummary,
    pub last_opened: Option<DateTime<Utc>>,
}

impl Project {
    pub fn load(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            name,
            git: read_git_summary(&path),
            path,
            last_opened: None,
        }
    }

    pub fn display_path(&self) -> String {
        let home = dirs::home_dir();
        if let Some(home) = home
            && let Ok(rest) = self.path.strip_prefix(&home)
        {
            return format!("~/{}", rest.display());
        }
        self.path.display().to_string()
    }
}

/// Read a project list from `root`. Non-directories and noisy build folders are
/// skipped. Git is queried for each project; this may block and should run off
/// the UI thread.
pub fn discover(root: &Path) -> io::Result<Vec<Project>> {
    discover_protected(root, &[])
}

/// Like [`discover`], but also skips any project that overlaps hegn state.
///
/// Callers with a [`crate::config::Config`] should pass
/// `config.protected_paths()` so a manually planted symlink to the run
/// directory, logs or snapshots can never be opened as a project.
pub fn discover_protected(root: &Path, protected: &[PathBuf]) -> io::Result<Vec<Project>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let home = dirs::home_dir().and_then(|home| home.canonicalize().ok());
    let protected: Vec<PathBuf> = protected
        .iter()
        .map(|path| absolute_or_canonical(path))
        .collect();
    let mut projects = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        // Resolve imports (symlinks) but never let a link widen the picker to
        // the filesystem root, the home directory, an ancestor of the projects
        // root, or hegn state. Opening such a "project" would export far too
        // much to the guest through virtiofsd.
        let canonical = match path.canonicalize() {
            Ok(canonical) if canonical.is_dir() => canonical,
            _ => continue,
        };
        if canonical == Path::new("/")
            || home.as_ref().is_some_and(|home| &canonical == home)
            || root_canonical.starts_with(&canonical)
            || protected
                .iter()
                .any(|path| canonical.starts_with(path) || path.starts_with(&canonical))
        {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        projects.push(Project::load(path));
    }
    projects.sort_by_key(|project| project.name.to_lowercase());
    Ok(projects)
}

fn absolute_or_canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// Parse `git status --porcelain=v1 --branch` output.
pub fn parse_git_status(output: &str) -> GitSummary {
    let mut summary = GitSummary {
        is_repo: true,
        ..GitSummary::default()
    };

    for line in output.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            parse_branch_header(header, &mut summary);
            continue;
        }
        let mut chars = line.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        if x == '?' && y == '?' {
            summary.untracked += 1;
        } else {
            if x != ' ' && x != '!' {
                summary.staged += 1;
            }
            if y != ' ' && y != '!' {
                summary.modified += 1;
            }
        }
    }
    summary
}

fn parse_branch_header(header: &str, summary: &mut GitSummary) {
    // Forms:
    //   main
    //   main...origin/main [ahead 1, behind 2]
    //   No commits yet on main
    //   HEAD (no branch)
    let (branch_part, tracking) = match header.split_once(" [") {
        Some((branch, tracking)) => (branch, Some(tracking.trim_end_matches(']'))),
        None => (header, None),
    };
    let branch = if let Some(rest) = branch_part.strip_prefix("No commits yet on ") {
        rest
    } else if branch_part == "HEAD (no branch)" {
        "(detached)"
    } else {
        branch_part.split("...").next().unwrap_or(branch_part)
    };
    summary.branch = Some(branch.trim().to_string());

    if let Some(tracking) = tracking {
        for part in tracking.split(',') {
            let part = part.trim();
            if let Some(n) = part.strip_prefix("ahead ") {
                summary.ahead = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                summary.behind = n.trim().parse().unwrap_or(0);
            }
        }
    }
}

/// Run git status for `path`.
pub fn read_git_summary(path: &Path) -> GitSummary {
    if !path.is_dir() {
        return GitSummary {
            error: Some("not a directory".into()),
            ..GitSummary::default()
        };
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain=v1", "--branch"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_git_status(&stdout)
        }
        Ok(output) => GitSummary {
            error: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
            ..GitSummary::default()
        },
        Err(err) => GitSummary {
            error: Some(format!("git unavailable: {err}")),
            ..GitSummary::default()
        },
    }
}

/// Fuzzy subsequence match. Higher is better; `None` means no match.
///
/// Matching is case-insensitive and rewards consecutive characters, word
/// boundaries and matches near the start.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    if needle.len() > hay.len() {
        return None;
    }

    let mut score = 0i32;
    let mut hi = 0usize;
    let mut previous_match: Option<usize> = None;
    for &want in &needle {
        let mut found = None;
        while hi < hay.len() {
            if hay[hi] == want {
                found = Some(hi);
                break;
            }
            hi += 1;
        }
        let idx = found?;
        score += 10;
        if idx == 0 {
            score += 30;
        }
        if idx > 0 && matches!(hay[idx - 1], '/' | '-' | '_' | '.' | ' ') {
            score += 20;
        }
        if previous_match == Some(idx.saturating_sub(1)) {
            score += 30;
        }
        score -= (idx.saturating_sub(previous_match.map(|p| p + 1).unwrap_or(0))) as i32;
        previous_match = Some(idx);
        hi = idx + 1;
    }
    Some(score)
}

/// Filter indices of `projects` by `query`, preserving recency for empty
/// queries and ranking by fuzzy score otherwise.
pub fn filter_projects(projects: &[Project], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        let mut indices: Vec<usize> = (0..projects.len()).collect();
        indices.sort_by(|&a, &b| {
            projects[b]
                .last_opened
                .cmp(&projects[a].last_opened)
                .then_with(|| {
                    projects[a]
                        .name
                        .to_lowercase()
                        .cmp(&projects[b].name.to_lowercase())
                })
        });
        return indices;
    }

    let mut scored: Vec<(usize, i32)> = projects
        .iter()
        .enumerate()
        .filter_map(|(i, project)| {
            let name = fuzzy_score(query, &project.name);
            let path = fuzzy_score(query, &project.path.to_string_lossy());
            match (name, path) {
                (Some(a), Some(b)) => Some((i, a.max(b) + 25)),
                (Some(a), None) => Some((i, a + 25)),
                (None, Some(b)) => Some((i, b)),
                (None, None) => None,
            }
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Optional per-project hegn settings stored in `hegn.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectMeta {
    pub harness: Option<crate::config::Harness>,
    pub packages: Vec<String>,
    pub template: Option<String>,
    /// Launch command override for this project (`harness`, `shell`, or a
    /// command name from the global `[launch]` table).
    pub launch: Option<String>,
    /// Terminal integration override for this project.
    pub integration: Option<crate::launch::Integration>,
    /// Whether starting this project's VM also launches the configured
    /// command. Overrides the global `launch.auto_start`.
    pub auto_start: Option<bool>,
}

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

/// Create a project directory with a seeded `AGENTS.md`, README and git repo.
///
/// Returns the created project path. Refuses to overwrite an existing directory.
pub fn create_project(
    root: &Path,
    name: &str,
    template: Option<&Path>,
    harness: crate::config::Harness,
) -> Result<PathBuf> {
    let name = validate_project_name(name)?;
    let path = root.join(name);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;

    let agents = match template {
        Some(template) if template.is_file() => fs::read_to_string(template)
            .with_context(|| format!("failed to read template {}", template.display()))?,
        _ => crate::config::DEFAULT_AGENTS_MD.to_string(),
    };
    fs::write(path.join("AGENTS.md"), agents)?;
    fs::write(
        path.join("README.md"),
        format!(
            "# {name}\n\nCreated with hegn. Harness: {}.\n",
            harness.label()
        ),
    )?;

    let git = Command::new("git")
        .arg("-C")
        .arg(&path)
        .arg("init")
        .arg("--quiet")
        .output();
    match git {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            // Not fatal: the project is still usable without git.
            eprintln!(
                "warning: git init failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(err) => eprintln!("warning: git is not installed: {err}"),
    }
    Ok(path)
}

/// Expand a leading `~` in a path typed into the TUI or a config file.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    let raw = path.to_string_lossy();
    if raw == "~" {
        home
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        path.to_path_buf()
    }
}

/// Import an existing directory into the project picker without moving it.
///
/// The import is a symlink inside `root`; the project keeps living at its
/// original path. This is deliberately the only host-side mutation, so a large
/// or already-versioned project is never copied or rewritten.
///
/// Safety checks reject the filesystem root, the home directory, anything
/// inside `root` (it is already discoverable), any ancestor of `root`, and any
/// path that overlaps hegn's own state (`protected`).
pub fn import_project(
    root: &Path,
    source: &Path,
    name: Option<&str>,
    protected: &[PathBuf],
) -> Result<PathBuf> {
    let source = expand_tilde(source);
    if !source.exists() {
        bail!("{} does not exist", source.display());
    }
    let source = source
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", source.display()))?;
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }
    if source == Path::new("/") {
        bail!("refusing to import the filesystem root");
    }
    if let Some(home) = dirs::home_dir().and_then(|home| home.canonicalize().ok())
        && source == home
    {
        bail!("refusing to import the home directory");
    }

    fs::create_dir_all(root).with_context(|| format!("failed to create {}", root.display()))?;
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", root.display()))?;
    if source == root {
        bail!("refusing to import the projects root itself");
    }
    if root.starts_with(&source) {
        bail!(
            "{} is an ancestor of the projects root; import a single project instead",
            source.display()
        );
    }
    if source.starts_with(&root) {
        bail!(
            "{} is already inside the projects root; no import needed",
            source.display()
        );
    }
    for protected in protected {
        let protected = absolute_or_canonical(protected);
        if source.starts_with(&protected) || protected.starts_with(&source) {
            bail!(
                "refusing to import {}: it overlaps hegn state at {}",
                source.display(),
                protected.display()
            );
        }
    }

    let name = match name {
        Some(name) => name.trim().to_string(),
        None => source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .with_context(|| {
                format!(
                    "{} has no usable directory name; pass --name",
                    source.display()
                )
            })?,
    };
    let name = validate_project_name(&name)?.to_string();
    let link = root.join(&name);
    match fs::symlink_metadata(&link) {
        Ok(_) => bail!("{} already exists", link.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", link.display()));
        }
    }
    std::os::unix::fs::symlink(&source, &link)
        .with_context(|| format!("failed to create {}", link.display()))?;
    Ok(link)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDeleteOutcome {
    /// True when a real project directory was deleted. Import links and
    /// kept directories leave the original files on disk.
    pub files_deleted: bool,
    /// Instances that were stopped and removed with the project.
    pub instances: Vec<String>,
}

/// How a picker entry was removed.
enum EntryRemoval {
    /// A real project directory was deleted.
    Deleted,
    /// An import symlink was removed; the target directory is untouched.
    RemovedLink,
    /// A real project directory remains on disk and will be hidden.
    Kept,
}

/// Remove a project from the picker.
///
/// Every instance belonging to the project is stopped and deleted. When
/// `delete_files` is true the project directory is removed; an imported symlink
/// only loses the link and its target always stays. When files are kept for a
/// real directory, the entry is recorded as hidden so the next scan does not
/// re-add it.
pub fn delete_project(
    config: &crate::config::Config,
    entry: &Path,
    delete_files: bool,
) -> Result<ProjectDeleteOutcome> {
    let registry = crate::instance::InstanceRegistry::new(config.clone());
    let instances: Vec<String> = registry
        .summaries()?
        .into_iter()
        .filter(|instance| {
            crate::instance::same_project(&instance.project, entry)
                || crate::instance::same_project(&instance.workspace, entry)
        })
        .map(|instance| instance.name)
        .collect();
    for name in &instances {
        registry.delete(name, true)?;
    }

    let removal = remove_entry(&config.projects_root, entry, delete_files)?;
    let kept = matches!(removal, EntryRemoval::Kept);
    let files_deleted = matches!(removal, EntryRemoval::Deleted);
    let mut store = crate::state::StateStore::load(config.state_dir.join("state.json"));
    store.forget_project(entry)?;
    store.set_project_hidden(entry, kept)?;
    Ok(ProjectDeleteOutcome {
        files_deleted,
        instances,
    })
}

/// Remove a picker entry from `root`.
///
/// Imported projects are symlinks: removing the link never touches the imported
/// directory. Real directories are deleted only when `delete_files` is true and
/// are always constrained to a direct child of `root`.
fn remove_entry(root: &Path, entry: &Path, delete_files: bool) -> Result<EntryRemoval> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", root.display()))?;
    let parent = entry
        .parent()
        .with_context(|| format!("{} has no parent directory", entry.display()))?
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", entry.display()))?;
    if parent != root {
        bail!(
            "refusing to delete {}: it is not directly inside {}",
            entry.display(),
            root.display()
        );
    }
    if root == Path::new("/") {
        bail!("refusing to delete projects directly under the filesystem root");
    }

    let metadata = fs::symlink_metadata(entry)
        .with_context(|| format!("failed to inspect {}", entry.display()))?;
    if metadata.file_type().is_symlink() {
        fs::remove_file(entry).with_context(|| format!("failed to remove {}", entry.display()))?;
        return Ok(EntryRemoval::RemovedLink);
    }
    if !metadata.is_dir() {
        bail!("{} is not a project directory", entry.display());
    }
    if !delete_files {
        return Ok(EntryRemoval::Kept);
    }

    let canonical = entry
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", entry.display()))?;
    let home = dirs::home_dir().and_then(|home| home.canonicalize().ok());
    if canonical == Path::new("/")
        || canonical == root
        || home.as_ref().is_some_and(|home| &canonical == home)
    {
        bail!("refusing to delete {}", canonical.display());
    }
    if !canonical.starts_with(&root) {
        bail!(
            "refusing to delete {}: it resolves outside {}",
            entry.display(),
            root.display()
        );
    }
    fs::remove_dir_all(&canonical)
        .with_context(|| format!("failed to delete {}", canonical.display()))?;
    Ok(EntryRemoval::Deleted)
}

/// Create a transient safety branch that snapshots the *working tree*.
///
/// A plain `git branch` only points at `HEAD`, so uncommitted changes would not
/// survive a destructive command even though the UI promised a checkpoint. We
/// instead build a commit object from an isolated temporary index, which
/// captures staged, unstaged and untracked files without touching the user's
/// index, `HEAD` or working tree.
pub fn create_checkpoint(path: &Path, label: &str) -> Result<String> {
    ensure_git_repo(path)?;
    let slug: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let branch = format!(
        "hegn/checkpoint-{}-{}",
        Utc::now().format("%Y%m%d-%H%M%S"),
        if slug.is_empty() { "manual" } else { slug }
    );
    let commit = snapshot_worktree(path, label)?;
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["branch", &branch, &commit])
        .output()
        .context("failed to run git branch")?;
    if !output.status.success() {
        bail!(
            "git branch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(branch)
}

/// Commit the current working tree into a dangling commit object.
fn snapshot_worktree(path: &Path, label: &str) -> Result<String> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let index = std::env::temp_dir().join(format!(
        "hegn-checkpoint-{}-{unique}.index",
        std::process::id()
    ));
    let _ = fs::remove_file(&index);

    let result = (|| -> Result<String> {
        git_stdout(path, &["add", "-A"], Some(&index))?;
        let tree = git_stdout(path, &["write-tree"], Some(&index))?;
        let message = format!(
            "hegn checkpoint: {}",
            if label.trim().is_empty() {
                "manual"
            } else {
                label
            }
        );
        // `commit-tree` needs an identity even when the repository has none
        // configured; use a synthetic one so the safety net always works.
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(path)
            .args(["-c", "user.name=hegn", "-c", "user.email=hegn@localhost"])
            .args(["commit-tree", tree.trim(), "-m", &message]);
        if has_head(path) {
            command.args(["-p", "HEAD"]);
        }
        let output = command.output().context("failed to run git commit-tree")?;
        if !output.status.success() {
            bail!(
                "git commit-tree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    })();

    let _ = fs::remove_file(&index);
    result
}

fn git_stdout(path: &Path, args: &[&str], index: Option<&Path>) -> Result<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(path).args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn has_head(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Create and check out `agent/hegn-session-<timestamp>`.
pub fn create_session_branch(path: &Path) -> Result<String> {
    ensure_git_repo(path)?;
    let branch = format!("agent/hegn-session-{}", Utc::now().format("%Y%m%d-%H%M%S"));
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["switch", "-c", &branch])
        .output()
        .context("failed to run git switch")?;
    if !output.status.success() {
        // Older git versions use checkout.
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["checkout", "-b", &branch])
            .output()
            .context("failed to run git checkout")?;
        if !output.status.success() {
            bail!(
                "git checkout failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(branch)
}

fn ensure_git_repo(path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context("failed to run git")?;
    // `rev-parse --is-inside-work-tree` succeeds in a bare repository but
    // prints `false`; a checkpoint needs a real working tree.
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true" {
        bail!("{} is not a git working tree", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        dir
    }

    #[test]
    fn parses_clean_branch_with_tracking() {
        let summary = parse_git_status("## main...origin/main [ahead 2, behind 1]\n");
        assert!(summary.is_repo);
        assert_eq!(summary.branch.as_deref(), Some("main"));
        assert_eq!(summary.ahead, 2);
        assert_eq!(summary.behind, 1);
        assert!(!summary.is_dirty());
    }

    #[test]
    fn parses_dirty_worktree() {
        let output = "## feature...origin/feature\nM  src/main.rs\n M src/lib.rs\n?? new.txt\n";
        let summary = parse_git_status(output);
        assert_eq!(summary.staged, 1);
        assert_eq!(summary.modified, 1);
        assert_eq!(summary.untracked, 1);
        assert!(summary.is_dirty());
        assert_eq!(summary.badge(), "feature*");
    }

    #[test]
    fn parses_unborn_branch_and_detached_head() {
        let unborn = parse_git_status("## No commits yet on main\n");
        assert_eq!(unborn.branch.as_deref(), Some("main"));
        let detached = parse_git_status("## HEAD (no branch)\n");
        assert_eq!(detached.branch.as_deref(), Some("(detached)"));
    }

    #[test]
    fn fuzzy_matches_subsequences_and_ranks_boundaries() {
        assert!(fuzzy_score("hgn", "hegn-tui").is_some());
        assert!(fuzzy_score("tui", "hegn-tui").is_some());
        assert!(fuzzy_score("xyz", "hegn-tui").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        let boundary = fuzzy_score("tui", "hegn-tui").unwrap();
        let scattered = fuzzy_score("tui", "t-u-i").unwrap();
        assert!(boundary > scattered);
    }

    #[test]
    fn filters_and_orders_by_recency() {
        let now = Utc::now();
        let projects = vec![
            Project {
                name: "alpha".into(),
                path: PathBuf::from("/p/alpha"),
                git: GitSummary::default(),
                last_opened: None,
            },
            Project {
                name: "hegn-tui".into(),
                path: PathBuf::from("/p/hegn-tui"),
                git: GitSummary::default(),
                last_opened: Some(now),
            },
        ];
        let filtered = filter_projects(&projects, "");
        assert_eq!(filtered[0], 1);
        let filtered = filter_projects(&projects, "hgn");
        assert_eq!(filtered[0], 1);
        assert!(filter_projects(&projects, "zzz").is_empty());
    }

    #[test]
    fn validates_names() {
        assert!(validate_project_name("my-project").is_ok());
        assert!(validate_project_name("../escape").is_err());
        assert!(validate_project_name(".hidden").is_err());
        assert!(validate_project_name("").is_err());
        assert!(validate_project_name("bad;rm").is_err());
    }

    #[test]
    fn creates_project_skeleton_and_git_repo() {
        let root = tempfile::tempdir().unwrap();
        let path =
            create_project(root.path(), "demo", None, crate::config::Harness::Opencode).unwrap();
        assert!(path.join("AGENTS.md").is_file());
        assert!(path.join("README.md").is_file());
        assert!(path.join(".git").exists());
        assert!(create_project(root.path(), "demo", None, crate::config::Harness::Shell).is_err());
    }

    #[test]
    fn imports_an_existing_directory_without_moving_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let source = dir.path().join("elsewhere/demo");
        std::fs::create_dir_all(&source).unwrap();

        let link = import_project(&root, &source, None, &[]).unwrap();
        assert_eq!(link, root.canonicalize().unwrap().join("demo"));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(link.canonicalize().unwrap(), source.canonicalize().unwrap());
        assert!(source.is_dir(), "the original directory must stay in place");

        let projects = discover(&root).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "demo");

        let err = import_project(&root, &source, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn import_rejects_roots_ancestors_and_hegn_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();

        assert!(import_project(&root, Path::new("/"), None, &[]).is_err());
        assert!(import_project(&root, dir.path(), None, &[]).is_err());
        assert!(import_project(&root, &root, None, &[]).is_err());
        assert!(import_project(&root, &state, None, std::slice::from_ref(&state)).is_err());
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let err = import_project(&root, &root.join("nested"), None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already inside"), "{err}");
        let err = import_project(&root, &source, Some("../escape"), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("separator"), "{err}");
    }

    #[test]
    fn discover_skips_links_to_dangerous_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::os::unix::fs::symlink("/", root.join("rootfs")).unwrap();
        std::os::unix::fs::symlink(&state, root.join("state-link")).unwrap();
        assert!(
            discover_protected(&root, std::slice::from_ref(&state))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn expands_a_leading_tilde() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde(Path::new("~/demo")), home.join("demo"));
            assert_eq!(expand_tilde(Path::new("~")), home);
        }
        assert_eq!(
            expand_tilde(Path::new("relative")),
            PathBuf::from("relative")
        );
    }

    #[test]
    fn project_meta_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let meta = ProjectMeta {
            harness: Some(crate::config::Harness::Codex),
            packages: vec!["nodejs".into(), "git".into()],
            template: Some("rust".into()),
            launch: Some("review".into()),
            integration: Some(crate::launch::Integration::Tmux),
            auto_start: Some(false),
        };
        write_meta(dir.path(), &meta).unwrap();
        let loaded = read_meta(dir.path());
        assert!(dir.path().join("hegn.toml").is_file());
        assert_eq!(loaded.harness, Some(crate::config::Harness::Codex));
        assert_eq!(loaded.packages.len(), 2);
        assert_eq!(loaded.launch.as_deref(), Some("review"));
        assert_eq!(loaded.integration, Some(crate::launch::Integration::Tmux));
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

    #[test]
    fn removing_a_project_can_keep_or_delete_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let project = root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), "demo").unwrap();

        assert!(matches!(
            remove_entry(&root, &project, false).unwrap(),
            EntryRemoval::Kept
        ));
        assert!(project.is_dir(), "keeping files must not remove them");

        assert!(matches!(
            remove_entry(&root, &project, true).unwrap(),
            EntryRemoval::Deleted
        ));
        assert!(!project.exists());
    }

    #[test]
    fn removing_an_imported_project_only_removes_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let target = dir.path().join("elsewhere/demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("README.md"), "demo").unwrap();
        let link = root.join("demo");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Even an explicit file deletion must never follow the import link.
        assert!(matches!(
            remove_entry(&root, &link, true).unwrap(),
            EntryRemoval::RemovedLink
        ));
        assert!(!link.exists());
        assert!(target.join("README.md").is_file());
    }

    #[test]
    fn removing_refuses_paths_outside_the_projects_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert!(remove_entry(&root, &outside, true).is_err());
        assert!(outside.is_dir());
    }

    #[test]
    fn creates_checkpoint_and_session_branch() {
        let repo = git_repo();
        std::fs::write(repo.path().join("a.txt"), "hello").unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["add", "."])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["commit", "-m", "init", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());

        let checkpoint = create_checkpoint(repo.path(), "before rm -rf").unwrap();
        assert!(checkpoint.starts_with("hegn/checkpoint-"));
        let branch = create_session_branch(repo.path()).unwrap();
        assert!(branch.starts_with("agent/hegn-session-"));
        let head = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), branch);
    }

    #[test]
    fn checkpoint_captures_uncommitted_and_untracked_files() {
        let repo = git_repo();
        std::fs::write(repo.path().join("tracked.txt"), "original").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "init", "--quiet"]] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(repo.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(repo.path().join("tracked.txt"), "modified").unwrap();
        std::fs::write(repo.path().join("untracked.txt"), "new file").unwrap();

        let branch = create_checkpoint(repo.path(), "before rm -rf").unwrap();

        let tracked = git_stdout(
            repo.path(),
            &["show", &format!("{branch}:tracked.txt")],
            None,
        )
        .unwrap();
        assert_eq!(tracked.trim(), "modified");
        let untracked = git_stdout(
            repo.path(),
            &["show", &format!("{branch}:untracked.txt")],
            None,
        )
        .unwrap();
        assert_eq!(untracked.trim(), "new file");

        // The checkpoint must not commit or stage anything in the real repo.
        let status = git_stdout(repo.path(), &["status", "--porcelain"], None).unwrap();
        assert!(status.contains("M tracked.txt"), "{status}");
        assert!(status.contains("?? untracked.txt"), "{status}");
    }

    #[test]
    fn checkpoint_works_in_a_repository_without_commits() {
        let repo = git_repo();
        std::fs::write(repo.path().join("first.txt"), "hello").unwrap();
        let branch = create_checkpoint(repo.path(), "before first commit").unwrap();
        let content =
            git_stdout(repo.path(), &["show", &format!("{branch}:first.txt")], None).unwrap();
        assert_eq!(content.trim(), "hello");
    }

    #[test]
    fn discovers_only_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("one")).unwrap();
        std::fs::create_dir(root.path().join(".hidden")).unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("file.txt"), "x").unwrap();
        let projects = discover(root.path()).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "one");
    }
}
