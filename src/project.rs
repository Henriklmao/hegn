//! Project discovery, git summaries and fuzzy filtering for the picker.

use std::fs;
use std::io;
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
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut projects = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
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

/// Optional per-project hegn settings stored in `.hegn.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectMeta {
    pub harness: Option<crate::config::Harness>,
    pub packages: Vec<String>,
    pub template: Option<String>,
}

const PROJECT_META_FILE: &str = ".hegn.toml";

pub fn read_meta(path: &Path) -> ProjectMeta {
    fs::read_to_string(path.join(PROJECT_META_FILE))
        .ok()
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn write_meta(path: &Path, meta: &ProjectMeta) -> Result<()> {
    let raw = toml::to_string_pretty(meta)?;
    let target = path.join(PROJECT_META_FILE);
    fs::write(&target, raw).with_context(|| format!("failed to write {}", target.display()))?;
    Ok(())
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
    fn project_meta_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let meta = ProjectMeta {
            harness: Some(crate::config::Harness::Codex),
            packages: vec!["nodejs".into(), "git".into()],
            template: Some("rust".into()),
        };
        write_meta(dir.path(), &meta).unwrap();
        let loaded = read_meta(dir.path());
        assert_eq!(loaded.harness, Some(crate::config::Harness::Codex));
        assert_eq!(loaded.packages.len(), 2);
        assert_eq!(read_meta(Path::new("/missing")).packages.len(), 0);
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
