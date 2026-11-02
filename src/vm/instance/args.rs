//! Command-line construction and process spawning for the VM stack.
//!
//! Cloud Hypervisor, virtiofsd and the console relay are all started from here
//! so the hardened arguments (seccomp, Landlock, private log files) stay in one
//! place and can be unit-tested without booting a VM.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::agent::profile::PROFILE_SHARE_TAG;
use crate::config::{Config, VIRTIOFSD_FALLBACKS};
use crate::network;
use crate::session::relay::{self, RelayArgs};
use crate::session::snapshot::{SnapshotMeta, SnapshotStore};
use crate::vm::cloud_init;
use crate::vm::hypervisor::CloudHypervisorClient;
use crate::vm::instance_config::InstanceConfig;
use crate::vm::virtiofs::{VirtiofsConfig, VirtiofsProcess};

use super::paths::InstancePaths;
use super::process::{hegn_binary, is_socket, process_alive, remove_socket, terminate_relay};
use super::util::log_tail;

pub(super) const API_START_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const RELAY_START_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_arch = "x86_64")]
const SERIAL_CONSOLE: &str = "ttyS0";
#[cfg(target_arch = "aarch64")]
const SERIAL_CONSOLE: &str = "ttyAMA0";

pub(super) fn boot_args(root_device: &str) -> String {
    format!("console={SERIAL_CONSOLE} reboot=k panic=1 root={root_device} rw")
}

#[cfg(test)]
fn virtiofs_config(
    config: &Config,
    paths: &InstancePaths,
    project: &Path,
) -> Result<VirtiofsConfig> {
    virtiofs_config_at(config, project, &paths.virtiofs_socket)
}

pub(super) fn virtiofs_config_at(
    config: &Config,
    shared_dir: &Path,
    socket_path: &Path,
) -> Result<VirtiofsConfig> {
    let binary = config.resolved_virtiofsd().with_context(|| {
        format!(
            "virtiofsd binary {} was not found on PATH or in {}; install virtiofsd or set \
             `virtiofsd_bin`",
            config.virtiofsd_bin.display(),
            VIRTIOFSD_FALLBACKS.join(", ")
        )
    })?;
    let mut virtiofs = VirtiofsConfig::new(binary, shared_dir, socket_path);
    // Honour the configured sandbox mode. The default is namespace; chroot is
    // available for hosts where user namespaces are restricted.
    virtiofs.sandbox = config.virtiofs_sandbox;
    Ok(virtiofs)
}

pub(super) fn spawn_virtiofsd(
    config: &Config,
    paths: &InstancePaths,
    project: &Path,
) -> Result<VirtiofsProcess> {
    spawn_virtiofsd_at(config, project, &paths.virtiofs_socket)
}

pub(super) fn spawn_virtiofsd_at(
    config: &Config,
    shared_dir: &Path,
    socket_path: &Path,
) -> Result<VirtiofsProcess> {
    let virtiofs = virtiofs_config_at(config, shared_dir, socket_path)?;
    let mut process = virtiofs.spawn().context("failed to start virtiofsd")?;
    if let Err(err) = process.wait_for_socket(Duration::from_secs(5)) {
        process.kill();
        return Err(err);
    }
    Ok(process)
}

