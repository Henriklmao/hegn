//! SHA-256 digest validation for downloaded assets.
//!
//! [`verify_file`] re-hashes an installed asset without following symlinks, and
//! [`is_sha256`] is the strict digest-shape check used by config validation and
//! downloads.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::MAX_ASSET_BYTES;

/// `true` for a well-formed lowercase or uppercase SHA-256 hex digest.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn verify_file_checks_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel");
        fs::write(&path, b"hello").unwrap();
        assert_eq!(verify_file(&path, SHA_HELLO).unwrap(), 5);
        assert!(verify_file(&path, &"0".repeat(64)).is_err());
    }

    #[test]
    fn verify_file_rejects_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = verify_file(&dir.path().join("absent"), SHA_HELLO).unwrap_err();
        assert!(
            format!("{err:#}").contains("failed to open"),
            "missing assets must fail loudly: {err:#}"
        );
    }

    #[test]
    fn verify_file_rejects_a_malformed_configured_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel");
        fs::write(&path, b"hello").unwrap();
        for bad in ["", "abc", &"g".repeat(64)] {
            let err = verify_file(&path, bad).unwrap_err();
            assert!(
                err.to_string().contains("no valid SHA-256"),
                "{bad:?} must be rejected before hashing: {err}"
            );
        }
    }

    #[test]
    fn verify_file_refuses_symlinks_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"hello").unwrap();
        let link = dir.path().join("kernel");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = verify_file(&link, SHA_HELLO).unwrap_err();
        assert!(
            format!("{err:#}").contains("failed to open"),
            "symlinked assets must never be hashed through: {err:#}"
        );

        let directory = dir.path().join("images");
        fs::create_dir(&directory).unwrap();
        let err = verify_file(&directory, SHA_HELLO).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn verify_file_accepts_uppercase_and_surrounding_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel");
        fs::write(&path, b"hello").unwrap();
        let configured = format!("  {} \n", SHA_HELLO.to_uppercase());
        assert_eq!(verify_file(&path, &configured).unwrap(), 5);
    }
}
