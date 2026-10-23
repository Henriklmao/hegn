//! Downloading and verifying the upstream Firecracker VM assets.
//!
//! hegn deliberately does not build or publish its own guest image. The
//! kernel and root filesystem come from the official Firecracker CI bucket,
//! pinned by URL **and** SHA-256. Downloads are streamed into a private
//! temporary file, verified, and atomically renamed; a checksum mismatch never
//! leaves a usable file behind.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::Config;

/// Refuse absurdly large downloads before they fill the disk.
pub const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(target_arch = "x86_64")]
mod pinned {
    pub const KERNEL_VERSION: &str = "v1.15";
    pub const KERNEL_ARTIFACT: &str = "vmlinux-6.1.155";
    pub const KERNEL_URL: &str =
        "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/x86_64/vmlinux-6.1.155";
    pub const KERNEL_SHA256: &str =
        "e20e46d0c36c55c0d1014eb20576171b3f3d922260d9f792017aeff53af3d4f2";

    pub const ROOTFS_VERSION: &str = "v1.10";
    pub const ROOTFS_ARTIFACT: &str = "ubuntu-22.04.ext4";
    pub const ROOTFS_URL: &str =
        "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/ubuntu-22.04.ext4";
    pub const ROOTFS_SHA256: &str =
        "040927105bd01b19e7b02cd5da5a9552b428a7f84bd5ffc22ebfce4ddf258a07";
}

#[cfg(target_arch = "aarch64")]
mod pinned {
    pub const KERNEL_VERSION: &str = "v1.15";
    pub const KERNEL_ARTIFACT: &str = "vmlinux-6.1.155";
    pub const KERNEL_URL: &str =
        "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/aarch64/vmlinux-6.1.155";
    pub const KERNEL_SHA256: &str =
        "e3544b10603acbf3db492cb52e000d22ba202cb4b63b9add027565683e11c591";

    pub const ROOTFS_VERSION: &str = "v1.10";
    pub const ROOTFS_ARTIFACT: &str = "ubuntu-22.04.ext4";
    pub const ROOTFS_URL: &str =
        "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/aarch64/ubuntu-22.04.ext4";
    pub const ROOTFS_SHA256: &str =
        "b7a8c7578812f907ad7f72ddea9b57a5d15f461b51bd3e1933b5ad5c8eb1d708";
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("hegn supports x86_64 and aarch64 guest assets only");

pub use pinned::*;

/// State of one configured VM asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetStatus {
    pub name: &'static str,
    pub path: PathBuf,
    pub url: String,
    pub sha256: String,
    pub exists: bool,
    pub size_bytes: Option<u64>,
}

impl AssetStatus {
    pub fn missing(&self) -> bool {
        !self.exists
    }
}

pub fn status(config: &Config) -> Vec<AssetStatus> {
    vec![
        asset(
            "kernel",
            &config.kernel_image,
            &config.kernel_url,
            &config.kernel_sha256,
        ),
        asset(
            "rootfs",
            &config.rootfs_image,
            &config.rootfs_url,
            &config.rootfs_sha256,
        ),
    ]
}

/// `true` only for a real file, never a symlink or directory.
pub fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false)
}

fn asset(name: &'static str, path: &Path, url: &str, sha256: &str) -> AssetStatus {
    let size_bytes = fs::symlink_metadata(path)
        .ok()
        .filter(|meta| meta.file_type().is_file())
        .map(|meta| meta.len());
    AssetStatus {
        name,
        path: path.to_path_buf(),
        url: url.to_string(),
        sha256: sha256.to_string(),
        exists: size_bytes.is_some(),
        size_bytes,
    }
}

/// `true` for a well-formed lowercase or uppercase SHA-256 hex digest.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Stream `reader` into `dest`, verifying the expected SHA-256.
///
/// The file is written to a private temporary sibling first. On success it is
/// atomically renamed; on any error the temporary file is removed. Existing
/// destinations are only replaced when `force` is set.
pub fn install_from_reader(
    mut reader: impl Read,
    dest: &Path,
    expected_sha256: &str,
    force: bool,
    allow_unverified: bool,
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
        create_private_dir(parent)?;
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
        }
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
    install_from_reader(
        &mut response,
        dest,
        expected_sha256,
        force,
        allow_unverified,
    )
}

/// Download every missing asset. Existing files are never replaced.
pub fn download_missing(config: &Config, allow_unverified: bool) -> Result<Vec<AssetStatus>> {
    download_all(config, false, allow_unverified)
}

/// Download configured assets. With `force`, existing files are replaced only
/// after the replacement download verified successfully.
pub fn download_all(
    config: &Config,
    force: bool,
    allow_unverified: bool,
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
        download_to(
            &asset.url,
            &asset.path,
            &asset.sha256,
            force,
            allow_unverified,
        )?;
    }
    Ok(status(config))
}

/// Hash an installed asset and compare it with the configured digest.
pub fn verify_file(path: &Path, expected_sha256: &str) -> Result<u64> {
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if !is_sha256(&expected) {
        bail!("no valid SHA-256 configured for {}", path.display());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_ASSET_BYTES {
            bail!("{} exceeds the asset size limit", path.display());
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        bail!(
            "SHA-256 mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(total)
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

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    const SHA_HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn sha256_validation_is_strict() {
        assert!(is_sha256(SHA_HELLO));
        assert!(is_sha256(&SHA_HELLO.to_uppercase()));
        assert!(!is_sha256(""));
        assert!(!is_sha256(&SHA_HELLO[..63]));
        assert!(!is_sha256(&format!("{}z", &SHA_HELLO[..63])));
    }

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
    fn verify_file_checks_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel");
        fs::write(&path, b"hello").unwrap();
        assert_eq!(verify_file(&path, SHA_HELLO).unwrap(), 5);
        assert!(verify_file(&path, &"0".repeat(64)).is_err());
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
    fn status_reflects_missing_and_present_assets() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.kernel_image = dir.path().join("kernel");
        config.rootfs_image = dir.path().join("rootfs");
        let assets = status(&config);
        assert!(assets.iter().all(AssetStatus::missing));
        fs::write(&config.kernel_image, b"kernel").unwrap();
        let assets = status(&config);
        assert!(assets[0].exists);
        assert!(assets[1].missing());
    }
}
