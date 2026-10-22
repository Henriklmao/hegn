//! Firecracker lifecycle orchestration.
//!
//! One hegn state directory manages one VM at a time:
//!
//! ```text
//! <state>/run/
//!   firecracker.sock
//!   virtiofs.sock
//!   serial.in        (FIFO: guest serial input)
//!   serial.out       (serial log: guest serial output)
//!   firecracker.pid
//!   virtiofs.pid
//!   session.json
//!   firecracker.log
//!   hegn.lock
//! ```
//!
//! `start` boots virtiofsd followed by Firecracker, configures the API and
//! issues `InstanceStart`. The child processes deliberately outlive the CLI
//! process; `stop` terminates them. VM lifecycle operations hold an exclusive
//! lock on `hegn.lock` so two concurrent `hegn open` calls cannot boot
//! Firecracker on the same sockets and serial FIFO.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Harness};
use crate::firecracker::{
    Action, BootSource, Drive, FirecrackerClient, InstanceInfo, MachineConfig, SnapshotCreate,
    SnapshotLoad, VhostUserFs, VmState,
};
use crate::logs::{DEFAULT_KEEP, LogStore};
use crate::snapshot::{SnapshotMeta, SnapshotStore};
use crate::virtiofs::{VirtiofsConfig, VirtiofsProcess};

const API_START_TIMEOUT: Duration = Duration::from_secs(5);
const ROOT_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPaths {
    pub run_dir: PathBuf,
    pub api_socket: PathBuf,
    pub virtiofs_socket: PathBuf,
    /// FIFO connected to Firecracker's stdin (guest serial input).
    pub serial_in: PathBuf,
    /// File connected to Firecracker's stdout (guest serial output).
    pub serial_out: PathBuf,
    pub firecracker_pid: PathBuf,
    pub virtiofs_pid: PathBuf,
    pub session: PathBuf,
    pub log: PathBuf,
    /// Exclusive lock serializing VM lifecycle operations.
    pub lock: PathBuf,
}

