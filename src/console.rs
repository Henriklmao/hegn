//! Firecracker serial-console transport.
//!
//! Firecracker exposes the guest UART on its stdin/stdout. hegn starts the VMM
//! with a private FIFO for serial input and a file for serial output, so the
//! TUI and CLI can detach and re-attach to a running VM without restarting it.
//!
//! There is deliberately no local-shell fallback: a console is only available
//! while a Firecracker VM is actually running.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::sandbox::{SandboxManager, SandboxPaths, process_matches};

/// Transport used by the terminal view. Implementations must never pretend to
/// be a sandbox when no VM is running.
pub trait TerminalConsole: Send {
    /// Send bytes to the guest serial port.
    fn send(&mut self, bytes: &[u8]) -> Result<()>;
    /// Drain newly available serial output.
    fn drain(&mut self) -> Vec<Vec<u8>>;
    /// Whether the backing VM has exited.
    fn is_exited(&mut self) -> bool;
    fn project(&self) -> &Path;
    fn label(&self) -> &str;
}

/// A console attached to a running Firecracker VM.
#[derive(Debug)]
pub struct VmConsole {
    serial_in: File,
    serial_out: File,
    firecracker_bin: PathBuf,
    pid: u32,
    project: PathBuf,
    label: String,
    exited: bool,
}

impl VmConsole {
    /// Attach to the VM described by the current session record.
    pub fn attach(config: &Config) -> Result<Self> {
        let paths = SandboxPaths::from_config(config);
        let manager = SandboxManager::new(config.clone());
        let session = manager.session().with_context(|| {
            format!(
                "no VM session in {}; start one with `hegn sandbox start <project>`",
                paths.run_dir.display()
            )
        })?;
        if !process_matches(session.firecracker_pid, &config.firecracker_bin) {
            bail!(
                "the VM recorded in {} is not running; start a VM before attaching",
                paths.session.display()
            );
        }
        if !paths.serial_in.exists() || !paths.serial_out.exists() {
            bail!("serial console files are missing; restart the VM with `hegn sandbox start`");
        }

        let serial_in = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&paths.serial_in)
            .with_context(|| format!("failed to open {}", paths.serial_in.display()))?;
        let serial_out = File::open(&paths.serial_out)
            .with_context(|| format!("failed to open {}", paths.serial_out.display()))?;

        Ok(Self {
            serial_in,
            serial_out,
            firecracker_bin: config.firecracker_bin.clone(),
            pid: session.firecracker_pid,
            project: session.project.clone(),
            label: format!("{} · serial", session.harness.label()),
            exited: false,
        })
    }

    fn read_new(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        loop {
            let mut buf = [0u8; 8192];
            match self.serial_out.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => chunks.push(buf[..n].to_vec()),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        chunks
    }
}

impl TerminalConsole for VmConsole {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let mut remaining = bytes;
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        while !remaining.is_empty() {
            match self.serial_in.write(remaining) {
                Ok(0) => {
                    if std::time::Instant::now() > deadline {
                        bail!("guest serial input buffer is full");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(n) => remaining = &remaining[n..],
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        bail!("guest serial input buffer is full");
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
        if !process_matches(self.pid, &self.firecracker_bin) {
            self.exited = true;
        }
        self.exited
    }

    fn project(&self) -> &Path {
        &self.project
    }

    fn label(&self) -> &str {
        &self.label
    }
}

/// Attach an interactive terminal to a VM serial console.
///
/// This is the CLI equivalent of the TUI terminal view. Input is passed
/// straight to the guest UART; use `hegn check` for pre-execution guardrails or
/// the TUI for line-buffered interception. `Ctrl-B` detaches without stopping
/// the VM.
pub fn run_interactive(mut console: VmConsole) -> Result<()> {
    use crossterm::event::{self, Event, KeyEventKind};
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use crossterm::{cursor::Show, execute};

    let mut out = std::io::stdout();
    enable_raw_mode().context("failed to enable raw mode")?;
    if let Err(err) = execute!(out, EnterAlternateScreen) {
        disable_raw_mode().ok();
        return Err(err).context("failed to enter the serial console");
    }

    let result = (|| -> Result<()> {
        loop {
            for chunk in console.drain() {
                out.write_all(&chunk)?;
            }
            out.flush()?;
            if console.is_exited() {
                break;
            }
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
            {
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
                    && key.code == crossterm::event::KeyCode::Char('b')
                {
                    break;
                }
                if let Some(bytes) = crate::app::encode_key(key) {
                    console.send(&bytes)?;
                }
            }
        }
        Ok(())
    })();

    disable_raw_mode().ok();
    execute!(out, LeaveAlternateScreen, Show).ok();
    result
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::sandbox::SandboxPaths;
    use chrono::Utc;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.state_dir = dir.join("state");
        config.backend = BackendKind::Firecracker;
        config.firecracker_bin = std::env::current_exe().unwrap();
        config
    }

    #[test]
    fn attach_requires_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let err = VmConsole::attach(&config(dir.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no VM session"), "{err}");
    }

    #[test]
    fn attach_reads_and_writes_the_serial_port() {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let paths = SandboxPaths::from_config(&config);
        paths.ensure().unwrap();
        crate::sandbox::ensure_serial_fifo(&paths.serial_in).unwrap();
        // Keep a reader open so writes to the FIFO do not block.
        let mut fifo_reader = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&paths.serial_in)
            .unwrap();
        std::fs::write(&paths.serial_out, b"boot complete\n").unwrap();

        let record = crate::sandbox::SessionRecord {
            project: dir.path().to_path_buf(),
            harness: crate::config::Harness::Opencode,
            started_at: Utc::now(),
            firecracker_pid: std::process::id(),
            virtiofs_pid: std::process::id(),
            boot_args: String::new(),
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();

        let mut console = VmConsole::attach(&config).unwrap();
        let output: Vec<u8> = console.drain().into_iter().flatten().collect();
        assert_eq!(output, b"boot complete\n");
        assert_eq!(console.project(), dir.path());
        assert!(console.label().contains("OpenCode"));

        console.send(b"ls\r").unwrap();
        let mut received = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while received != b"ls\r" && std::time::Instant::now() < deadline {
            let mut buf = [0u8; 16];
            match fifo_reader.read(&mut buf) {
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("fifo read failed: {err}"),
            }
        }
        assert_eq!(received, b"ls\r");
    }

    #[test]
    fn attach_requires_a_running_vm_and_serial_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let paths = SandboxPaths::from_config(&config);
        paths.ensure().unwrap();
        let record = crate::sandbox::SessionRecord {
            project: dir.path().to_path_buf(),
            harness: crate::config::Harness::Shell,
            started_at: Utc::now(),
            firecracker_pid: std::process::id(),
            virtiofs_pid: std::process::id(),
            boot_args: String::new(),
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
        let err = VmConsole::attach(&config).unwrap_err().to_string();
        assert!(err.contains("serial console files are missing"), "{err}");
    }
}
