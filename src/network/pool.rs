//! TAP pool reservations: owner files, per-slice locking and status rows.

use std::fs::{self, OpenOptions};
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
use crate::shared::fs::write_private;
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
    write_private(&path, format!("{instance}\n{}\n", std::process::id()))?;
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
    use crate::config::NetworkMode;
    use std::os::unix::fs::PermissionsExt;

    /// A config whose state and instance roots live in a throwaway directory, so
    /// no test can read or write the host's real hegn state.
    fn test_config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            instances_dir: dir.path().join("instances"),
            // `acquire` fails closed on guarded networking without a root
            // marker. The pool internals under test do not depend on the mode.
            network: NetworkMode::Off,
            ..Config::default()
        };
        (dir, config)
    }

    fn instance(name: &str, network_index: Option<u8>) -> InstanceConfig {
        InstanceConfig {
            name: name.to_string(),
            network_index,
            ..InstanceConfig::default()
        }
    }

    fn owner_file(config: &Config, index: u8) -> String {
        fs::read_to_string(owner_path(config, index)).unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

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

    #[test]
    fn try_claim_records_a_private_owner_file_and_lease() {
        let (_dir, config) = test_config();
        let lease = try_claim(&config, 3, "demo").unwrap().unwrap();

        assert_eq!(lease.index, 3);
        assert_eq!(lease.instance, "demo");
        assert_eq!(lease.tap, "hegn3");
        assert_eq!(lease.mac, "52:54:00:00:00:04");
        assert_eq!(lease.host_ip, Ipv4Addr::new(10, 213, 0, 13));
        assert_eq!(lease.guest_ip, Ipv4Addr::new(10, 213, 0, 14));

        let path = owner_path(&config, 3);
        assert_eq!(path, network_dir(&config).join("hegn3.owner"));
        assert_eq!(
            owner_file(&config, 3),
            format!("demo\n{}\n", std::process::id())
        );
        // The owner file names a process, so it must never be group/world
        // readable or writable.
        assert_eq!(mode_of(&path), 0o600, "owner file must be private");
        assert_eq!(
            mode_of(&lock_path(&config, 3)),
            0o600,
            "lock file must be private"
        );
    }

    #[test]
    fn try_claim_reuses_the_slice_for_the_same_instance() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        // A stale, dead pid recorded for the *same* instance is not a conflict:
        // the instance owns its stable pool slot.
        write_private(&owner_path(&config, 5), "demo\n4294967295\n").unwrap();

        let lease = try_claim(&config, 5, "demo").unwrap().unwrap();
        assert_eq!(lease.index, 5);
        assert_eq!(
            owner_file(&config, 5),
            format!("demo\n{}\n", std::process::id()),
            "re-claiming must refresh the recorded pid"
        );
    }

    #[test]
    fn try_claim_refuses_a_slice_owned_by_a_live_process() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        let live = format!("other\n{}\n", std::process::id());
        write_private(&owner_path(&config, 2), &live).unwrap();

        assert!(
            try_claim(&config, 2, "demo").unwrap().is_none(),
            "a live owner must win the reservation"
        );
        assert_eq!(owner_file(&config, 2), live, "live owner must be untouched");
    }

    #[test]
    fn try_claim_reclaims_a_slice_whose_owner_is_dead() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        // A pid outside the i32 range can never be alive; a crashed session
        // disappears but leaves its owner file behind.
        write_private(&owner_path(&config, 7), "ghost\n4294967295\n").unwrap();

        let lease = try_claim(&config, 7, "demo").unwrap().unwrap();
        assert_eq!(lease.index, 7);
        assert_eq!(
            owner_file(&config, 7),
            format!("demo\n{}\n", std::process::id())
        );
    }

    #[test]
    fn try_claim_reclaims_an_owner_file_without_a_pid() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        // Partial writes or hand-edited files default to pid 0, which is never
        // a live process. Treating them as live would leak the slot forever.
        write_private(&owner_path(&config, 9), "ghost\n").unwrap();

        assert!(try_claim(&config, 9, "demo").unwrap().is_some());
        assert_eq!(
            owner_file(&config, 9),
            format!("demo\n{}\n", std::process::id())
        );
    }

    #[test]
    fn release_removes_only_the_instances_own_reservations() {
        let (_dir, config) = test_config();
        try_claim(&config, 0, "demo").unwrap().unwrap();
        try_claim(&config, 1, "other").unwrap().unwrap();

        release(&config, "demo");

        assert!(!owner_path(&config, 0).exists());
        assert!(
            owner_path(&config, 1).exists(),
            "release must not touch another instance's slice"
        );

        // Releasing an instance that holds nothing is a no-op, not an error.
        release(&config, "demo");
        release(&config, "never-existed");
    }

    #[test]
    fn release_clears_stale_reservations_too() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        write_private(&owner_path(&config, 11), "demo\n4294967295\n").unwrap();

        release(&config, "demo");

        assert!(!owner_path(&config, 11).exists());
    }

    #[test]
    fn acquire_rejects_a_recorded_index_outside_the_pool() {
        let (_dir, config) = test_config();
        for index in [TAP_POOL, TAP_POOL + 1, u8::MAX] {
            let err = acquire(&config, &instance("demo", Some(index)))
                .expect_err("out-of-range index must be rejected");
            let message = format!("{err:#}");
            assert!(
                message.contains(&format!("network index {index}"))
                    && message.contains("TAP pool has 16 slots"),
                "unexpected error: {message}"
            );
        }
        assert!(
            !network_dir(&config).join("hegn0.owner").exists(),
            "a rejected index must not write an owner file"
        );
    }

    #[test]
    fn acquire_fails_when_all_slices_are_reserved() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        // Pretend another live hegn process reserved every slice. Whether or not
        // this host actually has TAP devices, acquire must exhaust the pool and
        // fail instead of sharing a TAP.
        for index in 0..TAP_POOL {
            write_private(
                &owner_path(&config, index),
                format!("other\n{}\n", std::process::id()),
            )
            .unwrap();
        }

        let err = acquire(&config, &instance("demo", None))
            .expect_err("an exhausted pool must not hand out a shared lease");
        let message = format!("{err:#}");
        assert!(
            message.contains("no free guarded-network TAP is available"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn status_reports_every_slot_and_its_owner() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        write_private(&owner_path(&config, 3), "demo\n1234\n").unwrap();
        // An owner file without a name is treated as unowned.
        write_private(&owner_path(&config, 4), "\n1234\n").unwrap();

        let rows = status(&config);
        assert_eq!(rows.len(), TAP_POOL as usize);
        for (index, row) in rows.iter().enumerate() {
            let index = index as u8;
            assert_eq!(row.index, index);
            assert_eq!(row.tap, tap_name(index));
            assert_eq!(row.host_ip, host_ip(index));
        }
        assert_eq!(rows[3].owner.as_deref(), Some("demo"));
        assert_eq!(rows[4].owner, None);
        assert_eq!(rows[5].owner, None);
    }

    #[test]
    fn read_owner_tolerates_malformed_but_not_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner");

        assert_eq!(read_owner(&path), None, "missing owner file is unowned");

        fs::write(&path, "").unwrap();
        assert_eq!(read_owner(&path), None, "empty owner file is unowned");

        fs::write(&path, "  demo  \n  42  \nextra\n").unwrap();
        assert_eq!(read_owner(&path), Some(("demo".to_string(), 42)));

        fs::write(&path, "demo\ngarbage\n").unwrap();
        assert_eq!(
            read_owner(&path),
            Some(("demo".to_string(), 0)),
            "an unparseable pid must not be treated as live"
        );

        fs::write(&path, "demo\n").unwrap();
        assert_eq!(read_owner(&path), Some(("demo".to_string(), 0)));
    }

    #[test]
    fn write_private_creates_private_files_and_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner");

        write_private(&path, "demo\n1\n").unwrap();
        assert_eq!(mode_of(&path), 0o600);

        write_private(&path, "x\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "x\n");
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn write_private_refuses_to_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "do not overwrite").unwrap();
        let link = dir.path().join("owner");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        assert!(
            write_private(&link, "attacker\n1\n").is_err(),
            "O_NOFOLLOW must reject a symlinked owner file"
        );
        assert_eq!(fs::read_to_string(&victim).unwrap(), "do not overwrite");
    }

    #[test]
    fn file_lock_creates_private_lock_files_in_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network").join("hegn0.lock");

        {
            let _guard = FileLock::acquire(&path).unwrap();
            assert!(path.exists());
            assert_eq!(mode_of(&path), 0o600);
        }

        // Dropping the guard releases the lock, so it can be acquired again.
        let _guard = FileLock::acquire(&path).unwrap();
    }

    #[test]
    fn instance_is_running_is_false_for_an_unknown_instance() {
        let (_dir, config) = test_config();
        assert!(!instance_is_running(&config, "demo"));
    }
}