impl SandboxPaths {
    pub fn from_config(config: &Config) -> Self {
        let run_dir = config.run_dir();
        Self {
            api_socket: run_dir.join("firecracker.sock"),
            virtiofs_socket: run_dir.join("virtiofs.sock"),
            serial_in: run_dir.join("serial.in"),
            serial_out: run_dir.join("serial.out"),
            firecracker_pid: run_dir.join("firecracker.pid"),
            virtiofs_pid: run_dir.join("virtiofs.pid"),
            session: run_dir.join("session.json"),
            log: run_dir.join("firecracker.log"),
            lock: run_dir.join("hegn.lock"),
            run_dir,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        // The API and virtiofs sockets live here; a private directory keeps
        // other local users from driving the microVM.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(&self.run_dir)
                .with_context(|| format!("failed to create {}", self.run_dir.display()))?;
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
    pub harness: Harness,
    pub started_at: DateTime<Utc>,
    pub firecracker_pid: u32,
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
    pub project: Option<PathBuf>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub api_socket: PathBuf,
    pub virtiofs_socket: PathBuf,
    pub virtiofs_running: bool,
    pub firecracker_pid: Option<u32>,
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
}

/// Exclusive lock held for the duration of a VM lifecycle operation.
///
/// Dropping it (including on an error path) releases the lock.
#[derive(Debug)]
struct RunLock {
    _file: File,
}

impl SandboxManager {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub fn paths(&self) -> SandboxPaths {
        SandboxPaths::from_config(&self.config)
    }

    pub fn session(&self) -> Option<SessionRecord> {
        let raw = fs::read_to_string(self.paths().session).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn is_running(&self) -> bool {
        self.session()
            .map(|session| process_matches(session.firecracker_pid, &self.config.firecracker_bin))
            .unwrap_or(false)
    }

    /// Take the exclusive VM lifecycle lock.
    ///
    /// A second `hegn open`/`sandbox start` must not recreate `serial.in` or the
    /// API socket underneath a VM that is already booting.
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

    /// Boot virtiofsd + Firecracker and start the guest.
    pub fn start(&self, project: &Path, harness: Harness) -> Result<SandboxReport> {
        let paths = self.paths();
        // A missing VM is always a hard error: there is no host-process fallback.
        self.config.require_vm_assets()?;
        if !project.is_dir() {
            bail!("project {} does not exist", project.display());
        }
        // Preflight both binaries before spawning anything, so a typo in the
        // configuration cannot leave a stray virtiofsd (or a VM without a
        // console) behind.
        if !command_available(&self.config.firecracker_bin) {
            bail!(
                "firecracker binary {} was not found; run `hegn doctor`",
                self.config.firecracker_bin.display()
            );
        }
        if !command_available(&self.config.virtiofsd_bin) {
            bail!(
                "virtiofsd binary {} was not found; run `hegn doctor`",
                self.config.virtiofsd_bin.display()
            );
        }
        let _lock = self.lock(&paths)?;
        if self.is_running() {
            bail!("a sandbox is already running (use `hegn sandbox stop` first)");
        }
        if self.session().is_some() {
            self.cleanup_stale();
        }
        paths.ensure()?;
        // Build the runtime before spawning anything so a failure here cannot
        // leak a running virtiofsd or Firecracker process.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build async runtime")?;

        // Serial output is written to a private, persistent log and exposed in
        // the run directory through a hard link. Create the log before any
        // process starts so a boot log is never lost and a logging problem is
        // a hard error rather than a silent downgrade.
        let started_at = Utc::now();
        let logs = LogStore::new(self.config.logs_dir());
        if let Err(err) = logs.prune(DEFAULT_KEEP) {
            eprintln!("warning: could not prune old serial logs: {err:#}");
        }
        let serial_log = logs.create_session(project, started_at, std::process::id())?;

        // 1. virtiofsd, sandboxed to exactly this project.
        let mut virtiofs = match spawn_virtiofsd(&self.config, &paths, project) {
            Ok(process) => process,
            Err(err) => {
                let _ = fs::remove_file(&serial_log);
                return Err(err);
            }
        };
        let virtiofs_pid = virtiofs.pid();
        if let Err(err) = fs::write(&paths.virtiofs_pid, virtiofs_pid.to_string()) {
            virtiofs.kill();
            cleanup_files(&paths);
            let _ = fs::remove_file(&serial_log);
            return Err(err).context("failed to record the virtiofsd pid");
        }

        // 2. Firecracker process. Children outlive this CLI call.
        let mut firecracker = match spawn_firecracker(&self.config, &paths, &serial_log) {
            Ok(child) => child,
            Err(err) => {
                virtiofs.kill();
                cleanup_files(&paths);
                let _ = fs::remove_file(&serial_log);
                return Err(err);
            }
        };
        let firecracker_pid = firecracker.id();

        let configured = runtime.block_on(async {
            wait_for_api(&paths.api_socket, &mut firecracker, API_START_TIMEOUT).await?;
            let client = FirecrackerClient::new(&paths.api_socket);
            client
                .put_machine_config(&MachineConfig {
                    vcpu_count: self.config.vcpus,
                    mem_size_mib: self.config.memory_mib,
                    smt: Some(false),
                    track_dirty_pages: Some(true),
                })
                .await?;
            client
                .put_boot_source(&BootSource {
                    kernel_image_path: self.config.kernel_image.display().to_string(),
                    boot_args: ROOT_BOOT_ARGS.to_string(),
                })
                .await?;
            client
                .put_drive(&Drive {
                    drive_id: "rootfs".into(),
                    path_on_host: self.config.rootfs_image.display().to_string(),
                    is_root_device: true,
                    is_read_only: false,
                })
                .await?;
            client
                .put_vhost_user_fs(&VhostUserFs {
                    fs_id: "workspace".into(),
                    socket: paths.virtiofs_socket.display().to_string(),
                    tag: self.config.shared_tag.clone(),
                })
                .await?;
            client.action(Action::InstanceStart).await?;
            Ok::<_, anyhow::Error>(())
        });

        if let Err(err) = configured {
            let _ = firecracker.kill();
            let _ = firecracker.wait();
            virtiofs.kill();
            cleanup_files(&paths);
            let _ = fs::remove_file(&serial_log);
            return Err(err).context("failed to configure Firecracker");
        }

        let record = SessionRecord {
            project: project.to_path_buf(),
            harness,
            started_at,
            firecracker_pid,
            virtiofs_pid,
            boot_args: ROOT_BOOT_ARGS.to_string(),
            serial_log: Some(serial_log.clone()),
        };
        let recorded = (|| -> Result<()> {
            fs::write(&paths.firecracker_pid, firecracker_pid.to_string())?;
            fs::write(&paths.session, serde_json::to_string_pretty(&record)?)?;
            Ok(())
        })();
        if let Err(err) = recorded {
            // Without the session record a later `stop` cannot find the VM, so
            // tear everything down instead of leaking a running Firecracker.
            let _ = firecracker.kill();
            let _ = firecracker.wait();
            virtiofs.kill();
            cleanup_files(&paths);
            let _ = fs::remove_file(&serial_log);
            return Err(err).context("failed to record the sandbox session");
        }

        // Do not kill the children on drop.
        std::mem::forget(firecracker);
        std::mem::forget(virtiofs);

        Ok(SandboxReport {
            state: "running".into(),
            backend: "firecracker".into(),
            project: Some(project.to_path_buf()),
            vcpus: self.config.vcpus,
            memory_mib: self.config.memory_mib,
            api_socket: paths.api_socket,
            virtiofs_socket: paths.virtiofs_socket,
            virtiofs_running: true,
            firecracker_pid: Some(firecracker_pid),
            virtiofs_pid: Some(virtiofs_pid),
            started_at: Some(record.started_at),
            serial_log: Some(serial_log),
            note: Some(format!(
                "guest workspace mounted at {}",
                self.config.guest_mount
            )),
        })
    }

    /// Attach the TUI/CLI terminal to the running guest serial console.
    pub fn attach_console(&self) -> Result<crate::console::VmConsole> {
        crate::console::VmConsole::attach(&self.config)
    }

    /// Boot a VM if needed, then attach its console.
    pub fn ensure_console(
        &self,
        project: &Path,
        harness: Harness,
    ) -> Result<crate::console::VmConsole> {
        // Only a *running* sandbox for another project is a conflict. A stale
        // session record (crash, reboot) must not block opening a project; the
        // start path cleans it up.
        if let Some(session) = self.session()
            && !same_project(&session.project, project)
            && process_matches(session.firecracker_pid, &self.config.firecracker_bin)
        {
            bail!(
                "a sandbox for {} is already running; stop it with `hegn sandbox stop` first",
                session.project.display()
            );
        }
        if !self.is_running() {
            self.start(project, harness)?;
        }
        self.attach_console()
    }

    /// Stop Firecracker and virtiofsd.
    pub fn stop(&self) -> Result<()> {
        let paths = self.paths();
        if !paths.run_dir.is_dir() {
            bail!("no sandbox session found in {}", paths.run_dir.display());
        }
        let _lock = self.lock(&paths)?;
        let session = self.session();
        let firecracker_pid = session
            .as_ref()
            .map(|s| s.firecracker_pid)
            .or_else(|| read_pid(&paths.firecracker_pid));
        let virtiofs_pid = session
            .as_ref()
            .map(|s| s.virtiofs_pid)
            .or_else(|| read_pid(&paths.virtiofs_pid));

        if firecracker_pid.is_none() && virtiofs_pid.is_none() {
            bail!("no sandbox session found in {}", paths.run_dir.display());
        }

        if let Some(pid) = firecracker_pid {
            terminate(pid, &self.config.firecracker_bin);
        }
        if let Some(pid) = virtiofs_pid {
            terminate(pid, &self.config.virtiofsd_bin);
        }
        cleanup_files(&paths);
        Ok(())
    }

    /// Pause the guest (used before snapshot creation).
    pub async fn pause(&self) -> Result<()> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        FirecrackerClient::new(&paths.api_socket).pause().await
    }

    pub async fn resume(&self) -> Result<()> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        FirecrackerClient::new(&paths.api_socket).resume().await
    }

    pub async fn instance_info(&self) -> Result<InstanceInfo> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        FirecrackerClient::new(&paths.api_socket)
            .instance_info()
            .await
    }

