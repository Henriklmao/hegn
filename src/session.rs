//! PTY-backed terminal session and a small ANSI-aware scrollback buffer.
//!
//! Phase 1 runs the harness in a local PTY. The same [`Session`] interface is
//! intended to be fed by the Firecracker serial console once the VM backend is
//! wired up; keeping the terminal view independent of the transport makes the
//! TUI testable without KVM.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::config::Harness;

/// Maximum number of scrollback lines retained in memory.
const DEFAULT_SCROLLBACK: usize = 10_000;
const MAX_LINE_CHARS: usize = 8_192;

#[derive(Debug)]
pub enum SessionEvent {
    Output(Vec<u8>),
    Exited,
}

/// A running PTY child plus its output channel.
pub struct Session {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<SessionEvent>,
    label: String,
    project: PathBuf,
    started_at: Instant,
    exited: bool,
    bytes_received: u64,
}

impl Session {
    /// Spawn `program` inside a new PTY.
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        cols: u16,
        rows: u16,
        label: impl Into<String>,
        project: &Path,
    ) -> Result<Self> {
        if !cwd.is_dir() {
            anyhow::bail!("working directory {} does not exist", cwd.display());
        }
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.max(2),
                cols: cols.max(10),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to allocate PTY")?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("HEGN", "1");
        cmd.env("HEGN_WORKSPACE", cwd.as_os_str());

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn {program}"))?;
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .context("failed to open PTY writer")?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to open PTY reader")?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("hegn-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(SessionEvent::Output(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.send(SessionEvent::Exited);
            })
            .context("failed to spawn PTY reader thread")?;

        Ok(Self {
            writer,
            master: pair.master,
            child,
            rx,
            label: label.into(),
            project: project.to_path_buf(),
            started_at: Instant::now(),
            exited: false,
            bytes_received: 0,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn project(&self) -> &Path {
        &self.project
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    pub fn is_exited(&mut self) -> bool {
        if self.exited {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.exited = true;
                true
            }
            _ => false,
        }
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .context("failed to write to PTY")?;
        self.writer.flush().context("failed to flush PTY")?;
        Ok(())
    }

    /// Send a prepared command line. A carriage return is used because the
    /// child's terminal is in canonical mode.
    pub fn send_line(&mut self, line: &str) -> Result<()> {
        self.send(line.as_bytes())?;
        self.send(b"\r")
    }

    /// Resize the PTY to match the terminal pane.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows: rows.max(2),
                cols: cols.max(10),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize PTY")
    }

    /// Drain pending output events without blocking.
    pub fn drain(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(event) => {
                    if let SessionEvent::Output(bytes) = &event {
                        self.bytes_received += bytes.len() as u64;
                    }
                    if matches!(event, SessionEvent::Exited) {
                        self.exited = true;
                    }
                    events.push(event);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.exited {
                        self.exited = true;
                        events.push(SessionEvent::Exited);
                    }
                    break;
                }
            }
        }
        events
    }

    /// Block until output arrives or `timeout` elapses. Used by tests.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<SessionEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        self.exited = true;
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("label", &self.label)
            .field("project", &self.project)
            .field("started_at", &self.started_at)
            .field("exited", &self.exited)
            .field("bytes_received", &self.bytes_received)
            .finish()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Resolve the executable for a harness. The shell harness uses `$SHELL`.
pub fn harness_command(harness: Harness) -> (String, Vec<String>) {
    match harness {
        Harness::Shell => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            (shell, vec!["-l".to_string()])
        }
        other => (other.command().to_string(), Vec::new()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

/// Minimal terminal buffer. It strips ANSI control sequences (colors, titles,
/// cursor movement) that would otherwise corrupt a ratatui `Paragraph`.
#[derive(Debug)]
pub struct TerminalBuffer {
    lines: VecDeque<String>,
    partial: String,
    /// Bytes of a UTF-8 sequence split across PTY reads.
    pending: Vec<u8>,
    state: AnsiState,
    max_lines: usize,
}

impl Default for TerminalBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_SCROLLBACK)
    }
}

