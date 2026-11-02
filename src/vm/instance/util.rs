//! Small filesystem and diagnostics helpers shared by the instance manager.
//!
//! Everything here is best-effort or explicitly private (`0600`) and never
//! follows symlinks when writing runtime metadata.

use std::fs::{self, OpenOptions};
use std::net::Ipv4Addr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::config::Config;
use crate::vm::instance_config::InstanceConfig;

pub(super) fn profile_bundle_dir(config: &Config, instance: &InstanceConfig) -> Option<PathBuf> {
    let bundle = instance.dir(&config.instances_dir).join("profile/bundle");
    bundle.join("manifest").is_file().then_some(bundle)
}

/// Last few lines of a startup log, flattened into one diagnostic line.
pub(super) fn log_tail(path: &Path) -> String {
    let Ok(bytes) = crate::session::logs::tail_bytes(path, 8) else {
        return "no log output".into();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return "no log output".into();
    }
    crate::session::logs::sanitize_label(&lines.join(" | "))
}

/// Tighten Cloud Hypervisor's snapshot output to `0700`/`0600`.
///
/// The snapshot directory is already private, but setting the file modes too
/// keeps the invariant robust if the directory is ever moved or re-created.
pub(super) fn secure_snapshot_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for entry in fs::read_dir(dir)
        .with_context(|| format!("failed to read snapshot directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            bail!(
                "refusing to secure a symlink in the snapshot directory: {}",
                path.display()
            );
        }
        let mode = if meta.is_dir() { 0o700 } else { 0o600 };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to secure {}", path.display()))?;
    }
    Ok(())
}

pub(super) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::symlink_metadata(entry.path()).ok())
        .filter(|meta| meta.file_type().is_file())
        .map(|meta| meta.len())
        .sum()
}

/// Guest URLs for the configured local endpoints on one TAP.
pub(super) fn endpoint_urls(config: &Config, host: Ipv4Addr) -> Vec<String> {
    config
        .local_endpoints
        .iter()
        .map(|endpoint| endpoint.url(host))
        .collect()
}

/// Append a timestamped lifecycle line to `run/hegn.log`.
///
/// Best effort: a logging failure must never mask the VM start result.
pub(super) fn append_event_log(path: &Path, message: &str) {
    use std::io::Write;

    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        let _ = writeln!(file, "{timestamp} hegn: {message}");
    }
}

/// Write a runtime metadata file with mode `0600`.
///
/// The run directory itself is `0700`, but keeping the files private too makes
/// the invariant robust against a future path move or permission mistake.
pub(super) fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(contents.as_ref())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_metadata_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        write_private(&path, br#"{"test":true}"#).unwrap();
        assert_eq!(fs::read(&path).unwrap(), br#"{"test":true}"#);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn startup_log_tail_is_flattened_and_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.log");
        assert_eq!(log_tail(&path), "no log output");
        std::fs::write(&path, b"first\n\x1b[31mfatal\x1b[0m\n\nlast\n").unwrap();
        let tail = log_tail(&path);
        assert!(tail.contains("fatal"), "{tail}");
        assert!(tail.contains("last"), "{tail}");
        assert!(!tail.contains('\n'), "{tail}");
        assert!(!tail.contains('\x1b'), "{tail}");
    }

    #[test]
    fn snapshot_files_are_tightened_to_private_modes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        secure_snapshot_dir(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn snapshot_securing_refuses_symlinks() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        std::fs::write(&target, b"secret").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("memory-ranges");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = secure_snapshot_dir(dir.path()).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}
