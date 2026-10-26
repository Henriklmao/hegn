//! Cloud Hypervisor lifecycle orchestration.
//!
//! One hegn state directory manages one VM at a time:
//!
//! ```text
//! <state>/run/
//!   cloud-hypervisor.sock   REST API socket
//!   serial.sock             guest serial socket (Cloud Hypervisor listener)
//!   console.sock            relay socket used by `hegn attach`
//!   virtiofs.sock           virtiofsd vhost-user socket
//!   cloud-hypervisor.pid
//!   console-relay.pid
//!   virtiofs.pid
//!   session.json
//!   cloud-hypervisor.log    VMM log
//!   hegn.log                lifecycle/start diagnostics
//!   hegn.lock               lifecycle flock
//!   cloud-init.img          NoCloud seed for the official cloud images
//! ```
//!
//! `start` boots virtiofsd, Cloud Hypervisor and the detached serial relay. The
//! child processes deliberately outlive the CLI process; `stop` terminates
//! them. VM lifecycle operations hold an exclusive lock on `hegn.lock` so two
//! concurrent `hegn open` calls cannot boot a VMM on the same sockets.
//!
//! Cloud Hypervisor supports virtio-fs natively, so no special fork or
//! patched build is required. The VMM runs with seccomp filters and, when
//! enabled, Landlock restricting its filesystem view to the kernel, disks and
//! serial/fs sockets.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cloud_hypervisor::{CloudHypervisorClient, SnapshotConfig, VmInfo, VmState};
use crate::cloud_init;
use crate::config::{Config, Harness, VIRTIOFSD_FALLBACKS};
use crate::logs::{DEFAULT_KEEP, LogStore};
use crate::network;
use crate::provision::ProvisionPlan;
use crate::relay::{self, RelayArgs};
use crate::sandbox_config::{CloneOutcome, DeleteOutcome, SandboxConfig, SandboxStore};
use crate::snapshot::{SnapshotMeta, SnapshotStore};
use crate::virtiofs::{VirtiofsConfig, VirtiofsProcess};

const API_START_TIMEOUT: Duration = Duration::from_secs(15);
const RELAY_START_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_arch = "x86_64")]
const SERIAL_CONSOLE: &str = "ttyS0";
#[cfg(target_arch = "aarch64")]
const SERIAL_CONSOLE: &str = "ttyAMA0";

