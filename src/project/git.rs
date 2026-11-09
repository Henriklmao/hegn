//! Git integration: status summaries, safety checkpoints and session branches.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::model::GitSummary;

/// A git invocation hardened against repo-controlled code execution.
///
/// Project repositories are untrusted: `core.fsmonitor`, hooks and clean
/// filters in `.git/config`/`.gitattributes` would otherwise run host code
/// merely by opening or checkpointing a project.
fn hardened_git(path: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(path)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "core.fsmonitor=false"]);
    command
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
    let output = hardened_git(path)
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
    let output = hardened_git(path)
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
    // A private directory (0700, never a symlink) keeps the checkpoint index
    // away from other users of the shared temp directory. The index itself is
    // created with O_NOFOLLOW/O_CLOEXEC and mode 0600.
    let dir = std::env::temp_dir().join(format!("hegn-checkpoint-{}-{unique}", std::process::id()));
    crate::shared::fs::create_private_dir(&dir)
        .with_context(|| format!("failed to create checkpoint directory {}", dir.display()))?;
    let index = dir.join("index");
    if let Err(err) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&index)
        .with_context(|| format!("failed to create checkpoint index {}", index.display()))
    {
        let _ = fs::remove_dir_all(&dir);
        return Err(err);
    }

    let result = (|| -> Result<String> {
        // `git read-tree --empty` seeds a valid empty index into the file we
        // pre-created above; git rejects a zero-length index file outright.
        git_stdout(path, &["read-tree", "--empty"], Some(&index), &[])?;
        // Neutralise every filter driver declared in the repository/global
        // configuration. The repository controls both `.gitattributes` and
        // `filter.<name>.clean`, so `git add` would otherwise execute arbitrary
        // host code. `/bin/cat` keeps the blob content unchanged; the process
        // driver is emptied so it can never spawn, and no filter command from
        // the repository runs.
        let overrides = filter_config_overrides(path)?;
        git_stdout(path, &["add", "-A"], Some(&index), &overrides)?;
        // `write-tree` can re-run the clean filter while hashing the index, so
        // it needs the same overrides as `add`.
        let tree = git_stdout(path, &["write-tree"], Some(&index), &overrides)?;
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
        let mut command = hardened_git(path);
        command
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

    let _ = fs::remove_dir_all(&dir);
    result
}

/// `-c key=value` overrides that disable every configured filter driver.
///
/// `git config --get-regexp` only reads configuration; it never executes the
/// commands it prints, which makes the hardened helper safe here. Exit code 1
/// means "no matches", which is not an error for us.
fn filter_config_overrides(path: &Path) -> Result<Vec<(String, String)>> {
    let output = hardened_git(path)
        .args(["config", "--get-regexp", r"^filter\."])
        .output()
        .context("failed to inspect git filter configuration")?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        bail!(
            "git config --get-regexp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut overrides = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(key) = line.split_whitespace().next() else {
            continue;
        };
        // `.clean`/`.smudge` become a pass-through so the captured blob is the
        // literal working-tree content. `.process` is emptied rather than
        // replaced: an empty command makes git skip the multi-file filter and
        // fall back to the clean/smudge pair, whereas pointing it at `/bin/cat`
        // would still spawn a subprocess that fails the process handshake
        // (stderr noise and a fragile cross-version fallback).
        let value = if key.ends_with(".clean") || key.ends_with(".smudge") {
            "/bin/cat"
        } else if key.ends_with(".process") {
            ""
        } else if key.ends_with(".required") {
            "false"
        } else {
            continue;
        };
        // Re-pass the key verbatim so a subsection's case is preserved.
        overrides.push((key.to_string(), value.to_string()));
    }
    Ok(overrides)
}

