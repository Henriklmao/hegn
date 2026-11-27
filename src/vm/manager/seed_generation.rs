//! Per-VM cloud-init seed generation.
//!
//! The seed's instance id changes with the guest identity and its
//! configuration. `hegn vm apply` must also invalidate an unchanged seed so
//! cloud-init runs again and rotates the guest host keys. This module stores
//! the monotonic generation that feeds into that id.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::shared::fs::{create_private_dir, is_permission_denied, write_private_replacing};
use crate::vm::config::VmConfig;

const SEED_GENERATION_FILE: &str = "seed-generation";

/// Path of the per-VM generation file.
pub(super) fn path(config: &Config, vm: &VmConfig) -> PathBuf {
    vm.dir(&config.vms_dir).join(SEED_GENERATION_FILE)
}

/// Read the current generation. A missing file means generation `0`.
pub(super) fn read(config: &Config, vm: &VmConfig) -> Result<u64> {
    let path = path(config, vm);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to inspect seed generation {}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        anyhow::bail!("seed generation {} is not a regular file", path.display());
    }
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if is_permission_denied(&err) => {
            // The true generation is unknown, so never rewind it to zero:
            // cloud-init derives the instance id from it. The doctor can hand
            // the file back without losing the value.
            return Err(err).with_context(|| {
                format!(
                    "seed generation {} is unreadable (a root-run hegn may own it); run `hegn \
                     doctor --fix`",
                    path.display()
                )
            });
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read seed generation {}", path.display()));
        }
    };
    raw.trim().parse::<u64>().with_context(|| {
        format!(
            "seed generation {} is not an unsigned integer",
            path.display()
        )
    })
}

/// Increment and persist the generation, returning the new value.
pub(super) fn bump(config: &Config, vm: &VmConfig) -> Result<u64> {
    let vm_dir = vm.dir(&config.vms_dir);
    create_private_dir(&vm_dir)
        .with_context(|| format!("failed to create VM directory {}", vm_dir.display()))?;
    let next = read(config, vm)?
        .checked_add(1)
        .context("seed generation overflowed; move the VM aside and create a fresh one")?;
    let path = path(config, vm);
    write_private_replacing(&path, format!("{next}\n"))
        .with_context(|| format!("failed to write seed generation {}", path.display()))?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(_config: &Config) -> VmConfig {
        VmConfig {
            name: "demo".into(),
            ..VmConfig::default()
        }
    }

    #[test]
    fn a_missing_generation_reads_as_zero_and_bump_is_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vms_dir: dir.path().join("vms"),
            ..Config::default()
        };
        let vm = vm(&config);

        assert_eq!(read(&config, &vm).unwrap(), 0);
        assert_eq!(bump(&config, &vm).unwrap(), 1);
        assert_eq!(read(&config, &vm).unwrap(), 1);
        assert_eq!(bump(&config, &vm).unwrap(), 2);
        assert_eq!(read(&config, &vm).unwrap(), 2);
    }

    #[test]
    fn the_generation_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vms_dir: dir.path().join("vms"),
            ..Config::default()
        };
        let vm = vm(&config);
        bump(&config, &vm).unwrap();

        let mode = fs::metadata(path(&config, &vm))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn an_unreadable_generation_fails_closed_until_repaired() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vms_dir: dir.path().join("vms"),
            ..Config::default()
        };
        let vm = vm(&config);
        let vm_dir = vm.dir(&config.vms_dir);
        fs::create_dir_all(&vm_dir).unwrap();
        let path = path(&config, &vm);
        fs::write(&path, "5\n").unwrap();
        // A root-run hegn leaves the file 0600; the owning user cannot read it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let err = read(&config, &vm).expect_err("the true generation is unknown");
        assert!(format!("{err:#}").contains("hegn doctor --fix"), "{err:#}");

        // The doctor hands the file back without rewriting it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read(&config, &vm).unwrap(), 5);
        assert_eq!(bump(&config, &vm).unwrap(), 6);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_symlinked_generation_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vms_dir: dir.path().join("vms"),
            ..Config::default()
        };
        let vm = vm(&config);
        let vm_dir = vm.dir(&config.vms_dir);
        fs::create_dir_all(&vm_dir).unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "7\n").unwrap();
        std::os::unix::fs::symlink(&target, path(&config, &vm)).unwrap();

        assert!(read(&config, &vm).is_err());
        assert!(bump(&config, &vm).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "7\n");
    }
}
