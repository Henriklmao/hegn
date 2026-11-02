//! TAP pool reservations: owner files, per-slice locking and status rows.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::Ipv4Addr;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::addressing::{guest_ip, host_ip, mac, tap_name};
use super::privilege::ensure_ready;
use super::{DNS, NETWORK_DIR, TAP_POOL};
use crate::config::Config;
use crate::vm::instance::InstanceManager;
use crate::vm::instance_config::{InstanceConfig, InstanceStore};

/// A reserved TAP device for one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lease {
    pub index: u8,
    pub instance: String,
    pub tap: String,
    pub mac: String,
    pub host_ip: Ipv4Addr,
    pub guest_ip: Ipv4Addr,
}

impl Lease {
    /// `--net` value for Cloud Hypervisor. The host address is configured by
    /// `hegn network setup`; passing `ip=`/`mask=` here would rewrite the host
    /// side of the TAP from inside the VMM.
    pub fn cloud_hypervisor_arg(&self) -> String {
        format!("tap={},mac={}", self.tap, self.mac)
    }

    /// NoCloud v2 network config for the guest. Matching on the MAC keeps
    /// this independent of the guest's predictable interface name.
    pub fn guest_network_config(&self) -> String {
        format!(
            "version: 2\n\
             ethernets:\n\
             \x20 hegn0:\n\
             \x20   match:\n\
             \x20     macaddress: \"{}\"\n\
             \x20   set-name: hegn0\n\
             \x20   dhcp4: false\n\
             \x20   dhcp6: false\n\
             \x20   accept-ra: false\n\
             \x20   addresses: [\"{}/30\"]\n\
             \x20   routes:\n\
             \x20     - to: default\n\
             \x20       via: {}\n\
             \x20   nameservers:\n\
             \x20     addresses: [\"{}\", \"{}\"]\n",
            self.mac, self.guest_ip, self.host_ip, DNS[0], DNS[1]
        )
    }
}

fn network_dir(config: &Config) -> PathBuf {
    config.state_dir.join(NETWORK_DIR)
}

fn owner_path(config: &Config, index: u8) -> PathBuf {
    network_dir(config).join(format!("{}.owner", tap_name(index)))
}

/// One row of `hegn network status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TapStatus {
    pub index: u8,
    pub tap: String,
    pub host_ip: Ipv4Addr,
    pub exists: bool,
    pub owner: Option<String>,
}

pub fn status(config: &Config) -> Vec<TapStatus> {
    (0..TAP_POOL)
        .map(|index| TapStatus {
            index,
            tap: tap_name(index),
            host_ip: host_ip(index),
            exists: tap_exists(index),
            owner: read_owner(&owner_path(config, index))
                .map(|(instance, _pid)| instance)
                .filter(|owner| !owner.is_empty()),
        })
        .collect()
}

/// Reserve a TAP slice for an instance.
///
/// The instance's recorded index is preferred so a snapshot restored later sees
/// the same device name. If the recorded owner is another running instance, or
/// the pool is exhausted, the start fails instead of silently sharing a TAP.
pub fn acquire(config: &Config, instance: &InstanceConfig) -> Result<Lease> {
    ensure_ready(config)?;
    fs::create_dir_all(network_dir(config))
        .with_context(|| format!("failed to create {}", network_dir(config).display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(network_dir(config), fs::Permissions::from_mode(0o700))?;
    }

    let preferred = instance.network_index;
    if let Some(index) = preferred {
        if index >= TAP_POOL {
            bail!(
                "instance '{}' records network index {index}, but the TAP pool has {TAP_POOL} slots",
                instance.name
            );
        }
        if !tap_exists(index) {
            bail!(
                "guarded TAP {} is missing; run `sudo hegn network setup --user $USER`",
                tap_name(index)
            );
        }
        return try_claim(config, index, &instance.name)?.with_context(|| {
            format!(
                "guarded TAP {} is reserved by another running instance",
                tap_name(index)
            )
        });
    }

    for index in 0..TAP_POOL {
        if !tap_exists(index) {
            continue;
        }
        if let Some(lease) = try_claim(config, index, &instance.name)? {
            return Ok(lease);
        }
    }
    bail!(
        "no free guarded-network TAP is available for instance '{}'; increase the pool or stop \
         another instance",
        instance.name
    )
}

fn try_claim(config: &Config, index: u8, instance: &str) -> Result<Option<Lease>> {
    let path = owner_path(config, index);
    let lock = lock_path(config, index);
    let _guard = FileLock::acquire(&lock)?;

    if let Some((owner, pid)) = read_owner(&path)
        && !owner.is_empty()
        && owner != instance
    {
        // A live hegn process may still be booting the owner VM, before its
        // session record exists. A live VM is equally authoritative.
        if crate::vm::instance::process_alive(pid) || instance_is_running(config, &owner) {
            return Ok(None);
        }
        // A crashed hegn process and a stopped VM left the reservation behind.
        let _ = fs::remove_file(&path);
    }

    let lease = Lease {
        index,
        instance: instance.to_string(),
        tap: tap_name(index),
        mac: mac(index),
        host_ip: host_ip(index),
        guest_ip: guest_ip(index),
    };
    write_private(&path, &format!("{instance}\n{}\n", std::process::id()))?;
    Ok(Some(lease))
}

/// Release every reservation held by an instance, including stale ones.
pub fn release(config: &Config, instance: &str) {
    // Hold the per-TAP lock while checking the owner: another hegn process may
    // be claiming the same slice concurrently, and removing its fresh owner
    // file would let a third instance share the TAP.
    for index in 0..TAP_POOL {
        let path = owner_path(config, index);
        if !path.exists() {
            continue;
        }
        let Ok(_guard) = FileLock::acquire(&lock_path(config, index)) else {
            continue;
        };
        if read_owner(&path).is_some_and(|(owner, _)| owner == instance) {
            let _ = fs::remove_file(&path);
        }
    }
}

pub fn instance_is_running(config: &Config, instance: &str) -> bool {
    InstanceStore::new(config.clone())
        .load(instance)
        .map(|instance_config| InstanceManager::new(config.clone(), instance_config).is_running())
        .unwrap_or(false)
}

/// Owner files contain `instance\npid\n`. The pid covers the window between
/// reservation and the session record becoming visible.
fn read_owner(path: &Path) -> Option<(String, u32)> {
    let raw = fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();
    let instance = lines.next()?.trim().to_string();
    let pid = lines
        .next()
        .and_then(|line| line.trim().parse().ok())
        .unwrap_or(0);
    Some((instance, pid))
}

fn tap_exists(index: u8) -> bool {
    Path::new("/sys/class/net").join(tap_name(index)).exists()
}

fn lock_path(config: &Config, index: u8) -> PathBuf {
    network_dir(config).join(format!("{}.lock", tap_name(index)))
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

struct FileLock {
    _file: fs::File,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        // Safety: `file` is a valid open descriptor for a private lock file.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if locked != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_config_matches_the_reserved_slice() {
        let lease = Lease {
            index: 3,
            instance: "demo".into(),
            tap: tap_name(3),
            mac: mac(3),
            host_ip: host_ip(3),
            guest_ip: guest_ip(3),
        };
        let config = lease.guest_network_config();
        assert!(config.contains("10.213.0.14/30"), "{config}");
        assert!(config.contains("via: 10.213.0.13"), "{config}");
        assert!(config.contains("1.1.1.1"));
        assert!(config.contains("52:54:00:00:00:04"));
        assert_eq!(
            lease.cloud_hypervisor_arg(),
            "tap=hegn3,mac=52:54:00:00:00:04"
        );
    }
}
