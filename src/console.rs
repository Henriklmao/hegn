//! Cloud Hypervisor serial-console transport.
//!
//! Cloud Hypervisor exposes the guest serial port on a Unix socket. hegn's
//! detached relay is that socket's only client: it keeps the persistent log
//! growing and re-exposes the console on `run/console.sock` for the TUI and
//! `hegn attach`.
//!
//! There is deliberately no local-shell fallback: a console is only available
//! while a Cloud Hypervisor VM is actually running.

use std::io::{Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::attach_bar::{OutputGate, StatusBar, detach_requested};
use crate::config::Config;
use crate::instance::{InstanceManager, InstancePaths, process_matches};
use crate::instance_config::InstanceConfig;
use crate::logs::sanitize_label;
use crate::relay;

/// Transport used by the terminal view. Implementations must never pretend to
/// be an instance when no VM is running.
pub trait TerminalConsole: Send {
    /// Send bytes to the guest serial port.
    fn send(&mut self, bytes: &[u8]) -> Result<()>;
    /// Drain newly available serial output.
    fn drain(&mut self) -> Vec<Vec<u8>>;
    /// Whether the backing VM has exited.
    fn is_exited(&mut self) -> bool;
    fn project(&self) -> &Path;
    fn label(&self) -> &str;
    fn instance_name(&self) -> &str;
}

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

/// The smallest terminal hegn will attach to. The bottom line is reserved for
/// the status bar, and the guest needs at least a few lines above it.
const MIN_COLS: u16 = 32;
const MIN_ROWS: u16 = 4;

/// Attaches the process terminal to the console and restores it on drop,
/// including when the console loop unwinds with a panic.
struct TerminalGuard {
    cols: u16,
    rows: u16,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        use crossterm::cursor::Show;
        use crossterm::execute;
        use crossterm::terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        };

        let (cols, rows) =
            crossterm::terminal::size().context("failed to read the terminal size")?;
        if cols < MIN_COLS || rows < MIN_ROWS {
            bail!(
                "terminal is too small to attach ({cols}x{rows}); need at least {MIN_COLS}x{MIN_ROWS}"
            );
        }

        enable_raw_mode().context("failed to enable raw mode")?;
        let mut out = std::io::stdout();
        if let Err(err) = execute!(out, EnterAlternateScreen) {
            disable_raw_mode().ok();
            return Err(err).context("failed to enter the serial console");
        }
        // Clear the alternate screen for both bar modes and start from a known
        // margin/origin state.
        if let Err(err) = write!(out, "\x1b[2J\x1b[?6l\x1b[r\x1b[H").and_then(|()| out.flush()) {
            disable_raw_mode().ok();
            let _ = execute!(out, LeaveAlternateScreen, Show);
            return Err(err).context("failed to initialize the serial console");
        }
        Ok(Self { cols, rows })
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    fn update_size(&mut self, cols: u16, rows: u16) -> Result<()> {
        if cols < MIN_COLS || rows < MIN_ROWS {
            bail!(
                "terminal is too small to attach ({cols}x{rows}); need at least {MIN_COLS}x{MIN_ROWS}"
            );
        }
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use crossterm::cursor::Show;
        use crossterm::execute;
        use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

        disable_raw_mode().ok();
        let mut out = std::io::stdout();
        // Reset margins and origin mode on both screens; a stale region or
        // origin mode would break the user's shell after hegn exits.
        let _ = write!(out, "\x1b[?6l\x1b[r\x1b[0m");
        let _ = out.flush();
        let _ = execute!(out, LeaveAlternateScreen, Show);
        let _ = write!(out, "\x1b[?6l\x1b[r\x1b[0m");
        let _ = out.flush();
    }
}

/// Attach an interactive terminal to a VM serial console.
///
/// This is the CLI equivalent of the TUI terminal view. Input is passed
/// straight to the guest UART (the status bar marks this as `INPUT DIRECT`); use
/// `hegn check` for pre-execution guardrails or the TUI for line-buffered
/// interception. `Ctrl-B` (or `Ctrl-]`) detaches without stopping the VM.
///
/// When `bar` is set, the bottom line shows the project, harness, VM pid and
/// persistent serial log while the guest uses the lines above it. The guest has
/// no window-size channel, so full-screen guest programs keep their own
/// geometry; pass `bar = false` to give them the whole host terminal.
pub fn run_interactive(console: VmConsole, bar: bool) -> Result<()> {
    run_interactive_with_command(console, bar, None)
}

