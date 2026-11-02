//! A console attached to one running Cloud Hypervisor VM.

use std::io::{Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::session::relay;
use crate::vm::instance::{InstanceManager, InstancePaths, process_matches};
use crate::vm::instance_config::InstanceConfig;

use super::transport::TerminalConsole;

/// A console attached to a running Cloud Hypervisor VM.
#[derive(Debug)]
pub struct VmConsole {
    stream: UnixStream,
    cloud_hypervisor_bin: PathBuf,
    vm_pid: u32,
    relay_pid: u32,
    instance: String,
    project: PathBuf,
    harness: String,
    label: String,
    serial_log: Option<PathBuf>,
    exited: bool,
}

impl VmConsole {
    /// Attach to the VM described by the current session record.
    pub fn attach(config: &Config, instance: &InstanceConfig) -> Result<Self> {
        let paths = InstancePaths::from_instance(config, instance);
        let manager = InstanceManager::new(config.clone(), instance.clone());
        let session = match manager.session() {
            Some(session) => session,
            None if paths.session.exists() => bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 stop the old VM and remove the run directory",
                paths.run_dir.display()
            ),
            None => bail!(
                "no VM session in {}; start one with `hegn instance start <project>`",
                paths.run_dir.display()
            ),
        };
        let vm_binary = session
            .vm_binary
            .clone()
            .unwrap_or_else(|| config.vm_binary());
        if !process_matches(session.vm_pid, &vm_binary) {
            bail!(
                "the VM recorded in {} is not running; start a VM before attaching",
                paths.session.display()
            );
        }
        let hegn = std::env::current_exe().context("failed to locate the hegn executable")?;
        if session.relay_pid != 0 && !relay::process_matches_relay(session.relay_pid, &hegn) {
            bail!("the console relay is not running; restart the VM with `hegn instance start`");
        }
        let socket_ok = std::fs::symlink_metadata(&paths.console_socket)
            .map(|meta| meta.file_type().is_socket())
            .unwrap_or(false);
        if !socket_ok {
            bail!("the console relay socket is missing; restart the VM with `hegn instance start`");
        }
        let stream = UnixStream::connect(&paths.console_socket).with_context(|| {
            format!(
                "failed to connect to the console relay at {}",
                paths.console_socket.display()
            )
        })?;
        stream
            .set_nonblocking(true)
            .context("failed to configure the console relay socket")?;
        let serial_log = session
            .serial_log
            .clone()
            .filter(|path| path.is_file() && !path.is_symlink());

        Ok(Self {
            stream,
            cloud_hypervisor_bin: vm_binary,
            vm_pid: session.vm_pid,
            relay_pid: session.relay_pid,
            instance: instance.name.clone(),
            // Clones live in their own workspace; older records only have the
            // original project.
            project: if session.workspace.as_os_str().is_empty() {
                session.project.clone()
            } else {
                session.workspace.clone()
            },
            harness: session.harness.label().to_string(),
            label: format!("{} · {} · serial", instance.name, session.harness.label()),
            serial_log,
            exited: false,
        })
    }

    /// Project name shown in the attach status bar.
    pub fn project_name(&self) -> String {
        self.project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.project.display().to_string())
    }

    /// Persistent serial log for this session, when one was recorded.
    pub fn serial_log(&self) -> Option<&Path> {
        self.serial_log.as_deref()
    }

    /// Harness label recorded for this session.
    pub fn harness(&self) -> &str {
        &self.harness
    }

    /// VM process id (verified when the console was attached).
    pub fn pid(&self) -> u32 {
        self.vm_pid
    }

    /// Instance name shown in the attach bar and TUI.
    pub fn instance_name(&self) -> &str {
        &self.instance
    }

    fn read_new(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        loop {
            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.exited = true;
                    break;
                }
                Ok(n) => chunks.push(buf[..n].to_vec()),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                // An unreadable console is unusable: stop the console rather
                // than silently dropping guest output for the rest of the
                // session.
                Err(_) => {
                    self.exited = true;
                    break;
                }
            }
        }
        chunks
    }
}