/// Build the hardened command line for a normal boot.
pub(super) fn cloud_hypervisor_args(
    config: &Config,
    instance: &InstanceConfig,
    paths: &InstancePaths,
    rootfs: &Path,
    network: Option<&network::Lease>,
    profile_socket: Option<&Path>,
) -> Vec<OsString> {
    let mut args = common_args(config, paths, true);
    args.push("--kernel".into());
    args.push(config.kernel_image.clone().into_os_string());
    args.push("--cmdline".into());
    args.push(boot_args(&config.root_device).into());

    args.push("--disk".into());
    args.push(
        format!(
            "path={},image_type={}",
            rootfs.display(),
            config.rootfs_format.as_str()
        )
        .into(),
    );
    if config.cloud_init {
        args.push(cloud_init::disk_arg(&paths.cloud_init).into());
    }

    args.push("--cpus".into());
    args.push(format!("boot={},max={}", instance.vcpus, instance.vcpus).into());
    args.push("--memory".into());
    args.push(format!("size={}M,shared=on", instance.memory_mib).into());
    args.push("--fs".into());
    args.push(
        format!(
            "tag={},socket={},num_queues=1,queue_size=1024",
            instance.shared_tag,
            paths.virtiofs_socket.display()
        )
        .into(),
    );
    if let Some(socket) = profile_socket {
        args.push("--fs".into());
        args.push(
            format!(
                "tag={},socket={},num_queues=1,queue_size=1024",
                PROFILE_SHARE_TAG,
                socket.display()
            )
            .into(),
        );
    }
    if let Some(lease) = network {
        args.push("--net".into());
        args.push(lease.cloud_hypervisor_arg().into());
        // Cloud Hypervisor checks an existing TAP's multiqueue support by
        // reading `/sys/class/net/<tap>/tun_flags`, which sysfs resolves to
        // `/sys/devices/virtual/net/<tap>/tun_flags`. Its own Landlock ruleset
        // only grants `/dev/net/tun`, so that read fails with EACCES and the VM
        // dies while creating the virtio-net device. Grant exactly this
        // instance's TAP directory, read-only: other interfaces and the rest of
        // /sys stay invisible to the VMM.
        if config.landlock {
            args.push("--landlock-rules".into());
            args.push(format!("path=/sys/devices/virtual/net/{},access=r", lease.tap).into());
        }
    }
    args.push("--serial".into());
    args.push(format!("socket={}", paths.serial_socket.display()).into());
    args.push("--console".into());
    args.push("off".into());
    args
}

/// Build the command line for a restore. The VM configuration comes from the
/// snapshot itself, so only VMM-level options are set here.
pub(super) fn restore_args(
    config: &Config,
    paths: &InstancePaths,
    snapshot: &SnapshotMeta,
) -> Vec<OsString> {
    let mut args = common_args(config, paths, false);
    args.push("--restore".into());
    args.push(format!("source_url={},resume=true", snapshot.source_url()).into());
    args
}

pub(super) fn common_args(config: &Config, paths: &InstancePaths, landlock: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--api-socket".into(),
        paths.api_socket.clone().into_os_string(),
        "--log-file".into(),
        paths.log.clone().into_os_string(),
        "--seccomp".into(),
        if config.seccomp { "true" } else { "false" }.into(),
    ];
    if config.landlock && landlock {
        args.push("--landlock".into());
        // Cloud Hypervisor applies Landlock at VM create. Snapshot writes
        // happen later, so the snapshot root has to be granted up front.
        args.push("--landlock-rules".into());
        let staging = SnapshotStore::new(&config.snapshots_dir).staging_dir();
        args.push(format!("path={},access=rw", staging.display()).into());
    }
    args
}

pub(super) fn spawn_cloud_hypervisor(
    config: &Config,
    instance: &InstanceConfig,
    paths: &InstancePaths,
    binary: &Path,
    restore: Option<&SnapshotMeta>,
    network: Option<&network::Lease>,
    profile_socket: Option<&Path>,
) -> Result<Child> {
    // Cloud Hypervisor must create the API and serial listeners itself.
    remove_socket(&paths.api_socket)?;
    remove_socket(&paths.serial_socket)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&paths.log)
        .with_context(|| format!("failed to open {}", paths.log.display()))?;
    let stderr = log.try_clone()?;
    let args = match restore {
        Some(snapshot) => restore_args(config, paths, snapshot),
        None => {
            let rootfs = instance.prepare_rootfs(config)?;
            cloud_hypervisor_args(config, instance, paths, &rootfs, network, profile_socket)
        }
    };
    Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn {} (is Cloud Hypervisor installed?)",
                binary.display()
            )
        })
}