    pub fn status(&self) -> Result<SandboxReport> {
        let paths = self.paths();
        let session = self.session();
        let virtiofs_running = session
            .as_ref()
            .map(|s| process_matches(s.virtiofs_pid, &self.config.virtiofsd_bin))
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
            match runtime.block_on(FirecrackerClient::new(&paths.api_socket).instance_info()) {
                Ok(info) => {
                    state = match info.state {
                        VmState::Running => "running",
                        VmState::Paused => "paused",
                        VmState::Starting => "starting",
                        VmState::NotStarted => "stopped",
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

        Ok(SandboxReport {
            state,
            backend: "firecracker".into(),
            project: session.as_ref().map(|s| s.project.clone()),
            vcpus: self.config.vcpus,
            memory_mib: self.config.memory_mib,
            api_socket: paths.api_socket,
            virtiofs_socket: paths.virtiofs_socket,
            virtiofs_running,
            firecracker_pid: session.as_ref().map(|s| s.firecracker_pid),
            virtiofs_pid: session.as_ref().map(|s| s.virtiofs_pid),
            started_at: session.as_ref().map(|s| s.started_at),
            serial_log: session.as_ref().and_then(|s| s.serial_log.clone()),
            note,
        })
    }

    /// Pause, snapshot RAM+CPU into `snapshots_dir`, resume.
    pub async fn create_snapshot(&self, project: &Path, name: &str) -> Result<SnapshotMeta> {
        let paths = self.paths();
        ensure_socket(&paths.api_socket)?;
        let name = crate::snapshot::sanitize_name(name)?;
        let store = SnapshotStore::new(&self.config.snapshots_dir);
        let prepared = store.prepare(&name)?;
        let snapshot_path = prepared.snapshot_path;
        let mem_file_path = prepared.mem_file_path;
        let snap_dir = prepared.dir;
        let client = FirecrackerClient::new(&paths.api_socket);

        client.pause().await?;
        if let Err(err) = client
            .create_snapshot(&SnapshotCreate {
                snapshot_path: snapshot_path.display().to_string(),
                mem_file_path: mem_file_path.display().to_string(),
                snapshot_type: Some("Full".into()),
            })
            .await
        {
            // Always try to resume, even if snapshot creation failed.
            let _ = client.resume().await;
            let _ = fs::remove_dir_all(&snap_dir);
            return Err(err);
        }

        let size = fs::metadata(&snapshot_path).map(|m| m.len()).unwrap_or(0)
            + fs::metadata(&mem_file_path).map(|m| m.len()).unwrap_or(0);
        let meta = SnapshotMeta {
            name: name.to_string(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            snapshot_path,
            mem_file_path,
            project: Some(project.to_path_buf()),
            size_bytes: size,
            note: None,
        };
        if let Err(err) = store.save_meta(&meta) {
            let _ = client.resume().await;
            return Err(err);
        }
        client
            .resume()
            .await
            .context("snapshot saved, but the VM could not be resumed")?;
        Ok(meta)
    }

    /// Restore a snapshot into a brand new Firecracker process.
    ///
    /// Firecracker only accepts `/snapshot/load` before the microVM is booted,
    /// so the running process is replaced first. A fresh process also keeps a
    /// failed load (Firecracker terminates itself on failure) from taking down
    /// an existing VM. virtiofsd is restarted for the snapshot's project so the
    /// restored guest finds a healthy backend on the same socket path.
    pub async fn restore_snapshot(&self, snapshot: &SnapshotMeta) -> Result<()> {
        let paths = self.paths();
        paths.ensure()?;
        if !snapshot.snapshot_path.is_file() {
            bail!(
                "snapshot state file {} is missing",
                snapshot.snapshot_path.display()
            );
        }
        if !snapshot.mem_file_path.is_file() {
            bail!(
                "snapshot memory file {} is missing",
                snapshot.mem_file_path.display()
            );
        }
        let existing = self.session();
        let project = snapshot
            .project
            .clone()
            .or_else(|| existing.as_ref().map(|session| session.project.clone()))
            .context("snapshot has no project; cannot start the shared filesystem")?;
        if !project.is_dir() {
            bail!("snapshot project {} no longer exists", project.display());
        }
        // Preflight the binaries before stopping a running VM, so a typo in
        // the configuration does not cost the user their current sandbox.
        if !command_available(&self.config.firecracker_bin) {
            bail!(
                "firecracker binary {} was not found",
                self.config.firecracker_bin.display()
            );
        }
        if !command_available(&self.config.virtiofsd_bin) {
            bail!(
                "virtiofsd binary {} was not found",
                self.config.virtiofsd_bin.display()
            );
        }
        // Serialize with start/stop so another hegn process cannot recreate the
        // sockets and serial FIFO while the snapshot boot is in flight.
        let _lock = self.lock(&paths)?;

        // Stop the old VM and its filesystem daemon. Both sockets are freed
        // and recreated below with the same paths the snapshot expects.
        if let Some(session) = &existing {
            terminate(session.firecracker_pid, &self.config.firecracker_bin);
            terminate(session.virtiofs_pid, &self.config.virtiofsd_bin);
        }
        if let Some(pid) = read_pid(&paths.firecracker_pid) {
            terminate(pid, &self.config.firecracker_bin);
        }
        if let Some(pid) = read_pid(&paths.virtiofs_pid) {
            terminate(pid, &self.config.virtiofsd_bin);
        }

        let mut virtiofs = match spawn_virtiofsd(&self.config, &paths, &project) {
            Ok(process) => process,
            Err(err) => {
                cleanup_files(&paths);
                return Err(err);
            }
        };
        let virtiofs_pid = virtiofs.pid();
        if let Err(err) = fs::write(&paths.virtiofs_pid, virtiofs_pid.to_string()) {
            virtiofs.kill();
            cleanup_files(&paths);
            return Err(err).context("failed to record the virtiofsd pid");
        }

        let started_at = Utc::now();
        let logs = LogStore::new(self.config.logs_dir());
        if let Err(err) = logs.prune(DEFAULT_KEEP) {
            eprintln!("warning: could not prune old serial logs: {err:#}");
        }
        let serial_log = match logs.create_session(&project, started_at, std::process::id()) {
            Ok(path) => path,
            Err(err) => {
                virtiofs.kill();
                cleanup_files(&paths);
                return Err(err);
            }
        };

        let mut firecracker = match spawn_firecracker(&self.config, &paths, &serial_log) {
            Ok(child) => child,
            Err(err) => {
                virtiofs.kill();
                cleanup_files(&paths);
                let _ = fs::remove_file(&serial_log);
                return Err(err);
            }
        };
        let firecracker_pid = firecracker.id();

        if let Err(err) = load_snapshot_into(&paths, &mut firecracker, snapshot).await {
            let _ = firecracker.kill();
            let _ = firecracker.wait();
            virtiofs.kill();
            cleanup_files(&paths);
            let _ = fs::remove_file(&serial_log);
            return Err(err).context("failed to load the Firecracker snapshot");
        }

        let record = SessionRecord {
            project,
            harness: existing
                .as_ref()
                .map(|session| session.harness)
                .unwrap_or(self.config.harness),
            started_at,
            firecracker_pid,
            virtiofs_pid,
            boot_args: existing
                .as_ref()
                .map(|session| session.boot_args.clone())
                .unwrap_or_else(|| ROOT_BOOT_ARGS.to_string()),
            serial_log: Some(serial_log.clone()),
        };
        let recorded = (|| -> Result<()> {
            fs::write(&paths.firecracker_pid, firecracker_pid.to_string())?;
            fs::write(&paths.session, serde_json::to_string_pretty(&record)?)?;
            Ok(())
        })();
        if let Err(err) = recorded {
            let _ = firecracker.kill();
            let _ = firecracker.wait();
            virtiofs.kill();
            cleanup_files(&paths);
            let _ = fs::remove_file(&serial_log);
            return Err(err).context("failed to record the restored sandbox session");
        }

        // The restored VM runs for the rest of the hegn session.
        std::mem::forget(firecracker);
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
            FirecrackerClient::new(&paths.api_socket)
                .instance_info()
                .await
                .map(|info| matches!(info.state, VmState::Running | VmState::Paused))
                .unwrap_or(false)
        })
    }

    fn cleanup_stale(&self) {
        let paths = self.paths();
        if let Some(session) = self.session() {
            if process_matches(session.firecracker_pid, &self.config.firecracker_bin) {
                return;
            }
            terminate(session.firecracker_pid, &self.config.firecracker_bin);
            terminate(session.virtiofs_pid, &self.config.virtiofsd_bin);
        }
        cleanup_files(&paths);
    }
}

fn spawn_virtiofsd(
    config: &Config,
    paths: &SandboxPaths,
    project: &Path,
) -> Result<VirtiofsProcess> {
    let virtiofs = VirtiofsConfig::new(
        config.virtiofsd_bin.clone(),
        project,
        paths.virtiofs_socket.clone(),
    );
    let mut process = virtiofs.spawn().context("failed to start virtiofsd")?;
    if let Err(err) = process.wait_for_socket(Duration::from_secs(5)) {
        process.kill();
        return Err(err);
    }
    Ok(process)
}

fn spawn_firecracker(config: &Config, paths: &SandboxPaths, serial_log: &Path) -> Result<Child> {
    let _ = fs::remove_file(&paths.api_socket);
    ensure_serial_fifo(&paths.serial_in)?;
    prepare_serial_output(paths, serial_log)?;
    // Firecracker registers serial input only when stdin is a TTY or a FIFO.
    // O_RDWR keeps both ends open so the child never sees EOF between attaches.
    let serial_in = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&paths.serial_in)
        .with_context(|| format!("failed to open {}", paths.serial_in.display()))?;
    // Open the persistent log itself (not the hard link) with O_NOFOLLOW so the
    // guest serial output can never be redirected through a planted symlink.
    let serial_out = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(serial_log)
        .with_context(|| format!("failed to open serial log {}", serial_log.display()))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&paths.log)
        .with_context(|| format!("failed to open {}", paths.log.display()))?;
    let stderr = log.try_clone()?;
    Command::new(&config.firecracker_bin)
        .arg("--api-sock")
        .arg(&paths.api_socket)
        .arg("--id")
        .arg(format!("hegn-{}", std::process::id()))
        .stdin(Stdio::from(serial_in))
        .stdout(Stdio::from(serial_out))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn {} (is Firecracker installed?)",
                config.firecracker_bin.display()
            )
        })
}

