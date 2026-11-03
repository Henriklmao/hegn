//! Pinned upstream assets and their on-disk status.
//!
//! The kernel and root filesystem are pinned per architecture by URL and
//! SHA-256. [`status`] reports whether each configured asset is present
//! without opening it, so the TUI and doctor can work from metadata alone.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;

#[cfg(target_arch = "x86_64")]
mod pinned {
    pub const KERNEL_VERSION: &str = "ch-release-v7.2.8-20261009";
    pub const KERNEL_ARTIFACT: &str = "vmlinux-x86_64";
    pub const KERNEL_URL: &str = "https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v7.2.8-20261009/vmlinux-x86_64";
    pub const KERNEL_SHA256: &str =
        "edfeb8a8a6c04ff2979e9f1c700feaa29b8f564f952261834ef0b7d2e673a853";

    pub const ROOTFS_VERSION: &str = "20241017";
    pub const ROOTFS_ARTIFACT: &str = "jammy-server-cloudimg-amd64-custom-20241017-0.qcow2";
    pub const ROOTFS_URL: &str =
        "https://ch-images.azureedge.net/jammy-server-cloudimg-amd64-custom-20241017-0.qcow2";
    pub const ROOTFS_SHA256: &str =
        "e05cb642d633e20e8e1390be23856744d3867744764bf3000ad0a9c22dd794e8";
    pub const ROOTFS_FORMAT: crate::config::RootfsFormat = crate::config::RootfsFormat::Qcow2;
}

#[cfg(target_arch = "aarch64")]
mod pinned {
    pub const KERNEL_VERSION: &str = "ch-release-v7.2.8-20261009";
    pub const KERNEL_ARTIFACT: &str = "Image-arm64";
    pub const KERNEL_URL: &str = "https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v7.2.8-20261009/Image-arm64";
    pub const KERNEL_SHA256: &str =
        "8f8f3d5a983809f87a4e1e9a13053a3361fda47ef04e09c771a41c5e037cb6c2";

    pub const ROOTFS_VERSION: &str = "20220329";
    pub const ROOTFS_ARTIFACT: &str = "jammy-server-cloudimg-arm64-custom-20220329-0.raw";
    pub const ROOTFS_URL: &str =
        "https://ch-images.azureedge.net/jammy-server-cloudimg-arm64-custom-20220329-0.raw";
    pub const ROOTFS_SHA256: &str =
        "09b0d626d6cef8d0426aa47b8694194eb476e5696ff19d743e3cd4a175d00645";
    pub const ROOTFS_FORMAT: crate::config::RootfsFormat = crate::config::RootfsFormat::Raw;
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

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn pinned_rootfs_format_matches_the_artifact() {
        match ROOTFS_FORMAT {
            crate::config::RootfsFormat::Qcow2 => {
                assert!(ROOTFS_ARTIFACT.ends_with(".qcow2"), "{ROOTFS_ARTIFACT}")
            }
            crate::config::RootfsFormat::Raw => {
                assert!(
                    [".raw", ".img", ".ext4"]
                        .iter()
                        .any(|suffix| ROOTFS_ARTIFACT.ends_with(suffix)),
                    "{ROOTFS_ARTIFACT}"
                )
            }
        }
        assert!(KERNEL_URL.starts_with("https://"));
        assert!(ROOTFS_URL.starts_with("https://"));
        assert!(crate::vm::images::is_sha256(KERNEL_SHA256));
        assert!(crate::vm::images::is_sha256(ROOTFS_SHA256));
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

    #[test]
    fn is_regular_file_never_accepts_symlinks_or_directories() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("kernel");
        fs::write(&file, b"kernel").unwrap();
        assert!(is_regular_file(&file));

        let directory = dir.path().join("images");
        fs::create_dir(&directory).unwrap();
        assert!(!is_regular_file(&directory));

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(!is_regular_file(&link));

        assert!(!is_regular_file(&dir.path().join("absent")));
    }

    #[test]
    fn status_does_not_report_symlinked_assets_as_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.kernel_image = dir.path().join("kernel");
        config.rootfs_image = dir.path().join("rootfs");
        let real = dir.path().join("real-kernel");
        fs::write(&real, b"kernel").unwrap();
        std::os::unix::fs::symlink(&real, &config.kernel_image).unwrap();

        let assets = status(&config);
        assert!(
            assets[0].missing(),
            "a symlinked kernel must not look installed"
        );
        assert_eq!(assets[0].size_bytes, None);
    }
}
