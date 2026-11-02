//! Git integration: status summaries, safety checkpoints and session branches.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::model::GitSummary;

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
}