/// Like [`run_interactive`], but types `command` into the guest after the
/// console is live. Used by the launch integrations so `tmux`/`herdr` panes can
/// open the agent instead of a bare shell.
pub fn run_interactive_with_command(
    mut console: VmConsole,
    bar: bool,
    initial_command: Option<&str>,
) -> Result<()> {
    use crossterm::event::{self, Event, KeyEventKind};

    let mut guard = TerminalGuard::enter()?;
    let project = console.project_name();
    let harness = console.harness().to_string();
    let pid = console.pid();
    let log = console.serial_log().map(Path::to_path_buf);
    let instance = console.instance_name().to_string();
    let (cols, rows) = guard.size();
    let mut status = bar.then(|| {
        StatusBar::new(
            project.clone(),
            harness.clone(),
            pid,
            log.clone(),
            cols,
            rows,
        )
    });
    let mut gate = OutputGate::new();
    let mut pending_resize: Option<(u16, u16)> = None;
    let mut vm_exited = false;

    let result = (|| -> Result<()> {
        let mut out = std::io::stdout();
        if let Some(status) = &status {
            status.reserve_region(&mut out)?;
            status.paint(&mut out)?;
        }
        if let Some(command) = initial_command {
            let command = command.trim_end_matches(['\r', '\n']);
            if !command.is_empty() {
                console.send(command.as_bytes())?;
                console.send(b"\n")?;
            }
        }

        loop {
            let chunks = console.drain();
            let mut dirty = !chunks.is_empty();
            for chunk in chunks {
                gate.feed(&chunk, status.as_ref().map(StatusBar::rows), &mut out)?;
            }
            out.flush()?;
            if console.is_exited() {
                vm_exited = true;
                break;
            }

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if detach_requested(key) {
                            break;
                        }
                        if let Some(bytes) = crate::app::encode_key(key) {
                            console.send(&bytes)?;
                        }
                    }
                    Event::Resize(cols, rows) => {
                        guard.update_size(cols, rows)?;
                        if let Some(status) = status.as_mut() {
                            if gate.safe_to_inject() {
                                status.resize(cols, rows, &mut out)?;
                                dirty = true;
                            } else {
                                // Do not split a half-received escape sequence.
                                // Apply the resize after the next clean batch.
                                pending_resize = Some((cols, rows));
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Repaint after every output burst. A full-screen guest can move
            // the cursor onto the reserved row, and a reset can drop the
            // scroll region; only an immediate repaint keeps the bar pinned to
            // the bottom. Skip it while a split escape sequence or multi-byte
            // character is in flight: injecting there would corrupt the guest
            // stream. The next batch that ends cleanly repaints.
            if let Some(status) = status.as_mut()
                && gate.safe_to_inject()
            {
                if let Some((cols, rows)) = pending_resize.take() {
                    status.resize(cols, rows, &mut out)?;
                    dirty = true;
                }
                if dirty {
                    status.paint(&mut out)?;
                }
            }
        }
        Ok(())
    })();

    // Terminate a partial escape sequence even when the loop unwound with an
    // error, so the terminal reset on drop is not swallowed by an open string.
    let _ = gate.finish(&mut std::io::stdout());
    drop(guard);
    result?;
    let project = sanitize_label(&project);
    let instance = sanitize_label(&instance);
    if vm_exited {
        println!("Serial console closed: the VM for {project} exited.");
    } else {
        println!(
            "Detached from {project}. The VM keeps running; re-attach with `hegn attach {instance}`."
        );
    }
    if let Some(log) = &log {
        println!("Serial log: {}", sanitize_label(&log.display().to_string()));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::instance::{InstancePaths, SessionRecord};
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
        let paths = InstancePaths::from_instance(config, instance);
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: dir.to_path_buf(),
            workspace: dir.to_path_buf(),
            instance: instance.name.clone(),
            harness: crate::config::Harness::Opencode,
            started_at: Utc::now(),
            vm_pid,
            vm_binary: None,
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
}
