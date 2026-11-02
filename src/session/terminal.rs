//! ANSI-aware scrollback buffer for the VM serial console.

use std::collections::VecDeque;

/// Maximum number of scrollback lines retained in memory.
pub const DEFAULT_SCROLLBACK: usize = 10_000;
pub const MAX_LINE_BYTES: usize = 8_192;

#[derive(Debug)]
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
    /// Bytes of a UTF-8 sequence split across serial reads.
    pending: Vec<u8>,
    state: AnsiState,
    /// A carriage return arrived. The next printable character overwrites the
    /// current line. Serial consoles emit CRLF, so CR on its own must not
    /// discard the line before LF commits it.
    pending_cr: bool,
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
            pending_cr: false,
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
                '\r' => self.pending_cr = true,
                '\t' => {
                    self.overwrite_after_cr();
                    let width = 4 - (self.partial.chars().count() % 4);
                    for _ in 0..width {
                        self.partial.push(' ');
                    }
                }
                '\x08' => {
                    if self.pending_cr {
                        self.overwrite_after_cr();
                    } else {
                        self.partial.pop();
                    }
                }
                c if c.is_control() => {}
                c => {
                    self.overwrite_after_cr();
                    if self.partial.len() < MAX_LINE_BYTES {
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

    fn overwrite_after_cr(&mut self) {
        if self.pending_cr {
            self.partial.clear();
            self.pending_cr = false;
        }
    }

    fn commit_line(&mut self) {
        self.pending_cr = false;
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

    /// Clone only the half-open range `start..end` of the combined view.
    ///
    /// The renderer calls this for every frame; cloning the full scrollback
    /// there would be needlessly expensive.
    pub fn range(&self, start: usize, end: usize) -> Vec<String> {
        let total = self.len();
        let start = start.min(total);
        let end = end.min(total);
        let mut out = Vec::with_capacity(end.saturating_sub(start));
        for index in start..end {
            if index < self.lines.len() {
                out.push(self.lines[index].clone());
            } else if index == self.lines.len() {
                out.push(self.partial.clone());
            }
        }
        out
    }

    /// Last `height` lines, newest at the bottom.
    pub fn tail(&self, height: usize) -> Vec<String> {
        let total = self.len();
        let start = total.saturating_sub(height.max(1));
        self.range(start, total)
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
        self.pending_cr = false;
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

    #[test]
    fn buffer_handles_newlines_and_carriage_returns() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"line one\nline two\rrewritten\n");
        assert_eq!(buf.lines(), vec!["line one", "rewritten", ""]);
    }

    #[test]
    fn buffer_keeps_every_line_from_crlf_serial_output() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"[    0.000000] boot one\r\n[    0.000000] boot two\r\n");
        assert_eq!(
            buf.lines(),
            vec!["[    0.000000] boot one", "[    0.000000] boot two", ""]
        );
    }

    #[test]
    fn buffer_keeps_the_last_rewrite_of_a_carriage_return_line() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"progress 10%\rprogress 99%\rdone\nnext\r\n");
        assert_eq!(buf.lines(), vec!["done", "next", ""]);
    }

    #[test]
    fn buffer_handles_a_carriage_return_split_across_reads() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"first\r");
        buf.push_bytes(b"\nsecond\r");
        buf.push_bytes(b"\n");
        assert_eq!(buf.lines(), vec!["first", "second", ""]);
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
    fn range_returns_only_the_requested_window() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(b"a\nb\nc");
        assert_eq!(buf.range(1, 3), vec!["b", "c"]);
        assert_eq!(buf.range(0, 1), vec!["a"]);
        assert!(buf.range(10, 20).is_empty());
    }

    #[test]
    fn buffer_caps_very_long_lines() {
        let mut buf = TerminalBuffer::default();
        buf.push_bytes(&vec![b'a'; MAX_LINE_BYTES * 2]);
        buf.push_bytes(b"\n");
        assert_eq!(buf.lines()[0].len(), MAX_LINE_BYTES);
    }
}