fn git_stdout(
    path: &Path,
    args: &[&str],
    index: Option<&Path>,
    config: &[(String, String)],
) -> Result<String> {
    let mut command = hardened_git(path);
    for (key, value) in config {
        command.arg("-c").arg(format!("{key}={value}"));
    }
    command.args(args);
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
    hardened_git(path)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Create and check out `agent/hegn-session-<timestamp>`.
pub fn create_session_branch(path: &Path) -> Result<String> {
    ensure_git_repo(path)?;
    let branch = format!("agent/hegn-session-{}", Utc::now().format("%Y%m%d-%H%M%S"));
    let output = hardened_git(path)
        .args(["switch", "-c", &branch])
        .output()
        .context("failed to run git switch")?;
    if !output.status.success() {
        // Older git versions use checkout.
        let output = hardened_git(path)
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
    let output = hardened_git(path)
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

    /// Serialise tests that create checkpoint index directories so the
    /// "no directory left behind" assertion cannot observe a concurrent test.
    fn checkpoint_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn commit_all(repo: &Path, message: &str) {
        for args in [vec!["add", "."], vec!["commit", "-m", message, "--quiet"]] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }

    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn checkpoint_dirs() -> Vec<std::path::PathBuf> {
        let prefix = format!("hegn-checkpoint-{}-", std::process::id());
        let mut dirs: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with(&prefix))
                    })
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        dirs
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
        let _guard = checkpoint_guard();
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
        let _guard = checkpoint_guard();
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
            &[],
        )
        .unwrap();
        assert_eq!(tracked.trim(), "modified");
        let untracked = git_stdout(
            repo.path(),
            &["show", &format!("{branch}:untracked.txt")],
            None,
            &[],
        )
        .unwrap();
        assert_eq!(untracked.trim(), "new file");

        // The checkpoint must not commit or stage anything in the real repo.
        let status = git_stdout(repo.path(), &["status", "--porcelain"], None, &[]).unwrap();
        assert!(status.contains("M tracked.txt"), "{status}");
        assert!(status.contains("?? untracked.txt"), "{status}");
    }

    #[test]
    fn checkpoint_works_in_a_repository_without_commits() {
        let _guard = checkpoint_guard();
        let repo = git_repo();
        std::fs::write(repo.path().join("first.txt"), "hello").unwrap();
        let branch = create_checkpoint(repo.path(), "before first commit").unwrap();
        let content = git_stdout(
            repo.path(),
            &["show", &format!("{branch}:first.txt")],
            None,
            &[],
        )
        .unwrap();
        assert_eq!(content.trim(), "hello");
    }

    #[test]
    fn session_branch_does_not_run_post_checkout_hooks() {
        let repo = git_repo();
        std::fs::write(repo.path().join("a.txt"), "hello").unwrap();
        commit_all(repo.path(), "init");

        let marker = repo.path().join("hook-ran");
        write_script(
            &repo.path().join(".git/hooks/post-checkout"),
            &format!("#!/bin/sh\necho ran > '{}'\n", marker.display()),
        );

        let branch = create_session_branch(repo.path()).unwrap();
        assert!(branch.starts_with("agent/hegn-session-"));
        assert!(
            !marker.exists(),
            "the post-checkout hook from .git/hooks must not run"
        );
    }

    #[test]
    fn read_git_summary_does_not_run_core_fsmonitor() {
        let repo = git_repo();
        std::fs::write(repo.path().join("a.txt"), "hello").unwrap();
        commit_all(repo.path(), "init");

        let marker = repo.path().join("fsmonitor-ran");
        let script = repo.path().join("fsmonitor.sh");
        write_script(
            &script,
            &format!("#!/bin/sh\necho ran > '{}'\nexit 0\n", marker.display()),
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["config", "core.fsmonitor"])
                .arg(&script)
                .status()
                .unwrap()
                .success()
        );

        let summary = read_git_summary(repo.path());
        assert!(summary.is_repo, "{summary:?}");
        assert!(
            !marker.exists(),
            "core.fsmonitor from the repository config must not run"
        );
    }

    #[test]
    fn checkpoint_does_not_run_clean_filters_and_keeps_content() {
        let _guard = checkpoint_guard();
        let repo = git_repo();
        std::fs::write(repo.path().join("keep.txt"), "original").unwrap();
        commit_all(repo.path(), "init");

        let marker = repo.path().join("filter-ran");
        let script = repo.path().join("clean.sh");
        write_script(
            &script,
            &format!("#!/bin/sh\necho ran > '{}'\ncat\n", marker.display()),
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["config", "filter.evil.clean"])
                .arg(&script)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        std::fs::write(repo.path().join("keep.txt"), "modified").unwrap();

        let branch = create_checkpoint(repo.path(), "before filter").unwrap();
        assert!(
            !marker.exists(),
            "the clean filter declared by the repository must not run"
        );
        let blob = git_stdout(
            repo.path(),
            &["show", &format!("{branch}:keep.txt")],
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            blob.trim(),
            "modified",
            "/bin/cat must pass content through"
        );
    }

    #[test]
    fn checkpoint_does_not_run_process_filters_and_keeps_content() {
        let _guard = checkpoint_guard();
        let repo = git_repo();
        std::fs::write(repo.path().join("keep.txt"), "original").unwrap();
        commit_all(repo.path(), "init");

        let marker = repo.path().join("process-ran");
        let script = repo.path().join("process.sh");
        write_script(
            &script,
            &format!("#!/bin/sh\necho ran > '{}'\ncat\n", marker.display()),
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["config", "filter.evil.process"])
                .arg(&script)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        std::fs::write(repo.path().join("keep.txt"), "modified").unwrap();

        let branch = create_checkpoint(repo.path(), "before process").unwrap();
        assert!(
            !marker.exists(),
            "the process filter declared by the repository must not run"
        );
        let blob = git_stdout(
            repo.path(),
            &["show", &format!("{branch}:keep.txt")],
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            blob.trim(),
            "modified",
            "the emptied process driver must fall back to the /bin/cat pass-through"
        );
    }

    #[test]
    fn checkpoint_leaves_no_private_index_directory_behind() {
        let _guard = checkpoint_guard();
        let repo = git_repo();
        std::fs::write(repo.path().join("a.txt"), "hello").unwrap();

        let before = checkpoint_dirs();
        create_checkpoint(repo.path(), "cleanup").unwrap();
        let after = checkpoint_dirs();

        assert_eq!(
            after, before,
            "checkpoint must remove its private temp directory"
        );
    }

    #[test]
    fn parses_staged_modified_renamed_and_ignored_entries() {
        let output = concat!(
            "## main\n",
            "AM staged-and-modified\n",
            "MM both\n",
            "A  added\n",
            " D deleted-in-worktree\n",
            "R  old -> new\n",
            "!! ignored\n",
            "?? untracked\n",
        );
        let summary = parse_git_status(output);
        assert!(summary.is_repo);
        assert_eq!(summary.branch.as_deref(), Some("main"));
        assert_eq!(summary.staged, 4, "{summary:?}");
        assert_eq!(summary.modified, 3, "{summary:?}");
        assert_eq!(summary.untracked, 1, "{summary:?}");
        assert!(summary.is_dirty());
    }

    #[test]
    fn parses_malformed_branch_tracking_and_short_lines_without_panicking() {
        let summary = parse_git_status("## feature...origin/feature [ahead nope, behind 4]\n");
        assert_eq!(summary.branch.as_deref(), Some("feature"));
        assert_eq!(summary.ahead, 0);
        assert_eq!(summary.behind, 4);

        let summary = parse_git_status("## main...origin/main [ahead 3]\n");
        assert_eq!(summary.branch.as_deref(), Some("main"));
        assert_eq!(summary.ahead, 3);
        assert_eq!(summary.behind, 0);

        // Short/garbage lines use space padding instead of panicking.
        let summary = parse_git_status("## main\nx\n\n??\n");
        assert_eq!(summary.branch.as_deref(), Some("main"));
        assert_eq!(summary.staged, 1);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.untracked, 1);
    }

    #[test]
    fn badge_combines_dirt_ahead_and_behind() {
        let mut summary = parse_git_status("## main...origin/main [ahead 2, behind 1]\n");
        assert_eq!(summary.badge(), "main ↑2 ↓1");
        summary.untracked = 1;
        assert_eq!(summary.badge(), "main* ↑2 ↓1");

        let empty = parse_git_status("");
        assert!(empty.is_repo);
        assert_eq!(empty.badge(), "detached");
        assert_eq!(GitSummary::default().badge(), "not a repo");
    }

    #[test]
    fn read_git_summary_reports_non_directories_and_non_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, "x").unwrap();
        let summary = read_git_summary(&file);
        assert_eq!(summary.error.as_deref(), Some("not a directory"));
        assert!(!summary.is_repo);
        assert_eq!(summary.badge(), "not a repo");

        let plain = tempfile::tempdir().unwrap();
        let summary = read_git_summary(plain.path());
        assert!(!summary.is_repo);
        assert!(summary.error.is_some(), "{summary:?}");
    }

    #[test]
    fn checkpoint_and_session_branch_refuse_non_repositories() {
        let _guard = checkpoint_guard();
        let dir = tempfile::tempdir().unwrap();
        let err = create_checkpoint(dir.path(), "manual")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a git working tree"), "{err}");
        let err = create_session_branch(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not a git working tree"), "{err}");
    }

    #[test]
    fn checkpoint_sanitizes_labels_and_falls_back_to_manual() {
        let _guard = checkpoint_guard();
        let repo = git_repo();
        std::fs::write(repo.path().join("a.txt"), "hello").unwrap();

        let branch = create_checkpoint(repo.path(), "  !!!  ").unwrap();
        assert!(branch.starts_with("hegn/checkpoint-"), "{branch}");
        assert!(branch.ends_with("-manual"), "{branch}");

        let repo = git_repo();
        std::fs::write(repo.path().join("a.txt"), "hello").unwrap();
        let branch = create_checkpoint(repo.path(), "before rm -rf && echo").unwrap();
        assert!(!branch.contains(' '), "{branch}");
        assert!(!branch.contains('&'), "{branch}");
        assert!(branch.ends_with("-before-rm--rf----echo"), "{branch}");
    }
}
