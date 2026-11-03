//! Retention of the newest session logs, never unlinking a live one.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::store::LogStore;

impl LogStore {
    /// Delete all but the newest `keep` session logs. Returns the delete count.
    pub fn prune(&self, keep: usize) -> Result<usize> {
        self.prune_except(keep, None)
    }

    /// Like [`Self::prune`], but never deletes `protected`.
    ///
    /// The running session's log is protected so a manual `hegn logs prune`
    /// cannot unlink the file the console relay is still writing to.
    pub fn prune_except(&self, keep: usize, protected: Option<&Path>) -> Result<usize> {
        let protected: Vec<PathBuf> = protected.map(Path::to_path_buf).into_iter().collect();
        self.prune_except_many(keep, &protected)
    }

    /// Like [`Self::prune_except`], but protects several running sessions.
    pub fn prune_except_many(&self, keep: usize, protected: &[PathBuf]) -> Result<usize> {
        let keep = keep.max(1);
        let logs = self.list()?;
        let mut removed = 0;
        for (index, entry) in logs.iter().enumerate() {
            let is_protected = protected.iter().any(|path| path == &entry.path);
            if index < keep || is_protected {
                continue;
            }
            // Only files that matched our own naming scheme and were verified
            // as regular files by `list` are removed.
            match fs::remove_file(&entry.path) {
                Ok(()) => removed += 1,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to remove {}", entry.path.display()));
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn store(dir: &Path) -> LogStore {
        LogStore::new(dir.join("logs"))
    }

    #[test]
    fn prune_only_removes_old_session_logs() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.ensure().unwrap();
        let mut paths = Vec::new();
        for index in 0..4 {
            paths.push(
                store
                    .create_session(Path::new("/p/demo"), Utc::now(), index)
                    .unwrap(),
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let keep = store.root().join("keep-me.log");
        fs::write(&keep, b"user file").unwrap();

        let removed = store.prune(2).unwrap();
        assert_eq!(removed, 2);
        assert!(keep.exists(), "a foreign .log file must never be pruned");
        let remaining: Vec<_> = store.list().unwrap().into_iter().map(|e| e.path).collect();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0], paths[3]);
        assert_eq!(remaining[1], paths[2]);
    }

    #[test]
    fn prune_keeps_at_least_the_newest_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut paths = Vec::new();
        for index in 0..3 {
            paths.push(
                store
                    .create_session(Path::new("/p/demo"), Utc::now(), index)
                    .unwrap(),
            );
        }

        // keep = 0 is clamped to 1: hegn always retains something to inspect.
        let removed = store.prune(0).unwrap();
        assert_eq!(removed, 2);
        let remaining: Vec<_> = store.list().unwrap().into_iter().map(|e| e.path).collect();
        assert_eq!(remaining, vec![paths[2].clone()]);
    }

    #[test]
    fn prune_never_removes_symlinks_directories_or_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let older = store
            .create_session(Path::new("/p/demo"), Utc::now(), 1)
            .unwrap();
        let _newer = store
            .create_session(Path::new("/p/demo"), Utc::now(), 2)
            .unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"do not delete").unwrap();
        let link = store.root().join("session-link-3.log");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let log_dir = store.root().join("session-dir-4.log");
        fs::create_dir(&log_dir).unwrap();

        let removed = store.prune(1).unwrap();
        assert_eq!(removed, 1);
        assert!(!older.exists(), "the oldest real log is pruned");
        assert!(outside.exists(), "a symlink target must never be unlinked");
        assert!(link.exists());
        assert!(log_dir.is_dir());
    }

    #[test]
    fn prune_except_many_protects_every_active_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut paths = Vec::new();
        for index in 0..5 {
            paths.push(
                store
                    .create_session(Path::new("/p/demo"), Utc::now(), index)
                    .unwrap(),
            );
        }

        // Newest (index 4) is kept by `keep`; the two oldest are live sessions.
        let removed = store
            .prune_except_many(1, &[paths[0].clone(), paths[1].clone()])
            .unwrap();
        assert_eq!(removed, 2);
        let remaining: Vec<_> = store.list().unwrap().into_iter().map(|e| e.path).collect();
        assert!(remaining.contains(&paths[0]), "active log was removed");
        assert!(remaining.contains(&paths[1]), "active log was removed");
        assert!(remaining.contains(&paths[4]));
        assert!(!remaining.contains(&paths[2]));
        assert!(!remaining.contains(&paths[3]));
    }

    #[test]
    fn prune_on_a_missing_directory_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        assert_eq!(store.prune(1).unwrap(), 0);
        assert!(!store.root().exists());
    }

    #[test]
    fn prune_never_deletes_the_running_sessions_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.ensure().unwrap();
        let mut paths = Vec::new();
        for index in 0..3 {
            paths.push(
                store
                    .create_session(Path::new("/p/demo"), Utc::now(), index)
                    .unwrap(),
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Protect the oldest log, as if it were the active session.
        let removed = store.prune_except(1, Some(&paths[0])).unwrap();
        assert_eq!(removed, 1);
        let remaining: Vec<_> = store.list().unwrap().into_iter().map(|e| e.path).collect();
        assert!(remaining.contains(&paths[0]), "active log was removed");
        assert!(remaining.contains(&paths[2]));
        assert!(!remaining.contains(&paths[1]));
    }
}