impl TerminalConsole for VmConsole {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let mut remaining = bytes;
        let deadline = Instant::now() + Duration::from_millis(500);
        while !remaining.is_empty() {
            match self.stream.write(remaining) {
                Ok(0) => {
                    if Instant::now() > deadline {
                        bail!("console relay input buffer is full");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(n) => remaining = &remaining[n..],
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        bail!("console relay input buffer is full");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => return Err(err).context("failed to write to the guest serial port"),
            }
        }
        Ok(())
    }

    fn drain(&mut self) -> Vec<Vec<u8>> {
        self.read_new()
    }

    fn is_exited(&mut self) -> bool {
        if self.exited {
            return true;
        }
        if !process_matches(self.vm_pid, &self.cloud_hypervisor_bin) {
            self.exited = true;
        } else if self.relay_pid != 0 {
            let hegn = std::env::current_exe();
            if hegn
                .map(|binary| !relay::process_matches_relay(self.relay_pid, &binary))
                .unwrap_or(true)
            {
                self.exited = true;
            }
        }
        self.exited
    }

    fn project(&self) -> &Path {
        &self.project
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn instance_name(&self) -> &str {
        &self.instance
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::session::console::TerminalConsole;
    use crate::vm::instance::{InstancePaths, SessionRecord};
    use chrono::Utc;
    use std::os::unix::net::UnixListener;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.state_dir = dir.join("state");
        config.instances_dir = dir.join("instances");
        config.backend = BackendKind::CloudHypervisor;
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        config
    }

    fn instance(dir: &Path) -> InstanceConfig {
        InstanceConfig {
            name: "demo".into(),
            project: dir.to_path_buf(),
            workspace: dir.to_path_buf(),
            harness: crate::config::Harness::Opencode,
            vcpus: 1,
            memory_mib: 128,
            ..InstanceConfig::default()
        }
    }

    fn write_session(
        config: &Config,
        instance: &InstanceConfig,
        dir: &Path,
        vm_pid: u32,
        relay_pid: u32,
        log: Option<PathBuf>,
    ) {
        write_session_with_binary(config, instance, dir, vm_pid, relay_pid, log, None);
    }

    fn write_session_with_binary(
        config: &Config,
        instance: &InstanceConfig,
        dir: &Path,
        vm_pid: u32,
        relay_pid: u32,
        log: Option<PathBuf>,
        vm_binary: Option<PathBuf>,
    ) {
        let paths = InstancePaths::from_instance(config, instance);
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: dir.to_path_buf(),
            workspace: dir.to_path_buf(),
            instance: instance.name.clone(),
            harness: crate::config::Harness::Opencode,
            started_at: Utc::now(),
            vm_pid,
            vm_binary,
            relay_pid,
            virtiofs_pid: std::process::id(),
            profile_virtiofs_pid: 0,
            boot_args: String::new(),
            serial_log: log,
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
    }

    #[test]
    fn attach_requires_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let err = VmConsole::attach(&config, &instance)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no VM session"), "{err}");
    }

    #[test]
    fn attach_reads_and_writes_the_relay_socket() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        paths.ensure().unwrap();
        let listener = UnixListener::bind(&paths.console_socket).unwrap();
        let serial_log = dir.path().join("session-test.log");
        std::fs::write(&serial_log, b"boot complete\n").unwrap();
        // relay_pid = 0 skips the process check; the socket is the real test.
        write_session(
            &config,
            &instance,
            dir.path(),
            std::process::id(),
            0,
            Some(serial_log.clone()),
        );

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"boot complete\n").unwrap();
            let mut input = [0u8; 16];
            let read = stream.read(&mut input).unwrap();
            assert_eq!(&input[..read], b"ls\r");
        });

        let mut console = VmConsole::attach(&config, &instance).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = Vec::new();
        while output != b"boot complete\n" && Instant::now() < deadline {
            output.extend(console.drain().into_iter().flatten());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(output, b"boot complete\n");
        assert_eq!(console.project(), dir.path());
        assert!(console.label().contains("OpenCode"));
        assert_eq!(console.harness(), "OpenCode");
        assert_eq!(console.serial_log(), Some(serial_log.as_path()));
        assert_eq!(console.pid(), std::process::id());

        console.send(b"ls\r").unwrap();
        server.join().unwrap();
    }

    #[test]
    fn attach_requires_the_console_relay() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        write_session(&config, &instance, dir.path(), std::process::id(), 0, None);
        let err = VmConsole::attach(&config, &instance)
            .unwrap_err()
            .to_string();
        assert!(err.contains("console relay socket is missing"), "{err}");
    }

    #[test]
    fn attach_rejects_a_dead_vm() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        paths.ensure().unwrap();
        let _listener = UnixListener::bind(&paths.console_socket).unwrap();
        // Out of pid_t range: cannot be running, even after pid reuse.
        write_session(&config, &instance, dir.path(), 4_000_000_000, 0, None);

        let err = VmConsole::attach(&config, &instance)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running"), "{err}");
    }

    #[test]
    fn attach_refuses_unreadable_session_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        paths.ensure().unwrap();
        std::fs::write(&paths.session, b"{not json").unwrap();

        let err = VmConsole::attach(&config, &instance)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unreadable"), "{err}");
        assert!(err.contains("stop the old VM"), "{err}");
    }

    #[test]
    fn attach_rejects_a_vm_recorded_with_another_binary() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        paths.ensure().unwrap();
        let _listener = UnixListener::bind(&paths.console_socket).unwrap();
        // The pid is alive, but it is not the recorded binary.
        write_session_with_binary(
            &config,
            &instance,
            dir.path(),
            std::process::id(),
            0,
            None,
            Some(PathBuf::from("/opt/not-cloud-hypervisor")),
        );

        let err = VmConsole::attach(&config, &instance)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running"), "{err}");
        assert!(err.contains("start a VM before attaching"), "{err}");
    }

    #[test]
    fn attach_rejects_a_recycled_relay_pid() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        paths.ensure().unwrap();
        let _listener = UnixListener::bind(&paths.console_socket).unwrap();
        // The VM pid is this test process (the configured binary); the relay
        // pid is also alive but is not the internal relay.
        write_session(
            &config,
            &instance,
            dir.path(),
            std::process::id(),
            std::process::id(),
            None,
        );

        let err = VmConsole::attach(&config, &instance)
            .unwrap_err()
            .to_string();
        assert!(err.contains("console relay is not running"), "{err}");
    }

    #[test]
    fn attach_rejects_a_console_path_that_is_not_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        paths.ensure().unwrap();
        std::fs::write(&paths.console_socket, b"not a socket").unwrap();
        write_session(&config, &instance, dir.path(), std::process::id(), 0, None);

        let err = VmConsole::attach(&config, &instance)
            .unwrap_err()
            .to_string();
        assert!(err.contains("socket is missing"), "{err}");
    }

    #[test]
    fn attach_ignores_a_symlinked_serial_log() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        paths.ensure().unwrap();
        let _listener = UnixListener::bind(&paths.console_socket).unwrap();
        let real = dir.path().join("real.log");
        std::fs::write(&real, b"secret").unwrap();
        let link = dir.path().join("session-link.log");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_session(
            &config,
            &instance,
            dir.path(),
            std::process::id(),
            0,
            Some(link),
        );

        let console = VmConsole::attach(&config, &instance).unwrap();
        assert_eq!(console.serial_log(), None);
    }

    #[test]
    fn console_reports_exit_when_the_vm_process_disappears() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let instance = instance(dir.path());
        let paths = InstancePaths::from_instance(&config, &instance);
        paths.ensure().unwrap();
        let _listener = UnixListener::bind(&paths.console_socket).unwrap();
        write_session(&config, &instance, dir.path(), std::process::id(), 0, None);

        let mut console = VmConsole::attach(&config, &instance).unwrap();
        assert!(!console.is_exited());
        console.vm_pid = 4_000_000_000;
        assert!(console.is_exited());
    }
}
