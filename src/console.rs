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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::Config;
use crate::logs::sanitize_label;
use crate::relay;
use crate::sandbox::{SandboxManager, SandboxPaths, process_matches};
use crate::sandbox_config::SandboxConfig;

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
    fn sandbox_name(&self) -> &str;
}

/// A console attached to a running Cloud Hypervisor VM.
#[derive(Debug)]
pub struct VmConsole {
    stream: UnixStream,
    cloud_hypervisor_bin: PathBuf,
    vm_pid: u32,
    relay_pid: u32,
    sandbox: String,
    project: PathBuf,
    harness: String,
    label: String,
    serial_log: Option<PathBuf>,
    exited: bool,
}

impl VmConsole {
    /// Attach to the VM described by the current session record.
    pub fn attach(config: &Config, sandbox: &SandboxConfig) -> Result<Self> {
        let paths = SandboxPaths::from_sandbox(config, sandbox);
        let manager = SandboxManager::new(config.clone(), sandbox.clone());
        let session = match manager.session() {
            Some(session) => session,
            None if paths.session.exists() => bail!(
                "session metadata in {} is unreadable or from an older hegn version; \
                 stop the old VM and remove the run directory",
                paths.run_dir.display()
            ),
            None => bail!(
                "no VM session in {}; start one with `hegn sandbox start <project>`",
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
            bail!("the console relay is not running; restart the VM with `hegn sandbox start`");
        }
        let socket_ok = std::fs::symlink_metadata(&paths.console_socket)
            .map(|meta| meta.file_type().is_socket())
            .unwrap_or(false);
        if !socket_ok {
            bail!("the console relay socket is missing; restart the VM with `hegn sandbox start`");
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
            sandbox: sandbox.name.clone(),
            project: session.project.clone(),
            harness: session.harness.label().to_string(),
            label: format!("{} · {} · serial", sandbox.name, session.harness.label()),
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

    /// Sandbox name shown in the attach bar and TUI.
    pub fn sandbox_name(&self) -> &str {
        &self.sandbox
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

    fn sandbox_name(&self) -> &str {
        &self.sandbox
    }
}

/// The smallest terminal hegn will attach to. The bottom line is reserved for
/// the status bar, and the guest needs at least a few lines above it.
const MIN_COLS: u16 = 32;
const MIN_ROWS: u16 = 4;
/// Redraw the bar after the guest has been quiet this long. This repairs a bar
/// removed by a guest clear-screen without constantly resetting SGR state, and
/// input/output is never interleaved with escape sequences while output flows.
const BAR_REFRESH: Duration = Duration::from_secs(10);

/// Attaches the process terminal to the console and restores it on drop,
/// including when the console loop unwinds with a panic.
struct TerminalGuard {
    cols: u16,
    rows: u16,
    bar: bool,
}

impl TerminalGuard {
    fn enter(bar: bool) -> Result<Self> {
        use crossterm::execute;
        use crossterm::terminal::{EnterAlternateScreen, disable_raw_mode, enable_raw_mode};

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
        let mut guard = Self { cols, rows, bar };
        if let Err(err) = guard.reset_region(&mut out) {
            drop(guard);
            return Err(err).context("failed to initialize the serial console");
        }
        Ok(guard)
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if cols < MIN_COLS || rows < MIN_ROWS {
            bail!(
                "terminal is too small to attach ({cols}x{rows}); need at least {MIN_COLS}x{MIN_ROWS}"
            );
        }
        self.cols = cols;
        self.rows = rows;
        let mut out = std::io::stdout();
        // Keep the guest scrollback; only move the scroll region and restore
        // the guest cursor. The status bar is redrawn by the caller.
        write!(out, "\x1b7{}r\x1b8", self.region())?;
        out.flush()?;
        Ok(())
    }

    /// Scroll region argument: the whole screen, or everything but the bar.
    fn region(&self) -> String {
        if self.bar {
            format!("[1;{}", self.rows - 1)
        } else {
            "[".to_string()
        }
    }

    /// Clear the alternate screen and reserve the bottom line when needed.
    fn reset_region(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        write!(out, "\x1b[2J\x1b[0m{}r\x1b[H", self.region())?;
        out.flush()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use crossterm::cursor::Show;
        use crossterm::execute;
        use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

        disable_raw_mode().ok();
        let mut out = std::io::stdout();
        // Reset the scroll region on both screens; a stale region would break
        // the user's shell after hegn exits.
        let _ = write!(out, "\x1b[r\x1b[0m");
        let _ = out.flush();
        let _ = execute!(out, LeaveAlternateScreen, Show);
        let _ = write!(out, "\x1b[r\x1b[0m");
        let _ = out.flush();
    }
}

/// Draw the reserved bottom line without disturbing the guest cursor.
fn draw_bar(out: &mut impl Write, rows: u16, line: &str) -> std::io::Result<()> {
    // DECSC/DECRC: save the guest cursor, paint the bar, restore the cursor.
    write!(out, "\x1b7\x1b[{};1H\x1b[0m\x1b[7m{line}\x1b[0m\x1b8", rows)?;
    out.flush()
}

/// Build a status line exactly `cols` display columns wide.
///
/// The project name is untrusted filesystem data, so control and bidi
/// override characters are replaced before they reach the terminal. The
/// detach hint keeps its space; the left side is truncated first.
fn bar_line(project: &str, harness: &str, pid: u32, log: Option<&Path>, cols: u16) -> String {
    let cols = cols as usize;
    let tail = " INPUT DIRECT · ^B detach";
    if cols <= tail.width() {
        return fit_width(tail, cols);
    }
    let mut left = format!(
        " hegn ▸ {} · {} · pid {}",
        sanitize_label(project),
        sanitize_label(harness),
        pid
    );
    if let Some(log) = log.and_then(|path| path.file_name()) {
        left.push_str(" · log ");
        left.push_str(&sanitize_label(&log.to_string_lossy()));
    }
    format!("{}{}", fit_width(&left, cols - tail.width()), tail)
}

/// Truncate or pad `input` to exactly `width` display columns.
fn fit_width(input: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in input.chars() {
        let char_width = ch.width().unwrap_or(0);
        if used + char_width > width {
            break;
        }
        out.push(ch);
        used += char_width;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// Attach an interactive terminal to a VM serial console.
///
/// This is the CLI equivalent of the TUI terminal view. Input is passed
/// straight to the guest UART (the status bar marks this as `INPUT DIRECT`); use
/// `hegn check` for pre-execution guardrails or the TUI for line-buffered
/// interception. `Ctrl-B` detaches without stopping the VM.
///
/// When `bar` is set, the bottom line shows the project, harness, VM pid and
/// persistent serial log while the guest uses the lines above it. The guest tty
/// size cannot be changed through the legacy serial port, so full-screen guest
/// programs keep their own geometry; pass `bar = false` to give them the whole
/// host terminal.
pub fn run_interactive(mut console: VmConsole, bar: bool) -> Result<()> {
    use crossterm::event::{self, Event, KeyEventKind, KeyModifiers};

    let mut guard = TerminalGuard::enter(bar)?;
    let project = console.project_name();
    let harness = console.harness().to_string();
    let pid = console.pid();
    let log = console.serial_log().map(Path::to_path_buf);

    let result = (|| -> Result<()> {
        let mut out = std::io::stdout();
        if bar {
            let (cols, rows) = guard.size();
            draw_bar(
                &mut out,
                rows,
                &bar_line(&project, &harness, pid, log.as_deref(), cols),
            )?;
        }
        let mut last_bar = Instant::now();
        loop {
            let chunks = console.drain();
            let had_output = !chunks.is_empty();
            for chunk in chunks {
                out.write_all(&chunk)?;
            }
            out.flush()?;
            if console.is_exited() {
                break;
            }

            // Never inject the bar between two chunks of a possibly split
            // escape sequence. While output flows, the scroll region keeps the
            // bar intact; when the guest goes quiet, refresh it.
            if had_output {
                last_bar = Instant::now();
            } else if bar && last_bar.elapsed() >= BAR_REFRESH {
                let (cols, rows) = guard.size();
                draw_bar(
                    &mut out,
                    rows,
                    &bar_line(&project, &harness, pid, log.as_deref(), cols),
                )?;
                last_bar = Instant::now();
            }

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == crossterm::event::KeyCode::Char('b')
                        {
                            break;
                        }
                        if let Some(bytes) = crate::app::encode_key(key) {
                            console.send(&bytes)?;
                        }
                    }
                    Event::Resize(cols, rows) => {
                        guard.resize(cols, rows)?;
                        if bar {
                            draw_bar(
                                &mut out,
                                rows,
                                &bar_line(&project, &harness, pid, log.as_deref(), cols),
                            )?;
                        }
                        last_bar = Instant::now();
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    drop(guard);
    result?;
    let project = sanitize_label(&project);
    match &log {
        Some(log) => println!(
            "Detached from {project}. Serial log: {}",
            sanitize_label(&log.display().to_string())
        ),
        None => println!("Detached from {project}."),
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::sandbox::{SandboxPaths, SessionRecord};
    use chrono::Utc;
    use std::os::unix::net::UnixListener;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.state_dir = dir.join("state");
        config.sandboxes_dir = dir.join("sandboxes");
        config.backend = BackendKind::CloudHypervisor;
        config.cloud_hypervisor_bin = std::env::current_exe().unwrap();
        config
    }

    fn sandbox(dir: &Path) -> SandboxConfig {
        SandboxConfig {
            name: "demo".into(),
            project: dir.to_path_buf(),
            workspace: dir.to_path_buf(),
            harness: crate::config::Harness::Opencode,
            vcpus: 1,
            memory_mib: 128,
            ..SandboxConfig::default()
        }
    }

    fn write_session(
        config: &Config,
        sandbox: &SandboxConfig,
        dir: &Path,
        vm_pid: u32,
        relay_pid: u32,
        log: Option<PathBuf>,
    ) {
        let paths = SandboxPaths::from_sandbox(config, sandbox);
        paths.ensure().unwrap();
        let record = SessionRecord {
            project: dir.to_path_buf(),
            workspace: dir.to_path_buf(),
            sandbox: sandbox.name.clone(),
            harness: crate::config::Harness::Opencode,
            started_at: Utc::now(),
            vm_pid,
            vm_binary: None,
            relay_pid,
            virtiofs_pid: std::process::id(),
            boot_args: String::new(),
            serial_log: log,
        };
        std::fs::write(&paths.session, serde_json::to_string(&record).unwrap()).unwrap();
    }

    #[test]
    fn attach_requires_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let sandbox = sandbox(dir.path());
        let err = VmConsole::attach(&config, &sandbox)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no VM session"), "{err}");
    }

    #[test]
    fn attach_reads_and_writes_the_relay_socket() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let sandbox = sandbox(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        paths.ensure().unwrap();
        let listener = UnixListener::bind(&paths.console_socket).unwrap();
        let serial_log = dir.path().join("session-test.log");
        std::fs::write(&serial_log, b"boot complete\n").unwrap();
        // relay_pid = 0 skips the process check; the socket is the real test.
        write_session(
            &config,
            &sandbox,
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

        let mut console = VmConsole::attach(&config, &sandbox).unwrap();
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
        let sandbox = sandbox(dir.path());
        write_session(&config, &sandbox, dir.path(), std::process::id(), 0, None);
        let err = VmConsole::attach(&config, &sandbox)
            .unwrap_err()
            .to_string();
        assert!(err.contains("console relay socket is missing"), "{err}");
    }

    #[test]
    fn attach_rejects_a_dead_vm() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let sandbox = sandbox(dir.path());
        let paths = SandboxPaths::from_sandbox(&config, &sandbox);
        paths.ensure().unwrap();
        let _listener = UnixListener::bind(&paths.console_socket).unwrap();
        // Out of pid_t range: cannot be running, even after pid reuse.
        write_session(&config, &sandbox, dir.path(), 4_000_000_000, 0, None);

        let err = VmConsole::attach(&config, &sandbox)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running"), "{err}");
    }

    #[test]
    fn bar_line_fills_the_terminal_and_keeps_the_detach_hint() {
        let line = bar_line("demo", "OpenCode", 1234, None, 80);
        assert_eq!(line.width(), 80);
        assert!(line.contains("demo"), "{line}");
        assert!(line.contains("OpenCode"), "{line}");
        assert!(line.contains("INPUT DIRECT"), "{line}");
        assert!(line.contains("^B detach"), "{line}");
    }

    #[test]
    fn bar_line_truncates_the_project_first() {
        let long = "x".repeat(200);
        let line = bar_line(&long, "Codex", 7, Some(Path::new("/x/session-a.log")), 40);
        assert_eq!(line.width(), 40);
        assert!(line.ends_with(" INPUT DIRECT · ^B detach"), "{line}");
        assert!(!line.contains("session-a.log"), "{line}");
    }

    #[test]
    fn bar_line_strips_control_characters_from_project_names() {
        let line = bar_line("evil\x1b[31mname\u{202e}", "shell", 1, None, 60);
        assert!(!line.contains('\x1b'), "{line:?}");
        assert!(!line.contains('\u{202e}'), "{line:?}");
        assert!(line.contains("evil?[31mname?"), "{line}");
    }

    #[test]
    fn fit_width_handles_wide_characters() {
        assert_eq!(fit_width("日本", 4), "日本");
        assert_eq!(fit_width("日本", 3), "日 ");
        assert_eq!(fit_width("abc", 2), "ab");
        assert_eq!(fit_width("abc", 5), "abc  ");
    }
}
