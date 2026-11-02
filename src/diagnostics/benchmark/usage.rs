//! `/proc` and filesystem probing for process and disk costs.

use std::fs;
use std::path::Path;

/// Disk usage totals for a directory tree.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Usage {
    pub(super) allocated: u64,
    pub(super) apparent: u64,
    pub(super) files: u64,
}

pub(super) fn usage_excluding(root: &Path, exclude: Option<&Path>) -> Usage {
    let mut usage = Usage::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if exclude.is_some_and(|exclude| path == exclude) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let kind = metadata.file_type();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if let Ok(entries) = fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if kind.is_file() {
            usage.files += 1;
            usage.apparent += metadata.len();
            usage.allocated += allocated_bytes(&metadata);
        }
    }
    usage
}

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks() * 512
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

/// `(VmRSS, VmSize)` of a process in bytes, or `None` if it is not running.
fn proc_usage(pid: u32) -> Option<(u64, u64)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut rss = None;
    let mut virt = None;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            rss = parse_kib(value);
        } else if let Some(value) = line.strip_prefix("VmSize:") {
            virt = parse_kib(value);
        }
    }
    Some((rss?, virt?))
}

/// RSS and virtual size of a process plus all of its descendants.
///
/// `virtiofsd` re-executes itself into a sandboxed worker, so the pid recorded
/// in `session.json` is only the parent half of the file server.
pub(super) fn process_tree_usage(root: u32) -> Option<(u64, u64)> {
    let mut parents = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if let Some(parent) = read_ppid(pid) {
                parents.push((pid, parent));
            }
        }
    }

    let mut total = (0u64, 0u64);
    let mut found = false;
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if let Some((rss, virt)) = proc_usage(pid) {
            total.0 += rss;
            total.1 += virt;
            found = true;
        }
        stack.extend(
            parents
                .iter()
                .filter(|(_, parent)| *parent == pid)
                .map(|(child, _)| *child),
        );
    }
    found.then_some(total)
}

/// Parent pid from `/proc/<pid>/stat`; `comm` may contain spaces and `)`.
fn read_ppid(pid: u32) -> Option<u32> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &raw[raw.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn parse_kib(value: &str) -> Option<u64> {
    let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
    Some(kib * 1024)
}

pub(super) fn process_alive(pid: u32) -> bool {
    pid != 0 && Path::new("/proc").join(pid.to_string()).exists()
}

/// Whether the process command line contains `needle`.
///
/// Session metadata can outlive a reboot, and a recycled pid must not be
/// reported as the VMM, the file server or the relay.
pub(super) fn cmdline_contains(pid: u32, needle: &str) -> bool {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            String::from_utf8_lossy(&raw)
                .replace('\0', " ")
                .contains(needle)
        })
        .unwrap_or(false)
}

/// Whether the process runs `binary`, or at least a binary with its file name.
///
/// The configured path may be renamed (`cloud_hypervisor_bin = "/opt/ch"`), so
/// both the configured name and the conventional one are accepted.
pub(super) fn matches_binary(pid: u32, binary: &Path, fallback: &str) -> bool {
    let configured = binary
        .file_name()
        .map(|name| name.to_string_lossy())
        .filter(|name| !name.is_empty())
        .is_some_and(|name| cmdline_contains(pid, &name));
    configured || cmdline_contains(pid, fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_usage_reports_the_current_process() {
        let (rss, virt) = proc_usage(std::process::id()).expect("own /proc entry");
        assert!(rss > 0);
        assert!(virt >= rss);
        assert_eq!(proc_usage(u32::MAX - 1), None);

        let (tree_rss, tree_virt) =
            process_tree_usage(std::process::id()).expect("own process tree");
        // RSS can change between the two reads, so only assert that the tree
        // includes a live root; comparing sums across time would be flaky.
        assert!(tree_rss > 0);
        assert!(tree_virt > 0);
    }

    #[test]
    fn read_ppid_matches_the_os_parent() {
        let pid = std::process::id();
        assert_eq!(read_ppid(pid), Some(std::os::unix::process::parent_id()));
    }

    #[test]
    fn cmdline_contains_matches_the_test_binary() {
        let pid = std::process::id();
        let exe = std::env::current_exe().expect("current exe");
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(cmdline_contains(pid, &name));
        assert!(!cmdline_contains(pid, "definitely-not-on-this-cmdline"));
        assert!(!cmdline_contains(u32::MAX - 1, "hegn"));
    }

    #[test]
    fn matches_binary_accepts_the_configured_path() {
        let pid = std::process::id();
        let exe = std::env::current_exe().expect("current exe");
        assert!(matches_binary(pid, &exe, "no-such-fallback"));
        assert!(!matches_binary(
            pid,
            Path::new("/opt/not-running"),
            "no-such-fallback"
        ));
    }

    #[test]
    fn usage_counts_files_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), vec![0u8; 4096]).unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/other"), b"abc").unwrap();

        let first = usage_excluding(dir.path(), None);
        assert_eq!(first.files, 2);
        assert_eq!(first.apparent, 4096 + 3);
        assert!(first.allocated > 0);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("file"), dir.path().join("link")).unwrap();
            let linked = usage_excluding(dir.path(), None);
            assert_eq!(linked.files, 2, "symlinks are not followed or counted");
        }
    }

    #[test]
    fn usage_can_exclude_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("logs")).unwrap();
        fs::write(dir.path().join("logs/a.log"), vec![0u8; 1024]).unwrap();
        fs::write(dir.path().join("state.json"), b"{}").unwrap();

        let all = usage_excluding(dir.path(), None);
        assert_eq!(all.files, 2);

        let without_logs = usage_excluding(dir.path(), Some(&dir.path().join("logs")));
        assert_eq!(without_logs.files, 1);
        assert_eq!(without_logs.apparent, 2);
    }

    #[test]
    fn usage_can_exclude_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("state.json"), b"{}\n").unwrap();
        fs::write(dir.path().join("other.json"), b"[]\n").unwrap();

        let without_state = usage_excluding(dir.path(), Some(&dir.path().join("state.json")));
        assert_eq!(without_state.files, 1);
        assert_eq!(without_state.apparent, 3);
    }

    #[test]
    fn usage_of_a_missing_or_empty_directory_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            usage_excluding(&dir.path().join("missing"), None),
            Usage::default()
        );
        assert_eq!(usage_excluding(dir.path(), None), Usage::default());
    }

    #[test]
    fn usage_does_not_follow_a_symlinked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), vec![0u8; 1024]).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();

        let usage = usage_excluding(dir.path(), None);
        assert_eq!(usage, Usage::default());
    }

    #[test]
    fn parse_kib_reads_the_leading_number() {
        assert_eq!(parse_kib("1234 kB"), Some(1234 * 1024));
        assert_eq!(parse_kib("  42"), Some(42 * 1024));
        assert_eq!(parse_kib("0 kB"), Some(0));
        assert_eq!(parse_kib("not a number"), None);
        assert_eq!(parse_kib(""), None);
    }

    #[test]
    fn process_alive_rejects_pid_zero_and_missing_processes() {
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX - 1));
        assert!(process_alive(std::process::id()));
    }
}
