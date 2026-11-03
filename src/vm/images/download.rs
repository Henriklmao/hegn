//! Streaming HTTPS downloads with private temporary files and atomic install.
//!
//! Every download is written to a private `0600` temporary sibling, hashed
//! while streaming, and only renamed into place after the digest matches. A
//! failure never leaves a usable file behind and only replaces an existing
//! asset when `force` is set.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::shared::fs::create_private_dir_if_missing;

use super::MAX_ASSET_BYTES;
use super::assets::{AssetStatus, status};
use super::verify::is_sha256;

const HTTP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Report download progress at most every 256 KiB, so a multi-gigabyte asset
/// does not flood the channel that feeds the TUI.
const PROGRESS_STEP: u64 = 256 * 1024;

/// Bytes streamed for one asset while a download is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    /// Asset name as reported by [`status`] ("kernel" or "rootfs").
    pub asset: &'static str,
    /// Bytes written so far.
    pub received: u64,
    /// Expected size from the response, when the server advertises one.
    pub total: Option<u64>,
}

impl DownloadProgress {
    /// Completion in `0.0..=1.0`, when the server advertised a length.
    pub fn fraction(&self) -> Option<f64> {
        self.total
            .filter(|total| *total > 0)
            .map(|total| (self.received.min(total) as f64) / (total as f64))
    }
}

/// Stream `reader` into `dest`, verifying the expected SHA-256.
///
/// The file is written to a private temporary sibling first. On success it is
/// atomically renamed; on any error the temporary file is removed. Existing
/// destinations are only replaced when `force` is set.
pub fn install_from_reader(
    reader: impl Read,
    dest: &Path,
    expected_sha256: &str,
    force: bool,
    allow_unverified: bool,
) -> Result<u64> {
    install_from_reader_with_progress(
        reader,
        dest,
        expected_sha256,
        force,
        allow_unverified,
        &mut |_| {},
    )
}