impl TerminalBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            partial: String::new(),
            pending: Vec::new(),
            state: AnsiState::Ground,
            max_lines: max_lines.max(1),
        }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let decoded = {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => Ok(text.to_string()),
                Err(err) => Err((err.valid_up_to(), err.error_len().is_none())),
            }
        };
        let text = match decoded {
            Ok(text) => {
                self.pending.clear();
                text
            }
            Err((valid, true)) => {
                // Incomplete sequence at the end: keep it for the next read.
                let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
                self.pending.drain(..valid);
                text
            }
            Err((_, false)) => {
                // Invalid bytes: render replacement characters rather than
                // stalling forever on them.
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                text
            }
        };
        for ch in text.chars() {
            self.push_char(ch);
        }
    }

    fn push_char(&mut self, ch: char) {
        match self.state {
            AnsiState::Ground => match ch {
                '\x1b' => self.state = AnsiState::Escape,
                '\n' => self.commit_line(),
                '\r' => self.partial.clear(),
                '\t' => {
                    let width = 4 - (self.partial.chars().count() % 4);
                    for _ in 0..width {
                        self.partial.push(' ');
                    }
                }
                '\x08' => {
                    self.partial.pop();
                }
                c if c.is_control() => {}
                c => {
                    if self.partial.chars().count() < MAX_LINE_CHARS {
                        self.partial.push(c);
                    }
                }
            },
            AnsiState::Escape => {
                self.state = match ch {
                    '[' => AnsiState::Csi,
                    ']' => AnsiState::Osc,
                    _ => AnsiState::Ground,
                };
            }
            AnsiState::Csi => {
                // CSI sequences end with a byte in 0x40..=0x7e.
                if ('\x40'..='\x7e').contains(&ch) {
                    self.state = AnsiState::Ground;
                }
            }
            AnsiState::Osc => {
                if ch == '\x07' {
                    self.state = AnsiState::Ground;
                } else if ch == '\x1b' {
                    self.state = AnsiState::OscEscape;
                }
            }
            AnsiState::OscEscape => {
                self.state = if ch == '\\' {
                    AnsiState::Ground
                } else {
                    AnsiState::Osc
                };
            }
        }
    }

    fn commit_line(&mut self) {
        let line = std::mem::take(&mut self.partial);
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines {
            self.lines.pop_front();
        }
    }

    /// Complete lines plus the in-progress line.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.lines.iter().cloned().collect();
        lines.push(self.partial.clone());
        lines
    }

    /// Last `height` lines, newest at the bottom.
    pub fn tail(&self, height: usize) -> Vec<String> {
        let lines = self.lines();
        let start = lines.len().saturating_sub(height.max(1));
        lines[start..].to_vec()
    }

    pub fn len(&self) -> usize {
        self.lines.len() + 1
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.partial.is_empty()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.partial.clear();
        self.pending.clear();
        self.state = AnsiState::Ground;
    }

    pub fn set_max_lines(&mut self, max: usize) {
        self.max_lines = max.max(1);
        while self.lines.len() > self.max_lines {
            self.lines.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn buffer_handles_newlines_and_carriage_returns() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"line one\nline two\rrewritten\n");
        assert_eq!(buf.lines(), vec!["line one", "rewritten", ""]);
    }

    #[test]
    fn buffer_strips_ansi_and_osc() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"\x1b[31mred\x1b[0m \x1b]0;title\x07line\n");
        assert_eq!(buf.lines()[0], "red line");
    }

    #[test]
    fn buffer_handles_backspace_and_tabs() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"ab\x08c\tX\n");
        assert_eq!(buf.lines()[0], "ac  X");
    }

    #[test]
    fn buffer_reassembles_utf8_split_across_reads() {
        let mut buf = TerminalBuffer::default();
        for byte in "héllo ✓".as_bytes() {
            // One byte per read splits every multi-byte character.
            buf.push_bytes(&[*byte]);
        }
        assert_eq!(buf.lines()[0], "héllo ✓");
    }

    #[test]
    fn buffer_replaces_invalid_utf8() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"ok \xff\n");
        assert_eq!(buf.lines()[0], "ok �");
    }

    #[test]
    fn buffer_limits_scrollback() {
        let mut buf = TerminalBuffer::new(2);
        for i in 0..10 {
            buf.push_bytes(format!("{i}\n").as_bytes());
        }
        assert_eq!(buf.tail(10), vec!["8", "9", ""]);
    }

    #[test]
    fn tail_returns_available_lines() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"only\n");
        assert_eq!(buf.tail(5), vec!["only", ""]);
        assert_eq!(buf.tail(1), vec![""]);
    }

    #[test]
    fn harness_shell_uses_login_shell() {
        let (program, args) = harness_command(Harness::Shell);
        assert!(!program.is_empty());
        assert_eq!(args, vec!["-l"]);
        let (program, args) = harness_command(Harness::Opencode);
        assert_eq!(program, "opencode");
        assert!(args.is_empty());
    }

    #[test]
    fn spawns_command_and_receives_output() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::spawn(
            "/bin/sh",
            &["-c".into(), "printf ready".into()],
            dir.path(),
            80,
            24,
            "test",
            dir.path(),
        )
        .unwrap();
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(SessionEvent::Output(bytes)) =
                session.recv_timeout(Duration::from_millis(100))
            {
                output.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&output).contains("ready") {
                    break;
                }
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("ready"));
    }

    #[test]
    fn spawn_rejects_missing_directory() {
        let err = Session::spawn(
            "/bin/sh",
            &[],
            Path::new("/definitely/missing/dir"),
            80,
            24,
            "test",
            Path::new("/tmp"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn spawn_rejects_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let err = Session::spawn(
            "/definitely/missing/binary",
            &[],
            dir.path(),
            80,
            24,
            "test",
            dir.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to spawn"));
    }
}
