//! Persistent, private serial-console logs.
//!
//! The detached console relay appends the guest UART to a log in
//! `<state>/logs/`, so the full boot and session output survives `hegn instance
//! stop` even while no terminal is attached.
//!
//! Log files can contain whatever an agent printed, including secrets or
//! terminal escape sequences. They are therefore created with mode `0600`
//! inside a `0700` directory, opened with `O_NOFOLLOW`, never resolved through
//! symlinks, and only its own `session-*.log` files are ever pruned.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

/// Prefix of every log file hegn creates. Pruning never touches other files.
pub const SESSION_PREFIX: &str = "session-";
pub const LOG_SUFFIX: &str = ".log";
/// Number of session logs kept by default when a new session starts.
pub const DEFAULT_KEEP: usize = 50;
/// Maximum number of bytes read from the end of a log for `hegn logs show`.
const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PROJECT_SEGMENT: usize = 64;
const MAX_NAME_ATTEMPTS: u32 = 100;

/// Metadata for one persisted serial log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LogEntry {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified: Option<DateTime<Utc>>,
}

/// Log directory inside the hegn state directory.
#[derive(Debug, Clone)]
pub struct LogStore {
    root: PathBuf,
}

impl LogStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the log directory with private permissions.
    pub fn ensure(&self) -> Result<()> {
        // A symlinked log directory could redirect every future session log.
        // Refuse rather than silently writing through it.
        if let Ok(metadata) = fs::symlink_metadata(&self.root) {
            if metadata.file_type().is_symlink() {
                bail!("refusing symlinked log directory {}", self.root.display());
            }
            if !metadata.is_dir() {
                bail!("{} is not a directory", self.root.display());
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            use std::os::unix::fs::PermissionsExt;
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(&self.root)
                .with_context(|| format!("failed to create {}", self.root.display()))?;
            // The directory may already have existed with wider permissions.
            // Failing to tighten it is a hard error: logs may contain secrets.
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", self.root.display()))?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        Ok(())
    }

    /// Create an empty, private log for a new VM session.
    ///
    /// The file is created with `O_NOFOLLOW` so a pre-existing symlink in the
    /// log directory can never redirect the write.
    pub fn create_session(
        &self,
        project: &Path,
        started_at: DateTime<Utc>,
        pid: u32,
    ) -> Result<PathBuf> {
        self.ensure()?;
        let project = project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "project".to_string());
        let project = sanitize_segment(&project, MAX_PROJECT_SEGMENT);
        let stamp = started_at.format("%Y%m%dT%H%M%SZ");
        for attempt in 0..MAX_NAME_ATTEMPTS {
            let suffix = if attempt == 0 {
                String::new()
            } else {
                format!("-{attempt}")
            };
            let name = format!("{SESSION_PREFIX}{stamp}-{project}-{pid}{suffix}{LOG_SUFFIX}");
            let path = self.root.join(&name);
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(_) => return Ok(path),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }
        bail!(
            "could not allocate a unique serial log name in {}",
            self.root.display()
        )
    }

    /// All session logs, newest first.
    pub fn list(&self) -> Result<Vec<LogEntry>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.root.display()));
            }
        };
        let mut logs = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_log_name(&name) {
                continue;
            }
            // Never follow symlinks: a link could point outside the state
            // directory, and listing it would suggest it is safe to read.
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) if metadata.file_type().is_file() => metadata,
                _ => continue,
            };
            logs.push(LogEntry {
                name,
                path: entry.path(),
                size_bytes: metadata.len(),
                modified: metadata.modified().ok().map(DateTime::<Utc>::from),
            });
        }
        logs.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| b.name.cmp(&a.name))
        });
        Ok(logs)
    }

    /// Resolve a user-supplied log name (or the newest log when `None`).
    pub fn resolve(&self, name: Option<&str>) -> Result<PathBuf> {
        match name {
            Some(name) => {
                if name.is_empty()
                    || name.contains('/')
                    || name.contains('\\')
                    || name == "."
                    || name == ".."
                {
                    bail!("invalid log name '{name}'");
                }
                if !is_log_name(name) {
                    // Convenience lookup by project segment, e.g.
                    // `hegn logs show my-app`. The name is sanitized and
                    // matched against generated file names only; it is never
                    // joined to a path.
                    let segment = sanitize_segment(name, MAX_PROJECT_SEGMENT);
                    let needle = format!("-{segment}-");
                    return self
                        .list()?
                        .into_iter()
                        .find(|entry| entry.name.contains(&needle))
                        .map(|entry| entry.path)
                        .with_context(|| format!("no serial log for project '{name}'"));
                }
                let path = self.root.join(name);
                let metadata = fs::symlink_metadata(&path)
                    .with_context(|| format!("log '{name}' does not exist"))?;
                if metadata.file_type().is_symlink() {
                    bail!("refusing to use symlinked log '{name}'");
                }
                if !metadata.file_type().is_file() {
                    bail!("'{name}' is not a regular log file");
                }
                Ok(path)
            }
            None => self
                .list()?
                .into_iter()
                .next()
                .map(|entry| entry.path)
                .context(
                    "no serial logs yet; start a VM with `hegn open` or `hegn instance start`",
                ),
        }
    }

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