/// TAP names embedded in a Cloud Hypervisor snapshot config.
fn snapshot_network_taps(dir: &Path) -> Result<Vec<String>> {
    let path = dir.join("config.json");
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(value
        .get("net")
        .and_then(serde_json::Value::as_array)
        .map(|devices| {
            devices
                .iter()
                .filter_map(|device| device.get("tap").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

fn boot_args(root_device: &str) -> String {
    format!("console={SERIAL_CONSOLE} reboot=k panic=1 root={root_device} rw")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPaths {
    pub run_dir: PathBuf,
    /// Cloud Hypervisor REST API.
    pub api_socket: PathBuf,
    /// Guest serial socket created by Cloud Hypervisor.
    pub serial_socket: PathBuf,
    /// Relay socket consumed by the TUI and `hegn attach`.
    pub console_socket: PathBuf,
    pub virtiofs_socket: PathBuf,
    pub vm_pid: PathBuf,
    pub relay_pid: PathBuf,
    pub virtiofs_pid: PathBuf,
    pub session: PathBuf,
    pub log: PathBuf,
    /// hegn lifecycle/start errors, kept in the run directory for post-mortems.
    pub event_log: PathBuf,
    /// Exclusive lock serializing VM lifecycle operations.
    pub lock: PathBuf,
    /// Generated NoCloud seed disk.
    pub cloud_init: PathBuf,
}

impl SandboxPaths {
    pub fn from_sandbox(config: &Config, sandbox: &SandboxConfig) -> Self {
        let run_dir = sandbox.dir(&config.sandboxes_dir).join("run");
        Self {
            api_socket: run_dir.join("cloud-hypervisor.sock"),
            serial_socket: run_dir.join("serial.sock"),
            console_socket: run_dir.join("console.sock"),
            virtiofs_socket: run_dir.join("virtiofs.sock"),
            vm_pid: run_dir.join("cloud-hypervisor.pid"),
            relay_pid: run_dir.join("console-relay.pid"),
            virtiofs_pid: run_dir.join("virtiofs.pid"),
            session: run_dir.join("session.json"),
            log: run_dir.join("cloud-hypervisor.log"),
            event_log: run_dir.join("hegn.log"),
            lock: run_dir.join("hegn.lock"),
            cloud_init: run_dir.join("cloud-init.img"),
            run_dir,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        // The API, serial, relay and virtiofs sockets live here; a private
        // directory keeps other local users from driving the microVM.
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            // A symlinked run directory could redirect the VMM/console sockets
            // outside the private state tree. Refuse instead of following it.
            if let Ok(metadata) = fs::symlink_metadata(&self.run_dir)
                && metadata.file_type().is_symlink()
            {
                bail!(
                    "refusing symlinked run directory {}",
                    self.run_dir.display()
                );
            }
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(&self.run_dir)
                .with_context(|| format!("failed to create {}", self.run_dir.display()))?;
            // The directory may already exist with wider permissions. Sockets
            // for the VMM and the console relay live here, so tightening it is
            // mandatory, not best effort.
            fs::set_permissions(&self.run_dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", self.run_dir.display()))?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(&self.run_dir)
            .with_context(|| format!("failed to create {}", self.run_dir.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub project: PathBuf,
    /// Workspace exported to the guest. Empty in records from older versions.
    #[serde(default)]
    pub workspace: PathBuf,
    #[serde(default)]
    pub sandbox: String,
    pub harness: Harness,
    pub started_at: DateTime<Utc>,
    pub vm_pid: u32,
    #[serde(default)]
    pub vm_binary: Option<PathBuf>,
    #[serde(default)]
    pub relay_pid: u32,
    pub virtiofs_pid: u32,
    pub boot_args: String,
    /// Persistent serial log for this session. Optional so session records
    /// written by older hegn versions still deserialize.
    #[serde(default)]
    pub serial_log: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SandboxReport {
    pub state: String,
    pub backend: String,
    pub sandbox: String,
    pub project: Option<PathBuf>,
    pub workspace: PathBuf,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub api_socket: PathBuf,
    pub serial_socket: PathBuf,
    pub console_socket: PathBuf,
    pub virtiofs_socket: PathBuf,
    pub virtiofs_running: bool,
    pub relay_running: bool,
    pub vm_pid: Option<u32>,
    pub relay_pid: Option<u32>,
    pub virtiofs_pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    /// Persistent serial console log, if the session predates logging or the
    /// log has been removed.
    pub serial_log: Option<PathBuf>,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SandboxManager {
    config: Config,
    sandbox: SandboxConfig,
}

/// Exclusive lock held for the duration of a VM lifecycle operation.
///
/// Dropping it (including on an error path) releases the lock.
#[derive(Debug)]
struct RunLock {
    _file: File,
}

impl SandboxManager {
    pub fn new(config: Config, sandbox: SandboxConfig) -> Self {
        Self { config, sandbox }
    }

    pub fn sandbox(&self) -> &SandboxConfig {
        &self.sandbox
    }

    /// Persist a newly assigned guarded-network slice so later boots and
    /// snapshot restores use the same TAP device.
    fn persist_network_index(&self, index: u8) -> Result<()> {
        if self.sandbox.network_index == Some(index) {
            return Ok(());
        }
        let mut sandbox = self.sandbox.clone();
        sandbox.network_index = Some(index);
        SandboxStore::new(self.config.clone()).save(&sandbox)
    }

    pub fn paths(&self) -> SandboxPaths {
        SandboxPaths::from_sandbox(&self.config, &self.sandbox)
    }

    pub fn session(&self) -> Option<SessionRecord> {
        let raw = fs::read_to_string(self.paths().session).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// A session file that exists but cannot be parsed is a migration hazard:
    /// an old VMM may still be running, but hegn no longer knows its pid.
    /// Refuse to overwrite that state instead of leaking an unmanaged VM.
    fn unreadable_session(&self) -> bool {
        let path = self.paths().session;
        path.exists() && self.session().is_none()
    }

    pub fn is_running(&self) -> bool {
        self.session()
            .map(|session| {
                let binary = session
                    .vm_binary
                    .clone()
                    .unwrap_or_else(|| self.config.vm_binary());
                process_matches(session.vm_pid, &binary)
            })
            .unwrap_or(false)
    }

    /// Take the exclusive VM lifecycle lock.
    ///
    /// A second `hegn open`/`sandbox start` must not recreate the API or serial
    /// socket underneath a VM that is already booting.
    fn lock(&self, paths: &SandboxPaths) -> Result<RunLock> {
        paths.ensure()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&paths.lock)
            .with_context(|| format!("failed to open {}", paths.lock.display()))?;
        // Safety: `file` is a valid, open descriptor for the lock file.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                bail!("another hegn VM operation is in progress; retry when it finishes");
            }
            return Err(err).with_context(|| format!("failed to lock {}", paths.lock.display()));
        }
        Ok(RunLock { _file: file })
    }

    /// Boot virtiofsd, Cloud Hypervisor and the console relay.
    ///
    /// Lifecycle errors are appended to `run/hegn.log` so a failed start can
    /// be diagnosed even when the guest never reached userspace.
    pub fn start(&self) -> Result<SandboxReport> {
        let result = self.start_inner();
        let paths = self.paths();
        // `start_inner` may fail before it creates the run directory.
        let _ = paths.ensure();
        let message = match &result {
            Ok(report) => format!(
                "started sandbox={} project={} harness={} vm_pid={}",
                report.sandbox,
                report.workspace.display(),
                self.sandbox.harness.label(),
                report.vm_pid.unwrap_or(0)
            ),
            Err(err) => format!(
                "start failed sandbox={} project={}: {err:#}",
                self.sandbox.name,
                self.sandbox.workspace.display()
            ),
        };
        append_event_log(&paths.event_log, &message);
        result.map_err(|err| anyhow::anyhow!("{err:#} (VM log: {})", paths.event_log.display()))
    }

    fn start_inner(&self) -> Result<SandboxReport> {
        let paths = self.paths();
        // A missing VM is always a hard error: there is no host-process fallback.
        self.config.require_vm_assets()?;
        // The snapshot staging directory must exist before Cloud Hypervisor
        // starts: it is a Landlock rule, and Landlock rules are validated at
        // create time.
        self.config.ensure_dirs()?;
        SnapshotStore::new(&self.config.snapshots_dir).ensure_staging()?;
        let project = &self.sandbox.workspace;
        if !project.is_dir() {
            bail!("sandbox workspace {} does not exist", project.display());
        }
        // Preflight every binary before spawning anything, so a typo in the
        // configuration cannot leave a stray virtiofsd (or a VM without a
        // console) behind.
        let vm_binary = self.config.resolved_cloud_hypervisor().with_context(|| {
            format!(
                "cloud-hypervisor binary {} was not found; hegn also accepts `{}`; run `hegn doctor`",
                self.config.cloud_hypervisor_bin.display(),
                crate::config::CLOUD_HYPERVISOR_STATIC
            )
        })?;
        if self.config.resolved_virtiofsd().is_none() {
            bail!(
                "virtiofsd binary {} was not found on PATH or in {}; install virtiofsd or set \
                 `virtiofsd_bin` (run `hegn doctor`)",
                self.config.virtiofsd_bin.display(),
                VIRTIOFSD_FALLBACKS.join(", ")
            );
        }
        let hegn = hegn_binary()?;
        let _lock = self.lock(&paths)?;
        if self.unreadable_session() {
            bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 stop the old VM manually and remove that directory before starting",
                paths.run_dir.display()
            );
        }
        if self.is_running() {
            bail!("a sandbox is already running (use `hegn sandbox stop` first)");
        }
        if self.session().is_some() {
            self.cleanup_stale();
        }
        if self.config.network == crate::config::NetworkMode::Off
            && self.config.cloud_init
            && self.sandbox.harness != Harness::Shell
        {
            bail!(
                "harness '{}' needs the guarded network for its first-boot install; enable \
                 `network = \"guarded\"` or use a pre-provisioned rootfs with `cloud_init = false`",
                self.sandbox.harness.label()
            );
        }
        // Reserve the guarded-network slice before any process is spawned.
        // `acquire` also verifies that the host firewall marker is present, so
        // an unconfigured guard fails before virtiofsd or the VMM can start.
        let network_lease = match self.config.network {
            crate::config::NetworkMode::Guarded => {
                Some(network::acquire(&self.config, &self.sandbox)?)
            }
            crate::config::NetworkMode::Off => None,
        };
        if let Some(lease) = &network_lease
            && let Err(err) = self.persist_network_index(lease.index)
        {
            network::release(&self.config, &self.sandbox.name);
            return Err(err);
        }
        if let Err(err) = paths.ensure() {
            network::release(&self.config, &self.sandbox.name);
            return Err(err);
        }
        // Build the runtime before spawning anything so a failure here cannot
        // leak a running virtiofsd or VMM process.
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                network::release(&self.config, &self.sandbox.name);
                return Err(err).context("failed to build async runtime");
            }
        };

        let started_at = Utc::now();
        let logs = LogStore::new(self.config.logs_dir());
        if let Err(err) = logs.prune(DEFAULT_KEEP) {
            append_event_log(
                &paths.event_log,
                &format!("warning: could not prune old serial logs: {err:#}"),
            );
        }
        let serial_log = match logs.create_session(project, started_at, std::process::id()) {
            Ok(path) => path,
            Err(err) => {
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };

        // 1. virtiofsd, sandboxed to exactly this project.
        let mut virtiofs = match spawn_virtiofsd(&self.config, &paths, project) {
            Ok(process) => process,
            Err(err) => {
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };
        let virtiofs_pid = virtiofs.pid();
        if let Err(err) = write_private(&paths.virtiofs_pid, virtiofs_pid.to_string()) {
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.sandbox.name);
            return Err(err).context("failed to record the virtiofsd pid");
        }

        // 2. Optional NoCloud seed for the official cloud images. It contains
        // no secrets and only enables a serial root login.
        let provision = match ProvisionPlan::new(self.sandbox.harness, &self.sandbox.extra_packages)
        {
            Ok(plan) => plan,
            Err(err) => {
                virtiofs.kill();
                cleanup_files(&paths);
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };
        if self.config.cloud_init
            && let Err(err) = cloud_init::write_seed(
                &paths.cloud_init,
                &self.sandbox.shared_tag,
                &self.sandbox.guest_mount,
                &cloud_init::instance_id(&self.sandbox.name, &provision, network_lease.as_ref()),
                &provision,
                network_lease.as_ref(),
            )
        {
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.sandbox.name);
            return Err(err);
        }

        // 3. Cloud Hypervisor. Children outlive this CLI call.
        let mut vm = match spawn_cloud_hypervisor(
            &self.config,
            &self.sandbox,
            &paths,
            &vm_binary,
            None,
            network_lease.as_ref(),
        ) {
            Ok(child) => child,
            Err(err) => {
                virtiofs.kill();
                cleanup_files(&paths);
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };
        let vm_pid = vm.id();

        // 4. The relay is Cloud Hypervisor's only serial client. Start it
        // before waiting for the API so the earliest boot output is captured.
        let relay_pid = match start_relay(&paths, &serial_log) {
            Ok(pid) => pid,
            Err(err) => {
                let _ = vm.kill();
                let _ = vm.wait();
                virtiofs.kill();
                cleanup_files(&paths);
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };

        if let Err(err) =
            runtime.block_on(wait_for_vm(&paths.api_socket, &mut vm, API_START_TIMEOUT))
        {
            let _ = vm.kill();
            let _ = vm.wait();
            terminate_relay(relay_pid, &hegn);
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.sandbox.name);
            return Err(err).context("failed to boot Cloud Hypervisor");
        }

        let record = SessionRecord {
            project: self.sandbox.project.clone(),
            workspace: self.sandbox.workspace.clone(),
            sandbox: self.sandbox.name.clone(),
            harness: self.sandbox.harness,
            started_at,
            vm_pid,
            vm_binary: Some(vm_binary.clone()),
            relay_pid,
            virtiofs_pid,
            boot_args: boot_args(&self.config.root_device),
            serial_log: Some(serial_log.clone()),
        };
        let recorded = (|| -> Result<()> {
            write_private(&paths.vm_pid, vm_pid.to_string())?;
            write_private(&paths.relay_pid, relay_pid.to_string())?;
            write_private(&paths.session, serde_json::to_string_pretty(&record)?)?;
            Ok(())
        })();
        if let Err(err) = recorded {
            // Without the session record a later `stop` cannot find the VM, so
            // tear everything down instead of leaking a running VMM.
            let _ = vm.kill();
            let _ = vm.wait();
            terminate_relay(relay_pid, &hegn);
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.sandbox.name);
            return Err(err).context("failed to record the sandbox session");
        }

        // Do not kill the children on drop.
        std::mem::forget(vm);
        std::mem::forget(virtiofs);

        Ok(SandboxReport {
            state: "running".into(),
            backend: "cloud-hypervisor".into(),
            sandbox: self.sandbox.name.clone(),
            project: Some(self.sandbox.project.clone()),
            workspace: self.sandbox.workspace.clone(),
            vcpus: self.sandbox.vcpus,
            memory_mib: self.sandbox.memory_mib,
            api_socket: paths.api_socket,
            serial_socket: paths.serial_socket,
            console_socket: paths.console_socket,
            virtiofs_socket: paths.virtiofs_socket,
            virtiofs_running: true,
            relay_running: true,
            vm_pid: Some(vm_pid),
            relay_pid: Some(relay_pid),
            virtiofs_pid: Some(virtiofs_pid),
            started_at: Some(record.started_at),
            serial_log: Some(serial_log),
            note: Some(format!(
                "guest workspace mounted at {}",
                self.sandbox.guest_mount
            )),
        })
    }

    /// Attach the TUI/CLI terminal to the running guest serial console.
    pub fn attach_console(&self) -> Result<crate::console::VmConsole> {
        crate::console::VmConsole::attach(&self.config, &self.sandbox)
    }

    /// Boot a VM if needed, then attach its console.
    pub fn ensure_console(&self) -> Result<crate::console::VmConsole> {
        if !self.is_running() {
            self.start()?;
        }
        self.attach_console()
    }

    /// Stop Cloud Hypervisor, the console relay and virtiofsd.
    pub fn stop(&self) -> Result<()> {
        let paths = self.paths();
        if !paths.run_dir.is_dir() {
            bail!("no sandbox session found in {}", paths.run_dir.display());
        }
        let _lock = self.lock(&paths)?;
        let hegn = hegn_binary()?;
        if self.unreadable_session() {
            bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 find and stop the recorded VMM manually, then remove that directory",
                paths.run_dir.display()
            );
        }
        let session = self.session();
        let vm_pid = session
            .as_ref()
            .map(|s| s.vm_pid)
            .or_else(|| read_pid(&paths.vm_pid));
        let relay_pid = session
            .as_ref()
            .map(|s| s.relay_pid)
            .filter(|pid| *pid != 0)
            .or_else(|| read_pid(&paths.relay_pid));
        let virtiofs_pid = session
            .as_ref()
            .map(|s| s.virtiofs_pid)
            .or_else(|| read_pid(&paths.virtiofs_pid));

        if vm_pid.is_none() && relay_pid.is_none() && virtiofs_pid.is_none() {
            bail!("no sandbox session found in {}", paths.run_dir.display());
        }
        let vm_binary = session
            .as_ref()
            .and_then(|session| session.vm_binary.clone())
            .unwrap_or_else(|| self.config.vm_binary());

        if let Some(pid) = vm_pid {
            terminate(pid, &vm_binary);
        }
        if let Some(pid) = relay_pid {
            terminate_relay(pid, &hegn);
        }
        if let Some(pid) = virtiofs_pid {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        cleanup_files(&paths);
        network::release(&self.config, &self.sandbox.name);
        Ok(())
    }

    /// Pause the guest (used before snapshot creation).
    pub async fn pause(&self) -> Result<()> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        CloudHypervisorClient::new(&paths.api_socket).pause().await
    }

    pub async fn resume(&self) -> Result<()> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        CloudHypervisorClient::new(&paths.api_socket).resume().await
    }

    pub async fn instance_info(&self) -> Result<VmInfo> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        CloudHypervisorClient::new(&paths.api_socket).info().await
    }

    pub fn status(&self) -> Result<SandboxReport> {
        let paths = self.paths();
        let session = self.session();
        let virtiofs_running = session
            .as_ref()
            .map(|s| process_matches(s.virtiofs_pid, &self.config.virtiofsd_bin))
            .unwrap_or(false);
        let hegn = hegn_binary().ok();
        let relay_running = session
            .as_ref()
            .filter(|s| s.relay_pid != 0)
            .map(|s| {
                hegn.as_deref()
                    .is_some_and(|binary| relay::process_matches_relay(s.relay_pid, binary))
            })
            .unwrap_or(false);
        let api_socket_exists = paths.api_socket.exists();
        let mut state = if api_socket_exists {
            "unknown"
        } else {
            "stopped"
        }
        .to_string();
        let mut note = None;

        if api_socket_exists {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            match runtime.block_on(CloudHypervisorClient::new(&paths.api_socket).info()) {
                Ok(info) => {
                    state = match info.state {
                        VmState::Running => "running",
                        VmState::Paused | VmState::BreakPoint => "paused",
                        VmState::Created => "created",
                        VmState::Shutdown => "stopped",
                    }
                    .to_string();
                }
                Err(err) => {
                    state = "failed".into();
                    note = Some(err.to_string());
                }
            }
        } else if session.is_some() {
            state = "stopped".into();
            note = Some("session metadata exists but the API socket is gone".into());
        }
        if self.unreadable_session() {
            state = "failed".into();
            note = Some(
                "session metadata is unreadable or from an older hegn version; stop the old VM \
                 and remove the run directory"
                    .into(),
            );
        }

        Ok(SandboxReport {
            state,
            backend: "cloud-hypervisor".into(),
            sandbox: self.sandbox.name.clone(),
            project: session
                .as_ref()
                .map(|s| s.project.clone())
                .or_else(|| Some(self.sandbox.project.clone())),
            workspace: self.sandbox.workspace.clone(),
            vcpus: self.sandbox.vcpus,
            memory_mib: self.sandbox.memory_mib,
            api_socket: paths.api_socket,
            serial_socket: paths.serial_socket,
            console_socket: paths.console_socket,
            virtiofs_socket: paths.virtiofs_socket,
            virtiofs_running,
            relay_running,
            vm_pid: session.as_ref().map(|s| s.vm_pid),
            relay_pid: session.as_ref().map(|s| s.relay_pid),
            virtiofs_pid: session.as_ref().map(|s| s.virtiofs_pid),
            started_at: session.as_ref().map(|s| s.started_at),
            serial_log: session.as_ref().and_then(|s| s.serial_log.clone()),
            note,
        })
    }

    /// Pause, snapshot RAM+CPU into `snapshots_dir`, resume.
    pub async fn create_snapshot(&self, name: &str) -> Result<SnapshotMeta> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        // Serialize with start/stop/restore so the VM cannot be torn down
        // between pause and resume.
        let _lock = self.lock(&paths)?;
        let name = crate::snapshot::sanitize_name(name)?;
        let store = SnapshotStore::new(&self.config.snapshots_dir);
        let prepared = store.prepare_staged(&name)?;
        let client = CloudHypervisorClient::new(&paths.api_socket);

        if let Err(err) = client.pause().await {
            let _ = fs::remove_dir_all(&prepared.dir);
            return Err(err);
        }
        if let Err(err) = client
            .snapshot(&SnapshotConfig {
                destination_url: format!("file://{}", prepared.dir.display()),
            })
            .await
        {
            // Always try to resume, even if snapshot creation failed.
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&prepared.dir);
            return Err(err);
        }

        if let Err(err) = secure_snapshot_dir(&prepared.dir) {
            // Snapshot memory can contain credentials; do not keep a snapshot
            // whose permissions we could not tighten.
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&prepared.dir);
            return Err(err);
        }
        // Move the finished snapshot out of the VMM's Landlock view before it
        // becomes a durable checkpoint.
        if let Err(err) = store.finalize(&prepared) {
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&prepared.dir);
            return Err(err);
        }
        let size = dir_size(&prepared.final_dir);
        let meta = SnapshotMeta {
            name: name.to_string(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: prepared.final_dir.clone(),
            sandbox: Some(self.sandbox.name.clone()),
            project: Some(self.sandbox.project.clone()),
            size_bytes: size,
            note: None,
        };
        if let Err(err) = store.save_meta(&meta) {
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&prepared.final_dir);
            return Err(err);
        }
        client
            .resume()
            .await
            .context("snapshot saved, but the VM could not be resumed")?;
        Ok(meta)
    }

    /// Restore a snapshot into a brand new Cloud Hypervisor process.
    ///
    /// The running VMM and relay are replaced first. A fresh process keeps a
    /// failed restore (Cloud Hypervisor exits on some restore errors) from
    /// taking down an existing VM. virtiofsd is restarted for the snapshot's
    /// project so the restored guest finds a healthy backend on the same
    /// socket path.
    pub async fn restore_snapshot(&self, snapshot: &SnapshotMeta) -> Result<()> {
        // The snapshot config embeds the disk paths from the sandbox that
        // created it. Restoring it elsewhere would make two sandboxes share one
        // writable rootfs, so only the owning sandbox may load it.
        if let Some(owner) = &snapshot.sandbox
            && owner != &self.sandbox.name
        {
            bail!(
                "snapshot '{}' belongs to sandbox '{}'; restore it into that sandbox",
                snapshot.name,
                owner
            );
        }
        // The name alone is not enough: a deleted sandbox can be recreated for
        // a different project, and its old snapshot must not be loaded there.
        if let Some(project) = &snapshot.project
            && !same_project(project, &self.sandbox.project)
        {
            bail!(
                "snapshot '{}' belongs to {}; restore it into a sandbox for that project",
                snapshot.name,
                project.display()
            );
        }
        let paths = self.paths();
        paths.ensure()?;
        self.config.ensure_dirs()?;
        SnapshotStore::new(&self.config.snapshots_dir).ensure_staging()?;
        if !snapshot.dir.is_dir() {
            bail!("snapshot directory {} is missing", snapshot.dir.display());
        }
        for required in ["config.json", "state.json", "memory-ranges"] {
            if !snapshot.dir.join(required).is_file() {
                bail!(
                    "snapshot {} is not restorable: missing {}",
                    snapshot.name,
                    snapshot.dir.join(required).display()
                );
            }
        }
        let existing = self.session();
        let project = self.sandbox.workspace.clone();
        if !project.is_dir() {
            bail!("sandbox workspace {} no longer exists", project.display());
        }
        // Preflight the binaries before stopping a running VM, so a typo in
        // the configuration does not cost the user their current sandbox.
        let vm_binary = self.config.resolved_cloud_hypervisor().with_context(|| {
            format!(
                "cloud-hypervisor binary {} was not found; hegn also accepts `{}`",
                self.config.cloud_hypervisor_bin.display(),
                crate::config::CLOUD_HYPERVISOR_STATIC
            )
        })?;
        if self.config.resolved_virtiofsd().is_none() {
            bail!(
                "virtiofsd binary {} was not found on PATH or in {}; install virtiofsd or set \
                 `virtiofsd_bin`",
                self.config.virtiofsd_bin.display(),
                VIRTIOFSD_FALLBACKS.join(", ")
            );
        }
        let hegn = hegn_binary()?;
        // Serialize with start/stop so another hegn process cannot recreate the
        // sockets while the snapshot boot is in flight.
        let _lock = self.lock(&paths)?;
        if self.unreadable_session() {
            bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 stop the old VM manually and remove that directory before restoring",
                paths.run_dir.display()
            );
        }
        if self.config.network == crate::config::NetworkMode::Off
            && self.config.cloud_init
            && self.sandbox.harness != Harness::Shell
        {
            bail!(
                "harness '{}' needs the guarded network for its first-boot install; enable \
                 `network = \"guarded\"` or use a pre-provisioned rootfs with `cloud_init = false`",
                self.sandbox.harness.label()
            );
        }
        let network_lease = match self.config.network {
            crate::config::NetworkMode::Guarded => {
                Some(network::acquire(&self.config, &self.sandbox)?)
            }
            crate::config::NetworkMode::Off => None,
        };
        if let Some(lease) = &network_lease
            && let Err(err) = self.persist_network_index(lease.index)
        {
            network::release(&self.config, &self.sandbox.name);
            return Err(err);
        }
        // The snapshot embeds its device tree. Never silently restore a NIC
        // that contradicts the current policy, and never attach a snapshot to
        // a different sandbox TAP than the one it was created with.
        let snapshot_taps = match snapshot_network_taps(&snapshot.dir) {
            Ok(taps) => taps,
            Err(err) => {
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };
        match (&network_lease, snapshot_taps.as_slice()) {
            (None, []) => {}
            (None, [_, ..]) => {
                network::release(&self.config, &self.sandbox.name);
                bail!(
                    "snapshot '{}' contains a network device but `network = \"off\"`; enable \
                     guarded networking or restore an air-gapped snapshot",
                    snapshot.name
                );
            }
            (Some(lease), []) => append_event_log(
                &paths.event_log,
                &format!(
                    "warning: snapshot {} has no network device; restoring it without the reserved {}",
                    snapshot.name, lease.tap
                ),
            ),
            (Some(lease), taps) if taps.iter().any(|tap| tap != &lease.tap) => {
                network::release(&self.config, &self.sandbox.name);
                bail!(
                    "snapshot '{}' expects TAP {}, but sandbox '{}' owns {}; restore it in the \
                     sandbox that created it",
                    snapshot.name,
                    taps.join(", "),
                    self.sandbox.name,
                    lease.tap
                );
            }
            (Some(_), [_, ..]) => {}
        }

        // Stop the old VM, relay and filesystem daemon. The sockets are freed
        // and recreated below with the same paths the snapshot expects.
        let existing_vm_binary = existing
            .as_ref()
            .and_then(|session| session.vm_binary.clone())
            .unwrap_or_else(|| self.config.vm_binary());
        if let Some(session) = &existing {
            terminate(session.vm_pid, &existing_vm_binary);
            terminate_relay(session.relay_pid, &hegn);
            terminate(session.virtiofs_pid, &self.config.virtiofsd_bin);
        }
        if let Some(pid) = read_pid(&paths.vm_pid) {
            terminate(pid, &existing_vm_binary);
        }
        if let Some(pid) = read_pid(&paths.relay_pid) {
            terminate_relay(pid, &hegn);
        }
        if let Some(pid) = read_pid(&paths.virtiofs_pid) {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        cleanup_files(&paths);

        let mut virtiofs = match spawn_virtiofsd(&self.config, &paths, &project) {
            Ok(process) => process,
            Err(err) => {
                cleanup_files(&paths);
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };
        let virtiofs_pid = virtiofs.pid();
        if let Err(err) = write_private(&paths.virtiofs_pid, virtiofs_pid.to_string()) {
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.sandbox.name);
            return Err(err).context("failed to record the virtiofsd pid");
        }

        let provision = match ProvisionPlan::new(self.sandbox.harness, &self.sandbox.extra_packages)
        {
            Ok(plan) => plan,
            Err(err) => {
                virtiofs.kill();
                cleanup_files(&paths);
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };
        if self.config.cloud_init
            && let Err(err) = cloud_init::write_seed(
                &paths.cloud_init,
                &self.sandbox.shared_tag,
                &self.sandbox.guest_mount,
                &cloud_init::instance_id(&self.sandbox.name, &provision, network_lease.as_ref()),
                &provision,
                network_lease.as_ref(),
            )
        {
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.sandbox.name);
            return Err(err);
        }

        let started_at = Utc::now();
        let logs = LogStore::new(self.config.logs_dir());
        if let Err(err) = logs.prune(DEFAULT_KEEP) {
            append_event_log(
                &paths.event_log,
                &format!("warning: could not prune old serial logs: {err:#}"),
            );
        }
        let serial_log = match logs.create_session(&project, started_at, std::process::id()) {
            Ok(path) => path,
            Err(err) => {
                virtiofs.kill();
                cleanup_files(&paths);
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };

        let mut vm = match spawn_cloud_hypervisor(
            &self.config,
            &self.sandbox,
            &paths,
            &vm_binary,
            Some(snapshot),
            None,
        ) {
            Ok(child) => child,
            Err(err) => {
                virtiofs.kill();
                cleanup_files(&paths);
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };
        let vm_pid = vm.id();
        let relay_pid = match start_relay(&paths, &serial_log) {
            Ok(pid) => pid,
            Err(err) => {
                let _ = vm.kill();
                let _ = vm.wait();
                virtiofs.kill();
                cleanup_files(&paths);
                network::release(&self.config, &self.sandbox.name);
                return Err(err);
            }
        };

        if let Err(err) = wait_for_vm(&paths.api_socket, &mut vm, API_START_TIMEOUT).await {
            let _ = vm.kill();
            let _ = vm.wait();
            terminate_relay(relay_pid, &hegn);
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.sandbox.name);
            return Err(err).context("failed to restore the Cloud Hypervisor snapshot");
        }

        let record = SessionRecord {
            project: self.sandbox.project.clone(),
            workspace: self.sandbox.workspace.clone(),
            sandbox: self.sandbox.name.clone(),
            harness: existing
                .as_ref()
                .map(|session| session.harness)
                .unwrap_or(self.sandbox.harness),
            started_at,
            vm_pid,
            vm_binary: Some(vm_binary.clone()),
            relay_pid,
            virtiofs_pid,
            boot_args: boot_args(&self.config.root_device),
            serial_log: Some(serial_log),
        };
        let recorded = (|| -> Result<()> {
            write_private(&paths.vm_pid, vm_pid.to_string())?;
            write_private(&paths.relay_pid, relay_pid.to_string())?;
            write_private(&paths.session, serde_json::to_string_pretty(&record)?)?;
            Ok(())
        })();
        if let Err(err) = recorded {
            let _ = vm.kill();
            let _ = vm.wait();
            terminate_relay(relay_pid, &hegn);
            virtiofs.kill();
            cleanup_files(&paths);
            network::release(&self.config, &self.sandbox.name);
            return Err(err).context("failed to record the restored sandbox session");
        }

        // The restored VM runs for the rest of the hegn session.
        std::mem::forget(vm);
        std::mem::forget(virtiofs);
        Ok(())
    }

    /// Is the VM running or paused and answering the API?
    pub fn running(&self) -> bool {
        let paths = self.paths();
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return false,
        };
        runtime.block_on(async {
            CloudHypervisorClient::new(&paths.api_socket)
                .info()
                .await
                .map(|info| info.state.is_alive())
                .unwrap_or(false)
        })
    }

    fn cleanup_stale(&self) {
        let paths = self.paths();
        if let Some(session) = self.session() {
            let binary = self.config.vm_binary();
            if process_matches(session.vm_pid, &binary) {
                return;
            }
            terminate(session.vm_pid, &binary);
            if let Ok(binary) = hegn_binary() {
                terminate_relay(session.relay_pid, &binary);
            }
            terminate(session.virtiofs_pid, &self.config.virtiofsd_bin);
        }
        cleanup_files(&paths);
        network::release(&self.config, &self.sandbox.name);
    }
}

fn virtiofs_config(
    config: &Config,
    paths: &SandboxPaths,
    project: &Path,
) -> Result<VirtiofsConfig> {
    let binary = config.resolved_virtiofsd().with_context(|| {
        format!(
            "virtiofsd binary {} was not found on PATH or in {}; install virtiofsd or set \
             `virtiofsd_bin`",
            config.virtiofsd_bin.display(),
            VIRTIOFSD_FALLBACKS.join(", ")
        )
    })?;
    let mut virtiofs = VirtiofsConfig::new(binary, project, paths.virtiofs_socket.clone());
    // Honour the configured sandbox mode. The default is namespace; chroot is
    // available for hosts where user namespaces are restricted.
    virtiofs.sandbox = config.virtiofs_sandbox;
    Ok(virtiofs)
}

/// A sandbox entry shown in the CLI and TUI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SandboxSummary {
    pub name: String,
    pub project: PathBuf,
    pub workspace: PathBuf,
    pub harness: Harness,
    pub ephemeral: bool,
    pub source_sandbox: Option<String>,
    pub state: String,
    pub vm_pid: Option<u32>,
    pub serial_log: Option<PathBuf>,
    pub config_path: PathBuf,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub error: Option<String>,
}

/// Multi-sandbox registry. Each sandbox owns its config, runtime directory and
/// workspace; the global [`Config`] only supplies machine-wide settings.
#[derive(Debug, Clone)]
pub struct SandboxRegistry {
    config: Config,
    store: SandboxStore,
}

impl SandboxRegistry {
    pub fn new(config: Config) -> Self {
        let store = SandboxStore::new(config.clone());
        Self { config, store }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn store(&self) -> &SandboxStore {
        &self.store
    }

    pub fn list(&self) -> Result<Vec<SandboxConfig>> {
        self.store.list()
    }

    pub fn manager(&self, sandbox: SandboxConfig) -> SandboxManager {
        SandboxManager::new(self.config.clone(), sandbox)
    }

    pub fn manager_by_name(&self, name: &str) -> Result<SandboxManager> {
        Ok(self.manager(self.store.load(name)?))
    }

    pub fn create(
        &self,
        project: &Path,
        name: Option<&str>,
        harness: Option<Harness>,
        clone: bool,
    ) -> Result<SandboxConfig> {
        self.store
            .create_from_project(project, name, harness, clone)
    }

    /// Like [`Self::create`], but returns non-fatal clone warnings for callers
    /// that can surface them (for example, git's empty-repository notice).
    pub fn create_with_warnings(
        &self,
        project: &Path,
        name: Option<&str>,
        harness: Option<Harness>,
        clone: bool,
    ) -> Result<CloneOutcome> {
        self.store
            .create_from_project_with_warnings(project, name, harness, clone)
    }

    pub fn clone_from(
        &self,
        source: &SandboxConfig,
        name: &str,
        harness: Option<Harness>,
    ) -> Result<CloneOutcome> {
        self.store.clone_from(source, name, harness)
    }

    /// Resolve a sandbox by name, or by project name/path.
    pub fn resolve(&self, selector: &str) -> Result<SandboxConfig> {
        if let Ok(config) = self.store.load(selector) {
            return Ok(config);
        }
        let project = resolve_project_selector(&self.config, selector)?;
        let mut matches: Vec<SandboxConfig> = self
            .store
            .list()?
            .into_iter()
            .filter(|sandbox| same_project(&sandbox.project, &project))
            .collect();
        // Prefer the non-ephemeral base sandbox when clones share a project.
        matches.sort_by_key(|sandbox| sandbox.ephemeral);
        matches.into_iter().next().with_context(|| {
            format!(
                "no sandbox for {}; create one with `hegn sandbox create {}`",
                project.display(),
                project.display()
            )
        })
    }

    /// Return the base sandbox for a project, creating it if necessary.
    pub fn ensure_for_project(&self, project: &Path, harness: Harness) -> Result<SandboxConfig> {
        let project = project
            .canonicalize()
            .with_context(|| format!("project {} does not exist", project.display()))?;
        if let Some(existing) = self
            .store
            .list()?
            .into_iter()
            .find(|sandbox| !sandbox.ephemeral && same_project(&sandbox.project, &project))
        {
            return Ok(existing);
        }
        let base = SandboxConfig::suggested_name(&project);
        let name = self.unique_name(&base)?;
        self.store
            .create_from_project(&project, Some(&name), Some(harness), false)
    }

    /// Find a free sandbox name starting at `base`.
    pub fn unique_name(&self, base: &str) -> Result<String> {
        let base = crate::logs::sanitize_segment(base, 48);
        let mut candidate = base.clone();
        for suffix in 1..1000 {
            if !self.store.dir(&candidate)?.exists() {
                return Ok(candidate);
            }
            candidate = format!("{base}-{suffix}");
        }
        bail!("could not allocate a free sandbox name for '{base}'")
    }

    pub fn summaries(&self) -> Result<Vec<SandboxSummary>> {
        self.store
            .list()?
            .into_iter()
            .map(|sandbox| Ok(self.summary(&sandbox)))
            .collect()
    }

    pub fn summary(&self, sandbox: &SandboxConfig) -> SandboxSummary {
        let manager = self.manager(sandbox.clone());
        let paths = manager.paths();
        let session = manager.session();
        let vm_pid = session.as_ref().map(|session| session.vm_pid);
        let binary = session
            .as_ref()
            .and_then(|session| session.vm_binary.clone())
            .unwrap_or_else(|| self.config.vm_binary());
        let alive = vm_pid.is_some_and(|pid| process_matches(pid, &binary));
        let state = if alive { "running" } else { "stopped" }.to_string();
        SandboxSummary {
            name: sandbox.name.clone(),
            project: sandbox.project.clone(),
            workspace: sandbox.workspace.clone(),
            harness: sandbox.harness,
            ephemeral: sandbox.ephemeral,
            source_sandbox: sandbox.source_sandbox.clone(),
            state,
            vm_pid: vm_pid.filter(|pid| process_matches(*pid, &binary)),
            serial_log: session.and_then(|session| session.serial_log),
            config_path: paths
                .run_dir
                .parent()
                .unwrap_or(Path::new(""))
                .join("hegn.toml"),
            vcpus: sandbox.vcpus,
            memory_mib: sandbox.memory_mib,
            error: None,
        }
    }

    pub fn stop(&self, name: &str) -> Result<()> {
        self.manager_by_name(name)?.stop()
    }

    /// Delete a sandbox. The source project of a normal sandbox is never
    /// deleted; only an in-sandbox clone can be removed or kept.
    pub fn delete(&self, name: &str, delete_files: bool) -> Result<DeleteOutcome> {
        let manager = self.manager_by_name(name)?;
        if manager.is_running() {
            manager.stop()?;
        }
        network::release(&self.config, name);
        self.store.delete(name, delete_files)
    }
}

fn resolve_project_selector(config: &Config, selector: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(selector);
    if direct.is_dir() {
        return Ok(direct.canonicalize().unwrap_or(direct));
    }
    let candidate = config.projects_root.join(selector);
    if candidate.is_dir() {
        return Ok(candidate.canonicalize().unwrap_or(candidate));
    }
    bail!(
        "no sandbox named '{selector}' and no project at {} or {}",
        direct.display(),
        candidate.display()
    )
}

fn spawn_virtiofsd(
    config: &Config,
    paths: &SandboxPaths,
    project: &Path,
) -> Result<VirtiofsProcess> {
    let virtiofs = virtiofs_config(config, paths, project)?;
    let mut process = virtiofs.spawn().context("failed to start virtiofsd")?;
    if let Err(err) = process.wait_for_socket(Duration::from_secs(5)) {
        process.kill();
        return Err(err);
    }
    Ok(process)
}

/// Build the hardened command line for a normal boot.
fn cloud_hypervisor_args(
    config: &Config,
    sandbox: &SandboxConfig,
    paths: &SandboxPaths,
    rootfs: &Path,
    network: Option<&network::Lease>,
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
    args.push(format!("boot={},max={}", sandbox.vcpus, sandbox.vcpus).into());
    args.push("--memory".into());
    args.push(format!("size={}M,shared=on", sandbox.memory_mib).into());
    args.push("--fs".into());
    args.push(
        format!(
            "tag={},socket={},num_queues=1,queue_size=1024",
            sandbox.shared_tag,
            paths.virtiofs_socket.display()
        )
        .into(),
    );
    if let Some(lease) = network {
        args.push("--net".into());
        args.push(lease.cloud_hypervisor_arg().into());
    }
    args.push("--serial".into());
    args.push(format!("socket={}", paths.serial_socket.display()).into());
    args.push("--console".into());
    args.push("off".into());
    args
}

/// Build the command line for a restore. The VM configuration comes from the
/// snapshot itself, so only VMM-level options are set here.
fn restore_args(config: &Config, paths: &SandboxPaths, snapshot: &SnapshotMeta) -> Vec<OsString> {
    let mut args = common_args(config, paths, false);
    args.push("--restore".into());
    args.push(format!("source_url={},resume=true", snapshot.source_url()).into());
    args
}

fn common_args(config: &Config, paths: &SandboxPaths, landlock: bool) -> Vec<OsString> {
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

fn spawn_cloud_hypervisor(
    config: &Config,
    sandbox: &SandboxConfig,
    paths: &SandboxPaths,
    binary: &Path,
    restore: Option<&SnapshotMeta>,
    network: Option<&network::Lease>,
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
            let rootfs = sandbox.prepare_rootfs(config)?;
            cloud_hypervisor_args(config, sandbox, paths, &rootfs, network)
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

fn start_relay(paths: &SandboxPaths, serial_log: &Path) -> Result<u32> {
    remove_socket(&paths.console_socket)?;
    let args = RelayArgs {
        backend: paths.serial_socket.clone(),
        frontend: paths.console_socket.clone(),
        log: serial_log.to_path_buf(),
    };
    let pid = relay::spawn(&args)?;
    let binary = hegn_binary()?;
    let deadline = Instant::now() + RELAY_START_TIMEOUT;
    while Instant::now() < deadline {
        if is_socket(&paths.console_socket) && process_alive(pid) {
            return Ok(pid);
        }
        if !process_alive(pid) {
            bail!("the console relay exited before creating its socket");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    terminate_relay(pid, &binary);
    bail!(
        "timed out waiting for the console relay socket {}",
        paths.console_socket.display()
    )
}

async fn wait_for_vm(socket: &Path, child: &mut Child, timeout: Duration) -> Result<()> {
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

fn ensure_socket(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!(
            "no Cloud Hypervisor API socket at {} — start a sandbox first",
            path.display()
        );
    }
    Ok(())
}

fn hegn_binary() -> Result<PathBuf> {
    std::env::current_exe().context("failed to locate the hegn executable")
}

fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_socket())
        .unwrap_or(false)
}

/// Remove a socket or stale socket file, but never a regular file or symlink.
fn remove_socket(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path)
            .with_context(|| format!("failed to remove stale socket {}", path.display())),
        Ok(_) => bail!(
            "refusing to replace {}: it exists and is not a socket",
            path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

/// Tighten Cloud Hypervisor's snapshot output to `0700`/`0600`.
///
/// The snapshot directory is already private, but setting the file modes too
/// keeps the invariant robust if the directory is ever moved or re-created.
fn secure_snapshot_dir(dir: &Path) -> Result<()> {
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

fn dir_size(dir: &Path) -> u64 {
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

/// Append a timestamped lifecycle line to `run/hegn.log`.
///
/// Best effort: a logging failure must never mask the VM start result.
fn append_event_log(path: &Path, message: &str) {
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
fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
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

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether two project paths refer to the same directory.
///
/// The TUI discovers projects as `<projects_root>/<name>` while the CLI
/// canonicalizes them, so a symlinked projects root would otherwise look like a
/// different project (and trigger a bogus "sandbox already running" error).
pub fn same_project(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn process_alive(pid: u32) -> bool {
    // `kill(0, ...)` and `kill(-pgrp, ...)` target hegn's own process group or
    // an arbitrary group, so only real positive PIDs are considered.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // Safety: kill(2) with signal 0 only probes for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Whether `pid` is alive *and* still runs the expected executable.
///
/// PID files and `session.json` outlive crashes and reboots. Without this check
/// a recycled PID could make hegn report a stale VM as running, or worse, send
/// SIGKILL to an unrelated process.
pub fn process_matches(pid: u32, binary: &Path) -> bool {
    if !process_alive(pid) {
        return false;
    }
    let Some(expected) = binary.file_name() else {
        return false;
    };
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or(&[]);
    let argv0 = String::from_utf8_lossy(argv0);
    Path::new(argv0.as_ref())
        .file_name()
        .is_some_and(|actual| actual == expected)
}

fn terminate(pid: u32, binary: &Path) {
    if !process_matches(pid, binary) {
        return;
    }
    signal_and_wait(pid, || process_matches(pid, binary));
}

fn terminate_relay(pid: u32, binary: &Path) {
    if !relay::process_matches_relay(pid, binary) {
        return;
    }
    signal_and_wait(pid, || relay::process_matches_relay(pid, binary));
}

fn signal_and_wait(pid: u32, still_running: impl Fn() -> bool) {
    // Safety: the pid was verified to belong to the expected executable.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !still_running() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !still_running() {
        return;
    }
    // Safety: same verified process as above; escalate after the grace period.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

fn cleanup_files(paths: &SandboxPaths) {
    for path in [
        &paths.api_socket,
        &paths.serial_socket,
        &paths.console_socket,
        &paths.virtiofs_socket,
        &paths.vm_pid,
        &paths.relay_pid,
        &paths.virtiofs_pid,
        &paths.session,
        &paths.cloud_init,
    ] {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::BackendKind;

    fn config_with_assets(dir: &Path) -> Config {
        let mut config = Config::default();
        config.state_dir = dir.join("state");
        config.projects_root = dir.join("projects");
        config.snapshots_dir = dir.join("snapshots");
        config.sandboxes_dir = dir.join("sandboxes");
        config.kernel_image = dir.join("vmlinux");
        config.rootfs_image = dir.join("rootfs.qcow2");
        config.rootfs_format = crate::config::RootfsFormat::Qcow2;
        config.root_device = "/dev/vda1".into();
        config.backend = BackendKind::CloudHypervisor;
        config.cloud_hypervisor_bin = PathBuf::from("/definitely/missing/cloud-hypervisor");
        config.virtiofsd_bin = PathBuf::from("/definitely/missing/virtiofsd");
        std::fs::write(&config.kernel_image, b"kernel").unwrap();
        std::fs::write(&config.rootfs_image, b"rootfs").unwrap();
        std::fs::create_dir_all(&config.projects_root).unwrap();
        config
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn sandbox_for(workspace: &Path) -> SandboxConfig {
        std::fs::create_dir_all(workspace).unwrap();
        SandboxConfig {
            name: "demo".into(),
            project: workspace.to_path_buf(),
            workspace: workspace.to_path_buf(),
            harness: Harness::Shell,
            vcpus: 1,
            memory_mib: 128,
            ..SandboxConfig::default()
        }
    }

    fn manager_for(config: Config, workspace: &Path) -> SandboxManager {
        SandboxManager::new(config, sandbox_for(workspace))
    }

    fn manager(config: Config, dir: &Path) -> SandboxManager {
        manager_for(config, &dir.join("project"))
    }

    #[test]
    fn paths_live_under_sandbox_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let sandbox = sandbox_for(&dir.path().join("project"));
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        assert_eq!(paths.run_dir, config.sandboxes_dir.join("demo").join("run"));
        assert_eq!(
            paths.api_socket.file_name().unwrap(),
            "cloud-hypervisor.sock"
        );
        assert_eq!(paths.serial_socket.file_name().unwrap(), "serial.sock");
        assert_eq!(paths.console_socket.file_name().unwrap(), "console.sock");
        assert_eq!(paths.virtiofs_socket.file_name().unwrap(), "virtiofs.sock");
    }

    #[test]
    fn run_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let run_dir = manager.paths().run_dir;
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        manager.paths().ensure().unwrap();
        let mode = std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "an existing run directory must be tightened");
    }

    #[test]
    fn run_directory_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(paths.run_dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &paths.run_dir).unwrap();

        let err = paths.ensure().unwrap_err().to_string();
        assert!(err.contains("symlinked run directory"), "{err}");
    }

    #[test]
    fn start_requires_assets() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.kernel_image = dir.path().join("missing");
        let manager = manager(config, dir.path());
        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("missing VM assets"), "{err}");
    }

    #[test]
    fn start_preflights_binaries_before_spawning_anything() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();

        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("cloud-hypervisor binary"), "{err}");
        assert!(!manager.paths().virtiofs_pid.exists());
        assert!(!manager.paths().api_socket.exists());
    }

    #[test]
    fn lifecycle_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        let held = manager.lock(&paths).unwrap();
        let err = manager.lock(&paths).unwrap_err().to_string();
        assert!(err.contains("in progress"), "{err}");
        drop(held);
        assert!(
            manager.lock(&paths).is_ok(),
            "lock must be released on drop"
        );
    }

    #[test]
    fn same_project_compares_canonical_paths() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(same_project(&real, &link));
        assert!(!same_project(&real, &dir.path().join("missing")));
    }

    #[test]
    fn start_requires_existing_project() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-project");
        let manager = manager_for(config_with_assets(dir.path()), &missing);
        std::fs::remove_dir_all(&missing).unwrap();
        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn start_reports_missing_virtiofsd() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        // Make the Cloud Hypervisor preflight pass so this test reaches virtiofsd.
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        let manager = manager(config, dir.path());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let err = manager.start().unwrap_err().to_string();
        assert!(
            err.contains("virtiofsd") || err.contains("failed to start"),
            "{err}"
        );
    }

    #[test]
    fn start_refuses_an_unreadable_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        let fake_virtiofsd = dir.path().join("virtiofsd");
        std::fs::write(&fake_virtiofsd, b"#!/bin/sh\n").unwrap();
        config.virtiofsd_bin = fake_virtiofsd;
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();

        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert_eq!(std::fs::read(&paths.session).unwrap(), b"{not json");
    }

    #[test]
    fn stop_refuses_an_unreadable_session() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();

        let err = manager.stop().unwrap_err().to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert!(
            paths.session.exists(),
            "unreadable metadata must be preserved"
        );
    }

    #[test]
    fn stop_without_session_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        assert!(manager.stop().is_err());
        assert!(!manager.is_running());
    }

    #[test]
    fn status_without_session_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let report = manager.status().unwrap();
        assert_eq!(report.state, "stopped");
        assert!(!report.virtiofs_running);
        assert!(!report.relay_running);
        assert_eq!(report.project, Some(dir.path().join("project")));
    }

    #[test]
    fn session_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config, dir.path());
        manager.paths().ensure().unwrap();
        let record = SessionRecord {
            project: dir.path().join("p"),
            workspace: dir.path().join("p"),
            sandbox: "demo".into(),
            harness: Harness::Codex,
            started_at: Utc::now(),
            vm_pid: 1234,
            vm_binary: None,
            relay_pid: 5678,
            virtiofs_pid: 4321,
            boot_args: boot_args("/dev/vda1"),
            serial_log: None,
        };
        std::fs::write(
            manager.paths().session,
            serde_json::to_string_pretty(&record).unwrap(),
        )
        .unwrap();
        let loaded = manager.session().unwrap();
        assert_eq!(loaded.harness, Harness::Codex);
        assert_eq!(loaded.vm_pid, 1234);
        assert_eq!(loaded.relay_pid, 5678);
    }

    #[test]
    fn failed_start_leaves_a_private_lifecycle_log() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.kernel_image = dir.path().join("missing-kernel");
        let manager = manager(config, dir.path());
        let err = manager.start().unwrap_err().to_string();
        assert!(err.contains("VM log:"), "{err}");
        let log = manager.paths().event_log;
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("start failed"), "{text}");
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

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
    fn process_alive_detects_self() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(4_000_000_000));
    }

    #[test]
    fn process_alive_rejects_pid_zero_and_out_of_range() {
        // `kill(0, 0)` probes hegn's own process group and would otherwise
        // report true (and allow terminate(0) to signal our own group).
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
        assert!(!process_alive(i32::MAX as u32 + 1));
    }

    #[test]
    fn process_matches_verifies_the_executable() {
        let exe = std::env::current_exe().unwrap();
        assert!(process_matches(std::process::id(), &exe));
        assert!(!process_matches(
            std::process::id(),
            Path::new("/definitely/not/this-binary")
        ));
        assert!(!process_matches(0, &exe));
    }

    #[test]
    fn restore_rejects_missing_snapshot_files() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snapshot = SnapshotMeta {
            name: "missing".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.path().join("missing"),
            sandbox: Some("demo".into()),
            project: Some(dir.path().join("project")),
            size_bytes: 0,
            note: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn restore_requires_an_existing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("project");
        let manager = manager_for(config_with_assets(dir.path()), &workspace);
        std::fs::remove_dir_all(&workspace).unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        for name in ["config.json", "state.json", "memory-ranges"] {
            std::fs::write(snap_dir.join(name), b"x").unwrap();
        }
        let snapshot = SnapshotMeta {
            name: "orphan".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: snap_dir,
            sandbox: Some("demo".into()),
            project: None,
            size_bytes: 0,
            note: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("workspace") && err.contains("no longer exists"),
            "{err}"
        );
    }

    #[test]
    fn restore_refuses_a_snapshot_from_another_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(config_with_assets(dir.path()), dir.path());
        let snapshot = SnapshotMeta {
            name: "shared".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: dir.path().join("snap"),
            sandbox: Some("someone-else".into()),
            project: Some(dir.path().join("project")),
            size_bytes: 0,
            note: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("belongs to sandbox 'someone-else'"), "{err}");
    }

    #[test]
    fn session_records_from_before_multi_sandbox_still_load() {
        let legacy = serde_json::json!({
            "project": "/projects/demo",
            "harness": "shell",
            "started_at": "2026-01-01T00:00:00Z",
            "vm_pid": 123,
            "relay_pid": 456,
            "virtiofs_pid": 789,
            "boot_args": "console=ttyS0",
        });
        let record: SessionRecord = serde_json::from_value(legacy).unwrap();
        assert!(record.workspace.as_os_str().is_empty());
        assert!(record.sandbox.is_empty());
        assert_eq!(record.vm_pid, 123);
    }

    #[test]
    fn running_state_uses_the_recorded_vm_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = PathBuf::from("/definitely/missing/cloud-hypervisor");
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: dir.path().join("project"),
            workspace: dir.path().join("project"),
            sandbox: "demo".into(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            vm_pid: std::process::id(),
            vm_binary: Some(std::env::current_exe().unwrap()),
            relay_pid: 0,
            virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: None,
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
        assert!(manager.is_running());
    }

    #[test]
    fn restore_refuses_an_unreadable_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        config.virtiofsd_bin = std::env::current_exe().unwrap();
        let manager = manager(config, dir.path());
        let paths = manager.paths();
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        for name in ["config.json", "state.json", "memory-ranges"] {
            std::fs::write(snap_dir.join(name), b"x").unwrap();
        }
        let snapshot = SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: snap_dir,
            sandbox: Some("demo".into()),
            project: Some(dir.path().join("project")),
            size_bytes: 0,
            note: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert!(
            paths.session.exists(),
            "unreadable metadata must be preserved"
        );
    }

    #[test]
    fn restore_preflights_binaries_before_stopping_a_vm() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_hypervisor_bin = PathBuf::from("/definitely/missing/cloud-hypervisor");
        config.virtiofsd_bin = PathBuf::from("/definitely/missing/virtiofsd");
        let manager = manager(config, dir.path());
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        for name in ["config.json", "state.json", "memory-ranges"] {
            std::fs::write(snap_dir.join(name), b"x").unwrap();
        }
        let snapshot = SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: snap_dir,
            sandbox: Some("demo".into()),
            project: Some(dir.path().join("project")),
            size_bytes: 0,
            note: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(manager.restore_snapshot(&snapshot))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cloud-hypervisor binary"), "{err}");
    }

    #[test]
    fn virtiofsd_honours_the_configured_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.virtiofsd_bin = std::env::current_exe().unwrap();
        config.virtiofs_sandbox = crate::config::VirtiofsSandbox::Chroot;
        let sandbox = sandbox_for(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        let virtiofs = virtiofs_config(&config, &paths, dir.path()).unwrap();
        assert_eq!(virtiofs.sandbox, crate::config::VirtiofsSandbox::Chroot);
    }

    #[test]
    fn cloud_hypervisor_args_are_hardened_and_use_native_virtiofs() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let sandbox = sandbox_for(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        let args = strings(&cloud_hypervisor_args(
            &config,
            &sandbox,
            &paths,
            &sandbox.rootfs_path(&config),
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
    fn snapshot_tap_names_are_read_from_the_device_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"net":[{"tap":"hegn3","mac":"52:54:00:00:00:04"}]}"#,
        )
        .unwrap();
        assert_eq!(snapshot_network_taps(dir.path()).unwrap(), vec!["hegn3"]);
        std::fs::write(dir.path().join("config.json"), r#"{"net":[]}"#).unwrap();
        assert!(snapshot_network_taps(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn guarded_network_adds_only_a_tap_and_mac() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let sandbox = sandbox_for(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        let lease = crate::network::Lease {
            index: 2,
            sandbox: sandbox.name.clone(),
            tap: crate::network::tap_name(2),
            mac: crate::network::mac(2),
            host_ip: crate::network::host_ip(2),
            guest_ip: crate::network::guest_ip(2),
        };
        let joined = strings(&cloud_hypervisor_args(
            &config,
            &sandbox,
            &paths,
            &sandbox.rootfs_path(&config),
            Some(&lease),
        ))
        .join(" ");
        assert!(
            joined.contains("--net tap=hegn2,mac=52:54:00:00:00:03"),
            "{joined}"
        );
        // The host TAP address is owned by `hegn network setup`, not the VMM.
        assert!(!joined.contains("ip=10.213.0."), "{joined}");
        assert!(!joined.contains("mask="), "{joined}");
    }

    #[test]
    fn air_gapped_boot_has_no_network_device() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let sandbox = sandbox_for(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        let joined = strings(&cloud_hypervisor_args(
            &config,
            &sandbox,
            &paths,
            &sandbox.rootfs_path(&config),
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
        let sandbox = sandbox_for(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        let joined = strings(&cloud_hypervisor_args(
            &config,
            &sandbox,
            &paths,
            &sandbox.rootfs_path(&config),
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
        let sandbox = sandbox_for(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        let snap_dir = dir.path().join("snap");
        let snapshot = SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            dir: snap_dir.clone(),
            sandbox: Some("demo".into()),
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

    #[test]
    fn socket_cleanup_refuses_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-hypervisor.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        let err = remove_socket(&path).unwrap_err().to_string();
        assert!(err.contains("not a socket"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    }

    #[test]
    fn cloud_init_seed_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.cloud_init = false;
        let sandbox = sandbox_for(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        let args = strings(&cloud_hypervisor_args(
            &config,
            &sandbox,
            &paths,
            &sandbox.rootfs_path(&config),
            None,
        ));
        assert!(
            !args.iter().any(|arg| arg.contains("cloud-init.img")),
            "{args:?}"
        );
    }

    #[test]
    fn registry_allows_multiple_sandboxes_and_resolves_projects() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let registry = SandboxRegistry::new(config.clone());
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let base = registry
            .ensure_for_project(&project, Harness::Shell)
            .unwrap();
        assert_eq!(base.name, "demo");
        assert!(!base.ephemeral);
        // A second call reuses the base sandbox.
        assert_eq!(
            registry
                .ensure_for_project(&project, Harness::Shell)
                .unwrap()
                .name,
            "demo"
        );
        assert_eq!(registry.resolve("demo").unwrap().name, "demo");
        assert_eq!(
            registry.resolve(project.to_str().unwrap()).unwrap().name,
            "demo"
        );
        let clone = registry
            .clone_from(&base, "demo-agent-1", Some(Harness::Codex))
            .unwrap()
            .sandbox;
        assert!(clone.ephemeral);
        assert_eq!(registry.list().unwrap().len(), 2);
        let summaries = registry.summaries().unwrap();
        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().all(|summary| summary.state == "stopped"));
        assert_eq!(registry.unique_name("demo").unwrap(), "demo-1");
    }

    #[test]
    fn report_serializes() {
        let dir = tempfile::tempdir().unwrap();
        let report = manager(config_with_assets(dir.path()), dir.path())
            .status()
            .unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["state"], "stopped");
        assert_eq!(json["backend"], "cloud-hypervisor");
    }
}