pub(super) fn start_relay(paths: &InstancePaths, serial_log: &Path, vm: &mut Child) -> Result<u32> {
    remove_socket(&paths.console_socket)?;
    let args = RelayArgs {
        backend: paths.serial_socket.clone(),
        frontend: paths.console_socket.clone(),
        log: serial_log.to_path_buf(),
    };
    let pid = relay::spawn(&args, &paths.relay_log)?;
    let binary = hegn_binary()?;
    let deadline = Instant::now() + RELAY_START_TIMEOUT;
    while Instant::now() < deadline {
        if is_socket(&paths.console_socket) && process_alive(pid) {
            return Ok(pid);
        }
        // The relay connects to Cloud Hypervisor's serial socket and exits as
        // soon as the backend disappears. A dead VMM therefore looks like a
        // relay failure; report the real cause (and the VMM log) instead.
        if let Ok(Some(status)) = vm.try_wait() {
            bail!(
                "Cloud Hypervisor exited during startup ({status}); last lines of {}: {}",
                paths.log.display(),
                log_tail(&paths.log)
            );
        }
        if !process_alive(pid) {
            bail!(
                "the console relay exited before creating its socket; last lines of {}: {}",
                paths.relay_log.display(),
                log_tail(&paths.relay_log)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    terminate_relay(pid, &binary);
    bail!(
        "timed out waiting for the console relay socket {}; last lines of {}: {}",
        paths.console_socket.display(),
        paths.relay_log.display(),
        log_tail(&paths.relay_log)
    )
}

pub(super) async fn wait_for_vm(socket: &Path, child: &mut Child, timeout: Duration) -> Result<()> {
    let client = CloudHypervisorClient::new(socket);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socket.exists()
            && let Ok(info) = client.info().await
            && info.state.is_alive()
        {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!("Cloud Hypervisor exited during startup ({status}); check the log");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!(
        "timed out after {timeout:?} waiting for a running Cloud Hypervisor VM at {}",
        socket.display()
    )
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::vm::instance::test_support::{config_with_assets, instance_for, strings};

    #[test]
    fn virtiofsd_honours_the_configured_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.virtiofsd_bin = std::env::current_exe().unwrap();
        config.virtiofs_sandbox = crate::config::VirtiofsSandbox::Chroot;
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let virtiofs = virtiofs_config(&config, &paths, dir.path()).unwrap();
        assert_eq!(virtiofs.sandbox, crate::config::VirtiofsSandbox::Chroot);
    }

    #[test]
    fn cloud_hypervisor_args_are_hardened_and_use_native_virtiofs() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let args = strings(&cloud_hypervisor_args(
            &config,
            &instance,
            &paths,
            &instance.rootfs_path(&config),
            None,
            None,
        ));
        let joined = args.join(" ");
        assert!(joined.contains("--kernel"), "{joined}");
        assert!(joined.contains("--disk"), "{joined}");
        assert!(joined.contains("image_type=qcow2"), "{joined}");
        assert!(joined.contains("--memory size=128M,shared=on"), "{joined}");
        assert!(joined.contains("--fs tag=workspace"), "{joined}");
        assert!(joined.contains("--serial socket="), "{joined}");
        assert!(joined.contains("--console off"), "{joined}");
        assert!(joined.contains("--seccomp true"), "{joined}");
        assert!(joined.contains("--landlock"), "{joined}");
        assert!(joined.contains("--landlock-rules"), "{joined}");
        assert!(
            joined.contains(&format!(
                "path={},access=rw",
                SnapshotStore::new(&config.snapshots_dir)
                    .staging_dir()
                    .display()
            )),
            "{joined}"
        );
        assert!(!joined.contains("--net"), "{joined}");
        assert!(
            joined.contains(&format!("root={}", config.root_device)),
            "{joined}"
        );
    }

    #[test]
    fn guarded_network_adds_only_a_tap_and_mac() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let lease = crate::network::Lease {
            index: 2,
            instance: instance.name.clone(),
            tap: crate::network::tap_name(2),
            mac: crate::network::mac(2),
            host_ip: crate::network::host_ip(2),
            guest_ip: crate::network::guest_ip(2),
        };
        let joined = strings(&cloud_hypervisor_args(
            &config,
            &instance,
            &paths,
            &instance.rootfs_path(&config),
            Some(&lease),
            None,
        ))
        .join(" ");
        assert!(
            joined.contains("--net tap=hegn2,mac=52:54:00:00:00:03"),
            "{joined}"
        );
        // Cloud Hypervisor's own Landlock ruleset only grants /dev/net/tun;
        // reading an existing TAP's sysfs flags needs only its own directory.
        assert!(
            joined.contains("--landlock-rules path=/sys/devices/virtual/net/hegn2,access=r"),
            "{joined}"
        );
        assert!(
            !joined.contains("--landlock-rules path=/sys/devices/virtual/net,access=r"),
            "{joined}"
        );
        // The host TAP address is owned by `hegn network setup`, not the VMM.
        assert!(!joined.contains("ip=10.213.0."), "{joined}");
        assert!(!joined.contains("mask="), "{joined}");
    }

    #[test]
    fn profile_share_is_a_second_sandboxed_device() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let socket = dir.path().join("profile.sock");
        let args = strings(&cloud_hypervisor_args(
            &config,
            &instance,
            &paths,
            &instance.rootfs_path(&config),
            None,
            Some(&socket),
        ));
        let joined = args.join(" ");
        assert!(joined.contains("--fs tag=workspace"), "{joined}");
        assert!(
            joined.contains(&format!(
                "--fs tag=hegn-profile,socket={}",
                socket.display()
            )),
            "{joined}"
        );
    }

    #[test]
    fn sysfs_tap_rule_is_absent_without_a_network_device() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let joined = strings(&cloud_hypervisor_args(
            &config,
            &instance,
            &paths,
            &instance.rootfs_path(&config),
            None,
            None,
        ))
        .join(" ");
        assert!(!joined.contains("/sys/devices/virtual/net"), "{joined}");
    }

    #[test]
    fn sysfs_tap_rule_is_skipped_when_landlock_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.landlock = false;
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let lease = crate::network::Lease {
            index: 2,
            instance: instance.name.clone(),
            tap: crate::network::tap_name(2),
            mac: crate::network::mac(2),
            host_ip: crate::network::host_ip(2),
            guest_ip: crate::network::guest_ip(2),
        };
        let joined = strings(&cloud_hypervisor_args(
            &config,
            &instance,
            &paths,
            &instance.rootfs_path(&config),
            Some(&lease),
            None,
        ))
        .join(" ");
        assert!(!joined.contains("--landlock"), "{joined}");
        assert!(!joined.contains("/sys/devices/virtual/net"), "{joined}");
    }

    #[test]
    fn air_gapped_boot_has_no_network_device() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let joined = strings(&cloud_hypervisor_args(
            &config,
            &instance,
            &paths,
            &instance.rootfs_path(&config),
            None,
            None,
        ))
        .join(" ");
        assert!(!joined.contains("--net"), "{joined}");
    }

    #[test]
    fn seccomp_and_landlock_can_be_disabled_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.seccomp = false;
        config.landlock = false;
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let joined = strings(&cloud_hypervisor_args(
            &config,
            &instance,
            &paths,
            &instance.rootfs_path(&config),
            None,
            None,
        ))
        .join(" ");
        assert!(joined.contains("--seccomp false"), "{joined}");
        assert!(!joined.contains("--landlock"), "{joined}");
    }

    #[test]
    fn restore_args_come_from_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let snap_dir = dir.path().join("snap");
        let snapshot = SnapshotMeta {
            name: "demo".into(),
            created_at: chrono::Utc::now(),
            vm_state: "Paused".into(),
            dir: snap_dir.clone(),
            instance: Some("demo".into()),
            project: None,
            size_bytes: 0,
            note: None,
        };
        let joined = strings(&restore_args(&config, &paths, &snapshot)).join(" ");
        assert!(
            joined.contains(&format!(
                "source_url=file://{},resume=true",
                snap_dir.display()
            )),
            "{joined}"
        );
        assert!(!joined.contains("--kernel"), "{joined}");
        assert!(!joined.contains("--fs"), "{joined}");
        // Landlock is part of the saved VM config on restore, not a CLI flag.
        assert!(!joined.contains("--landlock"), "{joined}");
    }

    #[test]
    fn boot_args_name_the_architecture_console() {
        let args = boot_args("/dev/vda1");
        assert!(args.contains(SERIAL_CONSOLE), "{args}");
        assert!(args.contains("root=/dev/vda1 rw"), "{args}");
    }

    #[test]
    fn cloud_init_seed_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_init = false;
        let instance = instance_for(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        let args = strings(&cloud_hypervisor_args(
            &config,
            &instance,
            &paths,
            &instance.rootfs_path(&config),
            None,
            None,
        ));
        assert!(
            !args.iter().any(|arg| arg.contains("cloud-init.img")),
            "{args:?}"
        );
    }
}
