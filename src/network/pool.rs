//! TAP pool reservations: owner files, per-slice locking and status rows.

use std::fs::{self, OpenOptions};
use std::net::Ipv4Addr;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::addressing::{
    control_guest_ip, control_host_ip, control_mac, control_tap_name, guest_ip, host_ip, mac,
    tap_name,
};
use super::privilege::ensure_ready;
use super::{DNS, NETWORK_DIR, TAP_POOL};
use crate::config::{Config, NetworkMode};
use crate::shared::fs::write_private;
use crate::vm::config::{VmConfig, VmStore};
use crate::vm::manager::VmManager;

/// A reserved TAP device for one VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapLease {
    pub index: u8,
    /// Owner VM name (the on-disk owner file format is unchanged).
    #[serde(alias = "instance")]
    pub vm: String,
    pub tap: String,
    pub mac: String,
    pub host_ip: Ipv4Addr,
    pub guest_ip: Ipv4Addr,
}

/// Compatibility alias: a data TAP lease used to be the only kind of lease.
pub type Lease = TapLease;

impl TapLease {
    /// `--net` value for Cloud Hypervisor. The host address is configured by
    /// `hegn network setup`; passing `ip=`/`mask=` here would rewrite the host
    /// side of the TAP from inside the VMM.
    pub fn cloud_hypervisor_arg(&self) -> String {
        format!("tap={},mac={}", self.tap, self.mac)
    }

    /// NoCloud v2 network config for the guarded data interface.
    pub fn guest_network_config(&self) -> String {
        format!("version: 2\nethernets:\n{}", self.data_ethernet())
    }

    /// NoCloud v2 network config for the control interface.
    ///
    /// The control interface is a host-only `/30` link: no default route and
    /// no resolvers, so it can never leak guest traffic onto the host LAN.
    pub fn control_network_config(&self) -> String {
        format!("version: 2\nethernets:\n{}", self.control_ethernet())
    }

    pub(crate) fn data_ethernet(&self) -> String {
        format!(
            "  hegn0:\n\
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
             \x20     addresses: [\"{}\", \"{}\"]\n\
             \x20   optional: true\n",
            self.mac, self.guest_ip, self.host_ip, DNS[0], DNS[1]
        )
    }

    pub(crate) fn control_ethernet(&self) -> String {
        format!(
            "  hegnc0:\n\
             \x20   match:\n\
             \x20     macaddress: \"{}\"\n\
             \x20   set-name: hegnc0\n\
             \x20   optional: true\n\
             \x20   dhcp4: false\n\
             \x20   dhcp6: false\n\
             \x20   accept-ra: false\n\
             \x20   addresses: [\"{}/30\"]\n",
            self.mac, self.guest_ip
        )
    }
}

/// The network reservation of one VM slot.
///
/// The control TAP always exists; the guarded data TAP only exists in
/// [`NetworkMode::Guarded`]. Both share one slot index, which is also the
/// index persisted in `vm.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkLease {
    pub slot: u8,
    pub vm: String,
    pub data: Option<TapLease>,
    pub control: TapLease,
}

impl NetworkLease {
    pub fn control_guest_ip(&self) -> Ipv4Addr {
        self.control.guest_ip
    }

    pub fn data_lease(&self) -> Option<&TapLease> {
        self.data.as_ref()
    }

    /// Every present TAP in attach order: data first, control last.
    pub fn taps(&self) -> Vec<&TapLease> {
        let mut taps = Vec::with_capacity(2);
        if let Some(data) = &self.data {
            taps.push(data);
        }
        taps.push(&self.control);
        taps
    }
}

fn network_dir(config: &Config) -> PathBuf {
    config.state_dir.join(NETWORK_DIR)
}

fn owner_path(config: &Config, index: u8) -> PathBuf {
    network_dir(config).join(format!("{}.owner", tap_name(index)))
}

fn control_owner_path(config: &Config, index: u8) -> PathBuf {
    network_dir(config).join(format!("{}.owner", control_tap_name(index)))
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
                .map(|(vm, _pid)| vm)
                .filter(|owner| !owner.is_empty()),
        })
        .collect()
}

/// The kernel interfaces a slot is expected to provide.
trait TapProbe {
    fn exists(&self, name: &str) -> bool;
}

struct SysfsTaps;