/// Expose the persistent serial log at `<run>/serial.out` without copying it.
///
/// Firecracker writes once to the log file; console attach reads the same
/// inode through `serial.out`. Stopping the VM removes only the run-directory
/// link, so the session log survives.
fn prepare_serial_output(paths: &SandboxPaths, serial_log: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(serial_log)
        .with_context(|| format!("serial log {} is missing", serial_log.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to use symlinked serial log {}",
            serial_log.display()
        );
    }
    if !metadata.file_type().is_file() {
        bail!("serial log {} is not a regular file", serial_log.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(serial_log, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure {}", serial_log.display()))?;
    }
    let _ = fs::remove_file(&paths.serial_out);
    fs::hard_link(serial_log, &paths.serial_out).with_context(|| {
        format!(
            "failed to link serial log {} to {}",
            serial_log.display(),
            paths.serial_out.display()
        )
    })?;
    Ok(())
}

/// Create a private FIFO for guest serial input, replacing any stale one.
pub(crate) fn ensure_serial_fifo(path: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let _ = fs::remove_file(path);
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("invalid FIFO path {}", path.display()))?;
    // Safety: c_path is a valid NUL-terminated path; mkfifo does not retain it.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to create {}", path.display()));
    }
    Ok(())
}

async fn wait_for_api(socket: &Path, child: &mut Child, timeout: Duration) -> Result<()> {
    let client = FirecrackerClient::new(socket);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socket.exists() && client.instance_info().await.is_ok() {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!("Firecracker exited during startup ({status}); check the log");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    bail!(
        "timed out after {timeout:?} waiting for Firecracker API at {}",
        socket.display()
    )
}

async fn load_snapshot_into(
    paths: &SandboxPaths,
    child: &mut Child,
    snapshot: &SnapshotMeta,
) -> Result<()> {
    wait_for_api(&paths.api_socket, child, API_START_TIMEOUT).await?;
    FirecrackerClient::new(&paths.api_socket)
        .load_snapshot(&SnapshotLoad {
            snapshot_path: snapshot.snapshot_path.display().to_string(),
            mem_file_path: snapshot.mem_file_path.display().to_string(),
            track_dirty_pages: Some(true),
            resume_vm: Some(true),
        })
        .await
}

fn ensure_socket(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!(
            "no Firecracker API socket at {} — start a sandbox first",
            path.display()
        );
    }
    Ok(())
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Is `binary` either an existing path or a command found on `PATH`?
fn command_available(binary: &Path) -> bool {
    if binary.components().count() > 1 {
        return binary.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
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
    // Safety: the pid was verified to belong to the expected executable.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !process_matches(pid, binary) {
            // Exited, or the PID was recycled by an unrelated process.
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !process_matches(pid, binary) {
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
        &paths.virtiofs_socket,
        &paths.serial_in,
        &paths.serial_out,
        &paths.firecracker_pid,
        &paths.virtiofs_pid,
        &paths.session,
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
        config.kernel_image = dir.join("vmlinux.bin");
        config.rootfs_image = dir.join("rootfs.ext4");
        config.backend = BackendKind::Firecracker;
        config.virtiofsd_bin = PathBuf::from("/definitely/missing/virtiofsd");
        std::fs::write(&config.kernel_image, b"kernel").unwrap();
        std::fs::write(&config.rootfs_image, b"rootfs").unwrap();
        std::fs::create_dir_all(&config.projects_root).unwrap();
        config
    }

    #[test]
    fn paths_live_under_state_dir() {
        let config = Config::default();
        let paths = SandboxPaths::from_config(&config);
        assert_eq!(paths.run_dir, config.state_dir.join("run"));
        assert_eq!(paths.api_socket.file_name().unwrap(), "firecracker.sock");
        assert_eq!(paths.virtiofs_socket.file_name().unwrap(), "virtiofs.sock");
    }

    #[test]
    fn run_directory_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager::new(config_with_assets(dir.path()));
        manager.paths().ensure().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&manager.paths().run_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn start_requires_assets() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.kernel_image = dir.path().join("missing");
        let manager = SandboxManager::new(config);
        let err = manager
            .start(dir.path(), Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing VM assets"), "{err}");
    }

    #[test]
    fn start_preflights_binaries_before_spawning_anything() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.firecracker_bin = PathBuf::from("/definitely/missing/firecracker");
        let manager = SandboxManager::new(config);
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();

        let err = manager
            .start(&project, Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(err.contains("firecracker binary"), "{err}");
        assert!(!manager.paths().virtiofs_pid.exists());
        assert!(!manager.paths().api_socket.exists());
    }

    #[test]
    fn lifecycle_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager::new(config_with_assets(dir.path()));
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

    fn write_session(manager: &SandboxManager, project: &Path, pid: u32) {
        manager.paths().ensure().unwrap();
        let record = SessionRecord {
            project: project.to_path_buf(),
            harness: Harness::Shell,
            started_at: Utc::now(),
            firecracker_pid: pid,
            virtiofs_pid: pid,
            boot_args: ROOT_BOOT_ARGS.into(),
            serial_log: None,
        };
        std::fs::write(
            manager.paths().session,
            serde_json::to_string(&record).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn ensure_console_ignores_a_stale_session_for_another_project() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.firecracker_bin = PathBuf::from("/definitely/missing/firecracker");
        let manager = SandboxManager::new(config);
        let other = dir.path().join("projects/other");
        std::fs::create_dir_all(&other).unwrap();
        // Pid 4_000_000_000 cannot exist, so this session is stale.
        write_session(&manager, &dir.path().join("projects/gone"), 4_000_000_000);

        let err = manager
            .ensure_console(&other, Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("already running"),
            "a stale session must not block another project: {err}"
        );
        assert!(err.contains("firecracker binary"), "{err}");
    }

    #[test]
    fn ensure_console_rejects_a_running_session_for_another_project() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.firecracker_bin = std::env::current_exe().unwrap();
        let manager = SandboxManager::new(config);
        let running = dir.path().join("projects/running");
        let other = dir.path().join("projects/other");
        std::fs::create_dir_all(&running).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        write_session(&manager, &running, std::process::id());

        let err = manager
            .ensure_console(&other, Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already running"), "{err}");
    }

    #[test]
    fn ensure_console_matches_a_symlinked_project_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.firecracker_bin = std::env::current_exe().unwrap();
        let manager = SandboxManager::new(config);
        let real = dir.path().join("projects/real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("projects/link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_session(&manager, &real, std::process::id());

        // Same VM, same project through a symlink: the conflict check passes
        // and only the missing serial files are reported.
        let err = manager
            .ensure_console(&link, Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(err.contains("serial console files"), "{err}");
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
        let manager = SandboxManager::new(config_with_assets(dir.path()));
        let err = manager
            .start(&dir.path().join("missing-project"), Harness::Shell)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn start_reports_missing_virtiofsd() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        // Make the Firecracker preflight pass so this test reaches virtiofsd.
        config.firecracker_bin = std::env::current_exe().unwrap();
        let manager = SandboxManager::new(config);
        let project = dir.path().join("projects/demo");
        std::fs::create_dir_all(&project).unwrap();
        let err = manager
            .start(&project, Harness::Opencode)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("virtiofsd") || err.contains("failed to start"),
            "{err}"
        );
    }

    #[test]
    fn stop_without_session_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager::new(config_with_assets(dir.path()));
        assert!(manager.stop().is_err());
        assert!(!manager.is_running());
    }

    #[test]
    fn status_without_session_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager::new(config_with_assets(dir.path()));
        let report = manager.status().unwrap();
        assert_eq!(report.state, "stopped");
        assert!(!report.virtiofs_running);
        assert!(report.project.is_none());
    }

    #[test]
    fn session_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = SandboxManager::new(config);
        manager.paths().ensure().unwrap();
        let record = SessionRecord {
            project: dir.path().join("p"),
            harness: Harness::Codex,
            started_at: Utc::now(),
            firecracker_pid: 1234,
            virtiofs_pid: 4321,
            boot_args: ROOT_BOOT_ARGS.into(),
            serial_log: None,
        };
        std::fs::write(
            manager.paths().session,
            serde_json::to_string_pretty(&record).unwrap(),
        )
        .unwrap();
        let loaded = manager.session().unwrap();
        assert_eq!(loaded.harness, Harness::Codex);
        assert_eq!(loaded.firecracker_pid, 1234);
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
        let manager = SandboxManager::new(config_with_assets(dir.path()));
        let snapshot = SnapshotMeta {
            name: "missing".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            snapshot_path: dir.path().join("missing.snap"),
            mem_file_path: dir.path().join("missing.mem"),
            project: Some(dir.path().join("projects")),
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
    fn restore_requires_a_project_without_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager::new(config_with_assets(dir.path()));
        let snap = dir.path().join("vm.snap");
        let mem = dir.path().join("vm.mem");
        std::fs::write(&snap, b"state").unwrap();
        std::fs::write(&mem, b"memory").unwrap();
        let snapshot = SnapshotMeta {
            name: "orphan".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            snapshot_path: snap,
            mem_file_path: mem,
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
        assert!(err.contains("no project"), "{err}");
    }

    #[test]
    fn restore_preflights_binaries_before_stopping_a_vm() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_assets(dir.path());
        config.firecracker_bin = PathBuf::from("/definitely/missing/firecracker");
        config.virtiofsd_bin = PathBuf::from("/definitely/missing/virtiofsd");
        let manager = SandboxManager::new(config);
        let snap = dir.path().join("vm.snap");
        let mem = dir.path().join("vm.mem");
        std::fs::write(&snap, b"state").unwrap();
        std::fs::write(&mem, b"memory").unwrap();
        let snapshot = SnapshotMeta {
            name: "demo".into(),
            created_at: Utc::now(),
            vm_state: "Paused".into(),
            snapshot_path: snap,
            mem_file_path: mem,
            project: Some(dir.path().join("projects")),
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
        assert!(err.contains("firecracker binary"), "{err}");
    }

    #[test]
    fn serial_output_is_hard_linked_to_the_private_log() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager::new(config_with_assets(dir.path()));
        let paths = manager.paths();
        paths.ensure().unwrap();
        let log = dir.path().join("session-test.log");
        std::fs::write(&log, b"boot\n").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();

        prepare_serial_output(&paths, &log).unwrap();
        assert_eq!(
            std::fs::metadata(&log).unwrap().ino(),
            std::fs::metadata(&paths.serial_out).unwrap().ino(),
            "serial.out must expose the same inode as the persistent log"
        );
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&paths.serial_out, b"guest output\n").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"guest output\n");
    }

    #[test]
    fn serial_output_refuses_a_symlinked_log() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SandboxManager::new(config_with_assets(dir.path()));
        let paths = manager.paths();
        paths.ensure().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"secret").unwrap();
        let link = dir.path().join("session-link.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = prepare_serial_output(&paths, &link)
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlinked serial log"), "{err}");
        assert!(!paths.serial_out.exists());
    }

    #[test]
    fn old_session_records_without_a_log_still_parse() {
        let raw = r#"{
            "project": "/projects/demo",
            "harness": "shell",
            "started_at": "2026-01-01T00:00:00Z",
            "firecracker_pid": 1,
            "virtiofs_pid": 2,
            "boot_args": ""
        }"#;
        let record: SessionRecord = serde_json::from_str(raw).unwrap();
        assert!(record.serial_log.is_none());
        assert_eq!(record.project, PathBuf::from("/projects/demo"));
    }

    #[test]
    fn report_serializes() {
        let dir = tempfile::tempdir().unwrap();
        let report = SandboxManager::new(config_with_assets(dir.path()))
            .status()
            .unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["state"], "stopped");
        assert_eq!(json["backend"], "firecracker");
    }
}
