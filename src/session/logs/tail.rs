//! Bounded, symlink-safe reads of the end of a session log.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Maximum number of bytes read from the end of a log for `hegn logs show`.
const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn tail_lines_handles_blank_and_unterminated_lines() {
        assert_eq!(tail_lines(b"a\nb\n", 1), b"b\n");
        assert_eq!(tail_lines(b"a\n\nb\n", 1), b"b\n");
        assert_eq!(tail_lines(b"a\n\nb\n", 2), b"\nb\n");
        assert_eq!(tail_lines(b"\n\n\n", 2), b"\n\n");
        assert_eq!(tail_lines(b"no-newline", 1), b"no-newline");
        assert_eq!(tail_lines(b"a\nb", 3), b"a\nb");
    }

    #[test]
    fn tail_bytes_is_capped_to_the_last_four_mib() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-large.log");
        let mut data = vec![b'x'; 4 * 1024 * 1024 + 100];
        data[0] = b'S';
        fs::write(&path, &data).unwrap();

        let tail = tail_bytes(&path, 10).unwrap();
        assert_eq!(tail.len(), 4 * 1024 * 1024);
        assert_eq!(tail[0], b'x', "the file prefix must be skipped");
    }

    #[test]
    fn tail_bytes_returns_binary_data_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-binary.log");
        fs::write(&path, b"\xff\xfeab\n\x1b[31mcolor\x00\n").unwrap();
        assert_eq!(
            tail_bytes(&path, 1).unwrap(),
            b"\x1b[31mcolor\x00\n",
            "tail must not sanitise; rendering decides what is safe"
        );
    }

    #[test]
    fn tail_bytes_rejects_a_non_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = tail_bytes(dir.path(), 1).unwrap_err().to_string();
        assert!(err.contains("not a regular log file"), "{err}");
    }
}