/// [`install_from_reader`] with a progress callback invoked while streaming.
pub fn install_from_reader_with_progress(
    mut reader: impl Read,
    dest: &Path,
    expected_sha256: &str,
    force: bool,
    allow_unverified: bool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<u64> {
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if expected.is_empty() && !allow_unverified {
        bail!(
            "refusing an unverified download for {}; configure a SHA-256 or pass --allow-unverified",
            dest.display()
        );
    }
    if !expected.is_empty() && !is_sha256(&expected) {
        bail!(
            "invalid SHA-256 for {}: '{expected_sha256}'",
            dest.display()
        );
    }
    if !force && fs::symlink_metadata(dest).is_ok() {
        bail!(
            "{} already exists; pass --force to replace it",
            dest.display()
        );
    }
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        create_private_dir_if_missing(parent)?;
    }

    let tmp = temp_path(dest);
    let mut file = match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)
    {
        Ok(file) => file,
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create {}", tmp.display()));
        }
    };

    let result = (|| -> Result<u64> {
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut reported = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > MAX_ASSET_BYTES {
                bail!(
                    "download exceeds the {} MiB safety limit",
                    MAX_ASSET_BYTES / (1024 * 1024)
                );
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
            if total - reported >= PROGRESS_STEP {
                reported = total;
                on_progress(total);
            }
        }
        on_progress(total);
        file.flush()?;
        file.sync_all()?;
        let actual = hex::encode(hasher.finalize());
        if !expected.is_empty() && actual != expected {
            bail!(
                "SHA-256 mismatch for {}: expected {expected}, got {actual}",
                dest.display()
            );
        }
        fs::rename(&tmp, dest).with_context(|| format!("failed to install {}", dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dest, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to secure {}", dest.display()))?;
        }
        Ok(total)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Download one asset over HTTPS and verify it before installation.
pub fn download_to(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    force: bool,
    allow_unverified: bool,
) -> Result<u64> {
    download_to_with_progress(
        url,
        dest,
        expected_sha256,
        force,
        allow_unverified,
        &mut |_, _| {},
    )
}

/// [`download_to`] with a progress callback receiving bytes read and the
/// response's advertised length.
pub fn download_to_with_progress(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    force: bool,
    allow_unverified: bool,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64> {
    if url.is_empty() {
        bail!("no download URL configured for {}", dest.display());
    }
    if !url.starts_with("https://") {
        bail!("refusing non-HTTPS download URL '{url}'");
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .context("failed to build the HTTP client")?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("download of {url} failed with HTTP {status}");
    }
    let total = response.content_length();
    install_from_reader_with_progress(
        &mut response,
        dest,
        expected_sha256,
        force,
        allow_unverified,
        &mut |received| on_progress(received, total),
    )
}

/// Download every missing asset. Existing files are never replaced.
pub fn download_missing(config: &Config, allow_unverified: bool) -> Result<Vec<AssetStatus>> {
    download_all(config, false, allow_unverified)
}

/// [`download_missing`] with a progress callback for the TUI notification.
pub fn download_missing_with_progress(
    config: &Config,
    allow_unverified: bool,
    on_progress: &mut dyn FnMut(DownloadProgress),
) -> Result<Vec<AssetStatus>> {
    download_all_with_progress(config, false, allow_unverified, on_progress)
}

/// Download configured assets. With `force`, existing files are replaced only
/// after the replacement download verified successfully.
pub fn download_all(
    config: &Config,
    force: bool,
    allow_unverified: bool,
) -> Result<Vec<AssetStatus>> {
    download_all_with_progress(config, force, allow_unverified, &mut |_| {})
}

/// [`download_all`] with a progress callback for every asset it streams.
pub fn download_all_with_progress(
    config: &Config,
    force: bool,
    allow_unverified: bool,
    on_progress: &mut dyn FnMut(DownloadProgress),
) -> Result<Vec<AssetStatus>> {
    for asset in status(config) {
        if let Ok(metadata) = fs::symlink_metadata(&asset.path)
            && metadata.file_type().is_symlink()
            && !force
        {
            bail!(
                "refusing symlinked image {}; remove it or pass --force",
                asset.path.display()
            );
        }
        if asset.exists && !force {
            continue;
        }
        let name = asset.name;
        download_to_with_progress(
            &asset.url,
            &asset.path,
            &asset.sha256,
            force,
            allow_unverified,
            &mut |received, total| {
                on_progress(DownloadProgress {
                    asset: name,
                    received,
                    total,
                });
            },
        )?;
    }
    Ok(status(config))
}

fn temp_path(dest: &Path) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "asset".into());
    dest.with_file_name(format!(".{name}.part-{}-{unique}", std::process::id()))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    const SHA_HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn installs_a_verified_file_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("images/kernel");
        let written =
            install_from_reader(Cursor::new(b"hello"), &dest, SHA_HELLO, false, false).unwrap();
        assert_eq!(written, 5);
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(dir.path().join("images"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn streaming_reports_progress_and_the_final_size() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("kernel");
        let mut seen = Vec::new();
        let payload = vec![b'x'; (PROGRESS_STEP * 3 + 17) as usize];
        let sha = hex::encode(Sha256::digest(&payload));
        let written = install_from_reader_with_progress(
            Cursor::new(payload.clone()),
            &dest,
            &sha,
            false,
            false,
            &mut |received| seen.push(received),
        )
        .unwrap();
        assert_eq!(written, payload.len() as u64);
        assert!(!seen.is_empty(), "progress must be reported at least once");
        assert_eq!(seen.last().copied(), Some(payload.len() as u64));
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "progress only moves forward: {seen:?}"
        );
        assert!(seen.len() < 10, "progress is throttled: {seen:?}");
    }

    #[test]
    fn download_fraction_clamps_to_the_advertised_length() {
        let progress = DownloadProgress {
            asset: "rootfs",
            received: 512,
            total: Some(1024),
        };
        assert_eq!(progress.fraction(), Some(0.5));
        let overshoot = DownloadProgress {
            received: 2048,
            ..progress
        };
        assert_eq!(overshoot.fraction(), Some(1.0));
        let unknown = DownloadProgress {
            asset: "kernel",
            received: 5,
            total: None,
        };
        assert_eq!(unknown.fraction(), None);
        let empty = DownloadProgress {
            total: Some(0),
            ..unknown
        };
        assert_eq!(empty.fraction(), None);
    }

    #[test]
    fn a_checksum_mismatch_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("kernel");
        let err = install_from_reader(Cursor::new(b"evil"), &dest, SHA_HELLO, false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("SHA-256 mismatch"), "{err}");
        assert!(!dest.exists());
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary file left behind: {leftovers:?}"
        );
    }

    #[test]
    fn unverified_downloads_need_an_explicit_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("kernel");
        assert!(install_from_reader(Cursor::new(b"hello"), &dest, "", false, false).is_err());
        install_from_reader(Cursor::new(b"hello"), &dest, "", false, true).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn existing_assets_are_not_replaced_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("kernel");
        fs::write(&dest, b"old").unwrap();
        let err = install_from_reader(Cursor::new(b"hello"), &dest, SHA_HELLO, false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(fs::read(&dest).unwrap(), b"old");

        install_from_reader(Cursor::new(b"hello"), &dest, SHA_HELLO, true, false).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn downloads_must_use_https() {
        let dir = tempfile::tempdir().unwrap();
        let err = download_to(
            "http://example.com/kernel",
            &dir.path().join("kernel"),
            SHA_HELLO,
            false,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("non-HTTPS"), "{err}");
    }

    #[test]
    fn download_all_refuses_symlinked_assets() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-kernel");
        fs::write(&target, b"kernel").unwrap();
        let link = dir.path().join("kernel");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut config = Config::default();
        config.kernel_image = link;
        config.rootfs_image = dir.path().join("rootfs");
        let err = download_all(&config, false, false).unwrap_err().to_string();
        assert!(err.contains("symlinked image"), "{err}");
    }

    #[test]
    fn invalid_checksums_are_rejected_before_any_file_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("kernel");
        let err = install_from_reader(Cursor::new(b"hello"), &dest, "not-a-digest", false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid SHA-256"), "{err}");
        assert!(!dest.exists());
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "a rejected digest must not create a temporary file"
        );
    }

    #[test]
    fn a_forced_install_replaces_a_symlink_instead_of_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"victim").unwrap();
        let dest = dir.path().join("kernel");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();

        install_from_reader(Cursor::new(b"hello"), &dest, SHA_HELLO, true, false).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"victim");
        let metadata = fs::symlink_metadata(&dest).unwrap();
        assert!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "the installed asset must be a real file"
        );
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn an_unforced_install_refuses_a_symlinked_destination() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"victim").unwrap();
        let dest = dir.path().join("kernel");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();

        let err = install_from_reader(Cursor::new(b"hello"), &dest, SHA_HELLO, false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(fs::read(&victim).unwrap(), b"victim");
    }

    #[test]
    fn progress_is_reported_for_empty_and_small_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let mut empty = Vec::new();
        let dest = dir.path().join("empty");
        install_from_reader_with_progress(
            Cursor::new(Vec::new()),
            &dest,
            "",
            false,
            true,
            &mut |received| empty.push(received),
        )
        .unwrap();
        assert_eq!(empty, vec![0]);

        let mut small = Vec::new();
        let dest = dir.path().join("small");
        install_from_reader_with_progress(
            Cursor::new(b"hello"),
            &dest,
            SHA_HELLO,
            false,
            false,
            &mut |received| small.push(received),
        )
        .unwrap();
        assert_eq!(small, vec![5]);
    }

    #[test]
    fn private_directories_are_created_with_mode_0700() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("one/two/three");
        crate::shared::fs::create_private_dir(&nested).unwrap();
        let mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Re-creating an existing private directory is a no-op.
        crate::shared::fs::create_private_dir(&nested).unwrap();
        assert!(nested.is_dir());
    }

    #[test]
    fn temporary_paths_stay_beside_the_destination() {
        let dest = Path::new("/images/kernel");
        let tmp = temp_path(dest);
        assert_eq!(tmp.parent(), dest.parent());
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".kernel.part-"), "{name}");
        assert_ne!(tmp, dest);
    }

    #[test]
    fn downloads_without_a_url_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = download_to("", &dir.path().join("kernel"), SHA_HELLO, false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no download URL"), "{err}");
    }
}