impl TapProbe for SysfsTaps {
    fn exists(&self, name: &str) -> bool {
        Path::new("/sys/class/net").join(name).exists()
    }
}

fn prepare_network_dir(config: &Config) -> Result<()> {
    fs::create_dir_all(network_dir(config))
        .with_context(|| format!("failed to create {}", network_dir(config).display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(network_dir(config), fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Reserve a network slot for a VM.
///
/// The control TAP always exists and is reserved for every mode. In guarded
/// mode the slot's data TAP is reserved and leased as well. The VM's recorded
/// index is preferred so a snapshot restored later sees the same device names.
/// If the recorded owner is another running VM, a required TAP is missing, or
/// the pool is exhausted, the start fails instead of silently sharing a TAP.
pub fn acquire(config: &Config, vm: &VmConfig) -> Result<NetworkLease> {
    // Fail closed before any reservation: guarded networking needs a live host
    // marker, and even an off VM still needs its control TAP pool.
    ensure_ready(config)?;
    prepare_network_dir(config)?;
    acquire_slot(config, vm, &SysfsTaps)
}

fn acquire_slot(config: &Config, vm: &VmConfig, taps: &impl TapProbe) -> Result<NetworkLease> {
    let guarded = config.network == NetworkMode::Guarded;

    if let Some(index) = vm.network_index {
        if index >= TAP_POOL {
            bail!(
                "VM '{}' records network index {index}, but the TAP pool has {TAP_POOL} slots",
                vm.name
            );
        }
        require_control_tap(taps, index)?;
        if try_claim(config, index, &vm.name)?.is_none() {
            bail!(
                "network slot {index} is reserved by another running VM (control TAP {} / data \
                 TAP {})",
                control_tap_name(index),
                tap_name(index)
            );
        }
        if guarded && !taps.exists(&tap_name(index)) {
            // The shared owner file now names this VM; give the slot back so a
            // failed boot does not leak a reservation.
            release_owner_if_owned(config, index, &vm.name);
            bail!(
                "guarded TAP {} is missing; run `sudo hegn network setup --user $USER`",
                tap_name(index)
            );
        }
        return Ok(network_lease(index, &vm.name, guarded));
    }

    let mut control_missing = false;
    let mut data_missing = false;
    let mut complete_slot_seen = false;
    for index in 0..TAP_POOL {
        if !taps.exists(&control_tap_name(index)) {
            control_missing = true;
            continue;
        }
        if guarded && !taps.exists(&tap_name(index)) {
            data_missing = true;
            continue;
        }
        complete_slot_seen = true;
        if try_claim(config, index, &vm.name)?.is_some() {
            return Ok(network_lease(index, &vm.name, guarded));
        }
    }
    if !complete_slot_seen && control_missing {
        bail!("the control TAP pool is incomplete; run `sudo hegn network setup --user $USER`");
    }
    if !complete_slot_seen && data_missing {
        bail!(
            "the guarded-network data TAP pool is incomplete; run `sudo hegn network setup \
             --user $USER`"
        );
    }
    bail!(
        "no free {}TAP is available for VM '{}'; increase the pool or stop another VM",
        if guarded {
            "guarded-network "
        } else {
            "control "
        },
        vm.name
    )
}

fn require_control_tap(taps: &impl TapProbe, index: u8) -> Result<()> {
    if !taps.exists(&control_tap_name(index)) {
        bail!(
            "control TAP {} is missing; run `sudo hegn network setup --user $USER`",
            control_tap_name(index)
        );
    }
    Ok(())
}

fn network_lease(index: u8, vm: &str, with_data: bool) -> NetworkLease {
    let control = TapLease {
        index,
        vm: vm.to_string(),
        tap: control_tap_name(index),
        mac: control_mac(index),
        host_ip: control_host_ip(index),
        guest_ip: control_guest_ip(index),
    };
    let data = with_data.then(|| TapLease {
        index,
        vm: vm.to_string(),
        tap: tap_name(index),
        mac: mac(index),
        host_ip: host_ip(index),
        guest_ip: guest_ip(index),
    });
    NetworkLease {
        slot: index,
        vm: vm.to_string(),
        data,
        control,
    }
}

fn try_claim(config: &Config, index: u8, vm: &str) -> Result<Option<TapLease>> {
    let path = owner_path(config, index);
    let lock = lock_path(config, index);
    let _guard = FileLock::acquire(&lock)?;

    if let Some((owner, pid)) = read_owner(&path)
        && !owner.is_empty()
        && owner != vm
    {
        // A live hegn process may still be booting the owner VM, before its
        // session record exists. A live VM is equally authoritative. The pid
        // alone is not proof: it may have been recycled by an unrelated
        // process, so require it to still execute the hegn binary.
        if owner_process_alive(pid) || vm_is_running(config, &owner) {
            return Ok(None);
        }
        // A crashed hegn process and a stopped VM left the reservation behind.
        let _ = fs::remove_file(&path);
    }

    let lease = TapLease {
        index,
        vm: vm.to_string(),
        tap: tap_name(index),
        mac: mac(index),
        host_ip: host_ip(index),
        guest_ip: guest_ip(index),
    };
    write_private(&path, format!("{vm}\n{}\n", std::process::id()))?;
    Ok(Some(lease))
}

fn release_owner_if_owned(config: &Config, index: u8, vm: &str) {
    // The claim was made under the slot lock, so release under it too.
    let Ok(_guard) = FileLock::acquire(&lock_path(config, index)) else {
        return;
    };
    let path = owner_path(config, index);
    if read_owner(&path).is_some_and(|(owner, _)| owner == vm) {
        let _ = fs::remove_file(&path);
    }
}

/// Release every reservation held by a VM, including stale ones.
pub fn release(config: &Config, vm: &str) {
    // Hold the per-slot lock while checking the owner: another hegn process may
    // be claiming the same slice concurrently, and removing its fresh owner
    // file would let a third VM share the TAP.
    for index in 0..TAP_POOL {
        let data_owner = owner_path(config, index);
        // The control pool shares the slot owner file. An explicit control
        // owner file is not written yet, but older/newer hegn builds may leave
        // one behind, so release both.
        let control_owner = control_owner_path(config, index);
        if !data_owner.exists() && !control_owner.exists() {
            continue;
        }
        let Ok(_guard) = FileLock::acquire(&lock_path(config, index)) else {
            continue;
        };
        for path in [data_owner, control_owner] {
            if read_owner(&path).is_some_and(|(owner, _)| owner == vm) {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// Whether the recorded owner pid is still the hegn process that wrote the
/// file. A recycled pid running any other program must not pin the slice.
fn owner_process_alive(pid: u32) -> bool {
    let Ok(binary) = std::env::current_exe() else {
        return false;
    };
    crate::vm::process_matches(pid, &binary)
}

/// Wait (bounded) until `pid` has completed its `exec`.
///
/// Immediately after `spawn`, `/proc/<pid>/exe` can still report the parent
/// image, which would make the identity check believe the expected binary is
/// running. Production never samples a freshly spawned pid this way; tests do,
/// so they must wait for the exec to become visible first.
#[cfg(test)]
fn wait_until_exec_visible(pid: u32) {
    let current = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|path| path.canonicalize().ok());
        if exe != current || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

pub fn vm_is_running(config: &Config, vm: &str) -> bool {
    let Ok(loaded) = VmStore::new(config).load(vm) else {
        return false;
    };
    VmManager::new(config.clone(), loaded).is_ok_and(|manager| manager.is_running())
}

/// Owner files contain `vm\npid\n`. The pid covers the window between
/// reservation and the session record becoming visible.
fn read_owner(path: &Path) -> Option<(String, u32)> {
    let raw = fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();
    let vm = lines.next()?.trim().to_string();
    let pid = lines
        .next()
        .and_then(|line| line.trim().parse().ok())
        .unwrap_or(0);
    Some((vm, pid))
}

fn tap_exists(index: u8) -> bool {
    SysfsTaps.exists(&tap_name(index))
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

    /// A config whose state and VM roots live in a throwaway directory, so
    /// no test can read or write the host's real hegn state.
    fn test_config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            vms_dir: dir.path().join("vms"),
            // `acquire` fails closed on guarded networking without a root
            // marker. The pool internals under test do not depend on the mode.
            network: NetworkMode::Off,
            ..Config::default()
        };
        (dir, config)
    }

    fn vm(name: &str, network_index: Option<u8>) -> VmConfig {
        VmConfig {
            name: name.to_string(),
            network_index,
            ..VmConfig::default()
        }
    }

    fn owner_file(config: &Config, index: u8) -> String {
        fs::read_to_string(owner_path(config, index)).unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A fake sysfs network directory, so slot selection can be tested
    /// without touching the host's real TAP devices.
    struct FakeTaps(Vec<String>);

    impl FakeTaps {
        fn full() -> Self {
            let mut names = Vec::new();
            for index in 0..TAP_POOL {
                names.push(control_tap_name(index));
                names.push(tap_name(index));
            }
            Self(names)
        }

        fn control_only() -> Self {
            Self((0..TAP_POOL).map(control_tap_name).collect())
        }
    }

    impl TapProbe for FakeTaps {
        fn exists(&self, name: &str) -> bool {
            self.0.iter().any(|known| known == name)
        }
    }

    #[test]
    fn guest_config_matches_the_reserved_slice() {
        let lease = Lease {
            index: 3,
            vm: "demo".into(),
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
    fn lease_serializes_vm_and_reads_the_legacy_instance_alias() {
        let legacy = r#"{"index":3,"instance":"demo","tap":"hegn3","mac":"52:54:00:00:00:04","host_ip":"10.213.0.13","guest_ip":"10.213.0.14"}"#;
        let lease: Lease = serde_json::from_str(legacy).unwrap();
        assert_eq!(lease.vm, "demo");

        let json = serde_json::to_value(&lease).unwrap();
        assert_eq!(json["vm"], "demo");
        assert!(json.get("instance").is_none());
    }

    #[test]
    fn try_claim_records_a_private_owner_file_and_lease() {
        let (_dir, config) = test_config();
        let lease = try_claim(&config, 3, "demo").unwrap().unwrap();

        assert_eq!(lease.index, 3);
        assert_eq!(lease.vm, "demo");
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
    fn try_claim_reuses_the_slice_for_the_same_vm() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        // A stale, dead pid recorded for the *same* VM is not a conflict:
        // the VM owns its stable pool slot.
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
    fn try_claim_reclaims_a_slice_pinned_by_a_recycled_pid() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        // The pid is alive but runs a different executable: pid reuse must not
        // keep the reservation alive after the hegn process exited.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep must be available");
        // `spawn` can return while the child's exec is still being installed;
        // in that window `/proc/<pid>/exe` transiently reports the parent (the
        // test binary) and the identity check would wrongly see "hegn running".
        wait_until_exec_visible(child.id());
        write_private(&owner_path(&config, 4), format!("other\n{}\n", child.id())).unwrap();

        let lease = try_claim(&config, 4, "demo")
            .expect("a recycled pid must not block the slice")
            .expect("the slice must be reclaimed");
        assert_eq!(lease.index, 4);

        let _ = child.kill();
        let _ = child.wait();
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
    fn release_removes_only_the_vms_own_reservations() {
        let (_dir, config) = test_config();
        try_claim(&config, 0, "demo").unwrap().unwrap();
        try_claim(&config, 1, "other").unwrap().unwrap();

        release(&config, "demo");

        assert!(!owner_path(&config, 0).exists());
        assert!(
            owner_path(&config, 1).exists(),
            "release must not touch another VM's slice"
        );

        // Releasing a VM that holds nothing is a no-op, not an error.
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
            let err = acquire_slot(&config, &vm("demo", Some(index)), &FakeTaps::full())
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
        let (_dir, mut config) = test_config();
        config.network = NetworkMode::Guarded;
        fs::create_dir_all(network_dir(&config)).unwrap();
        // Pretend another live hegn process reserved every slice. With every
        // TAP present, acquire must exhaust the pool and fail instead of
        // sharing a slot.
        for index in 0..TAP_POOL {
            write_private(
                &owner_path(&config, index),
                format!("other\n{}\n", std::process::id()),
            )
            .unwrap();
        }

        let err = acquire_slot(&config, &vm("demo", None), &FakeTaps::full())
            .expect_err("an exhausted pool must not hand out a shared lease");
        let message = format!("{err:#}");
        assert!(
            message.contains("no free guarded-network TAP is available"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn guarded_acquire_leases_data_and_control_for_the_same_slot() {
        let (_dir, mut config) = test_config();
        config.network = NetworkMode::Guarded;

        let lease = acquire_slot(&config, &vm("demo", None), &FakeTaps::full()).unwrap();

        assert_eq!(lease.slot, 0);
        assert_eq!(lease.vm, "demo");
        let data = lease
            .data_lease()
            .expect("guarded mode must lease a data TAP");
        assert_eq!(data.tap, "hegn0");
        assert_eq!(data.mac, "52:54:00:00:00:01");
        assert_eq!(data.host_ip, Ipv4Addr::new(10, 213, 0, 1));
        assert_eq!(data.guest_ip, Ipv4Addr::new(10, 213, 0, 2));
        assert_eq!(lease.control.tap, "hegnc0");
        assert_eq!(lease.control.mac, "52:54:00:00:01:01");
        assert_eq!(lease.control_guest_ip(), Ipv4Addr::new(10, 214, 0, 2));
        assert_eq!(
            lease
                .taps()
                .iter()
                .map(|lease| lease.tap.as_str())
                .collect::<Vec<_>>(),
            vec!["hegn0", "hegnc0"],
            "data is attached before control"
        );
        assert_eq!(
            owner_file(&config, 0),
            format!("demo\n{}\n", std::process::id()),
            "both networks share the slot owner file"
        );
    }

    #[test]
    fn off_acquire_leases_only_the_control_tap() {
        let (_dir, config) = test_config();
        let lease = acquire_slot(&config, &vm("demo", None), &FakeTaps::control_only()).unwrap();

        assert_eq!(lease.slot, 0);
        assert_eq!(lease.control.tap, "hegnc0");
        assert!(lease.data_lease().is_none());
        assert_eq!(lease.taps().len(), 1);
    }

    #[test]
    fn acquire_prefers_the_recorded_slot() {
        let (_dir, config) = test_config();
        let lease = acquire_slot(&config, &vm("demo", Some(7)), &FakeTaps::control_only()).unwrap();

        assert_eq!(lease.slot, 7);
        assert_eq!(lease.control.tap, "hegnc7");
        assert_eq!(lease.control_guest_ip(), Ipv4Addr::new(10, 214, 0, 30));
    }

    #[test]
    fn acquire_reports_a_missing_control_tap_before_writing_an_owner_file() {
        let (_dir, config) = test_config();
        let err = acquire_slot(&config, &vm("demo", Some(2)), &FakeTaps(Vec::new()))
            .expect_err("a missing control TAP must fail the start");
        let message = format!("{err:#}");
        assert!(
            message.contains("control TAP hegnc2 is missing"),
            "{message}"
        );
        assert!(!owner_path(&config, 2).exists());
    }

    #[test]
    fn acquire_fails_a_conflicting_slot_without_stealing_the_owner() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        let live = format!("other\n{}\n", std::process::id());
        write_private(&owner_path(&config, 3), &live).unwrap();

        let err = acquire_slot(&config, &vm("demo", Some(3)), &FakeTaps::full())
            .expect_err("a reserved slot must not be shared");
        let message = format!("{err:#}");
        assert!(
            message.contains("reserved by another running VM"),
            "{message}"
        );
        assert_eq!(
            owner_file(&config, 3),
            live,
            "the live owner must be intact"
        );
        assert!(!control_owner_path(&config, 3).exists());
    }

    #[test]
    fn a_missing_data_tap_releases_the_control_claim() {
        let (_dir, mut config) = test_config();
        config.network = NetworkMode::Guarded;

        let err = acquire_slot(&config, &vm("demo", Some(4)), &FakeTaps::control_only())
            .expect_err("a missing data TAP must fail the start");
        let message = format!("{err:#}");
        assert!(
            message.contains("guarded TAP hegn4 is missing"),
            "{message}"
        );
        assert!(
            !owner_path(&config, 4).exists(),
            "a failed data claim must release the shared owner file"
        );
    }

    #[test]
    fn release_removes_a_forward_compatible_control_owner_file() {
        let (_dir, config) = test_config();
        fs::create_dir_all(network_dir(&config)).unwrap();
        write_private(&control_owner_path(&config, 6), "demo\n4294967295\n").unwrap();
        write_private(&control_owner_path(&config, 7), "other\n4294967295\n").unwrap();

        release(&config, "demo");

        assert!(!control_owner_path(&config, 6).exists());
        assert!(control_owner_path(&config, 7).exists());
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
    fn vm_is_running_is_false_for_an_unknown_vm() {
        let (_dir, config) = test_config();
        assert!(!vm_is_running(&config, "demo"));
    }
}