/// `true` for names hegn itself could have generated.
pub fn is_log_name(name: &str) -> bool {
    name.starts_with(SESSION_PREFIX)
        && name.ends_with(LOG_SUFFIX)
        && name.len() > SESSION_PREFIX.len() + LOG_SUFFIX.len()
}

/// Read at most the last `lines` lines (and at most 4 MiB) of a log.
///
/// The file is opened with `O_NOFOLLOW` so a swapped-in symlink cannot make
/// `hegn logs show` read arbitrary host files.
pub fn tail_bytes(path: &Path, lines: usize) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        // O_NONBLOCK keeps a swapped-in special file from hanging the CLI; it
        // has no effect on regular files.
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("{} is not a regular log file", path.display());
    }
    let len = metadata.len();
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(tail_lines(&buf, lines))
}

fn tail_lines(buf: &[u8], lines: usize) -> Vec<u8> {
    if lines == 0 || buf.is_empty() {
        return Vec::new();
    }
    let mut end = buf.len();
    if buf[end - 1] == b'\n' {
        end -= 1;
    }
    let mut found = 0;
    let mut start = 0;
    let mut index = end;
    while index > 0 {
        index -= 1;
        if buf[index] == b'\n' {
            found += 1;
            if found == lines {
                start = index + 1;
                break;
            }
        }
    }
    buf[start..].to_vec()
}

/// Remove terminal control and bidi-override characters from untrusted text
/// before it is written into a status bar or log name.
pub fn sanitize_label(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_control() || is_bidi_control(ch) {
                '?'
            } else {
                ch
            }
        })
        .collect()
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch as u32,
        0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
    )
}

/// Turn arbitrary project names into a safe single path segment.
pub fn sanitize_segment(input: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        let allowed = ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.');
        if allowed {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= max_bytes {
            break;
        }
    }
    let trimmed = out.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn store(dir: &Path) -> LogStore {
        LogStore::new(dir.join("logs"))
    }

    #[test]
    fn creates_private_logs_with_safe_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let path = store
            .create_session(Path::new("/projects/../../etc/passwd"), Utc::now(), 42)
            .unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(is_log_name(&name), "{name}");
        assert!(!name.contains(".."), "{name}");
        assert!(!name.contains('/'), "{name}");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(store.root()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn list_is_newest_first_and_ignores_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.ensure().unwrap();
        let first = store
            .create_session(Path::new("/p/one"), Utc::now(), 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let second = store
            .create_session(Path::new("/p/two"), Utc::now(), 2)
            .unwrap();
        fs::write(store.root().join("notes.txt"), b"not a log").unwrap();

        let logs = store.list().unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].path, second);
        assert_eq!(logs[1].path, first);
    }

    #[test]
    fn resolve_rejects_traversal_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.ensure().unwrap();
        let log = store
            .create_session(Path::new("/p/one"), Utc::now(), 1)
            .unwrap();

        assert!(store.resolve(Some("../session-x.log")).is_err());
        assert!(
            store
                .resolve(Some(log.file_name().unwrap().to_str().unwrap()))
                .is_ok()
        );
        assert_eq!(store.resolve(Some("one")).unwrap(), log);

        let link = store.root().join("session-evil.log");
        std::os::unix::fs::symlink(&log, &link).unwrap();
        let err = store
            .resolve(Some("session-evil.log"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn refuses_a_symlinked_log_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("logs");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let store = LogStore::new(&link);
        let err = store.ensure().unwrap_err().to_string();
        assert!(err.contains("symlinked log directory"), "{err}");
    }

    #[test]
    fn latest_is_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        assert!(store.resolve(None).is_err());
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

    #[test]
    fn tail_bytes_returns_requested_lines() {
        assert_eq!(tail_lines(b"a\nb\nc\n", 2), b"b\nc\n");
        assert_eq!(tail_lines(b"a\nb\nc", 1), b"c");
        assert_eq!(tail_lines(b"a\nb\nc\n", 10), b"a\nb\nc\n");
        assert_eq!(tail_lines(b"", 5), b"");
        assert_eq!(tail_lines(b"a\n", 0), b"");
    }

    #[test]
    fn tail_bytes_reads_the_file_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-test.log");
        fs::write(&path, b"one\ntwo\nthree\n").unwrap();
        assert_eq!(tail_bytes(&path, 2).unwrap(), b"two\nthree\n");

        let link = dir.path().join("session-link.log");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(tail_bytes(&link, 1).is_err());
    }

    #[test]
    fn sanitizes_control_characters_and_project_segments() {
        assert_eq!(sanitize_label("a\x1b[31mb\u{202e}"), "a?[31mb?");
        assert_eq!(sanitize_segment("My Project/../x", 64), "My-Project-..-x");
        assert_eq!(sanitize_segment("///", 64), "project");
        assert_eq!(sanitize_segment("abcdef", 3), "abc");
    }

    #[test]
    fn names_must_look_like_session_logs() {
        assert!(is_log_name("session-20260101T000000Z-demo-1.log"));
        assert!(!is_log_name("config.toml"));
        assert!(!is_log_name("session-.log"));
        assert!(!is_log_name("other.log"));
    }
}
