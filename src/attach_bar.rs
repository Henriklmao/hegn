//! Bottom status bar and guest-output gate for `hegn attach`.
//!
//! The serial console has no window-size channel: the guest writes to the host
//! terminal directly. That breaks a bar painted only once, in two ways:
//!
//! - a full-screen guest assumes it owns the last row and repaints over it;
//! - a guest that resets or replaces the scroll region (`CSI r`, RIS, DECSTR)
//!   lets ordinary line feeds scroll the bar up into the guest output.
//!
//! [`OutputGate`] therefore inspects the guest stream. It rewrites DECSTBM
//! sequences so the reserved row can never enter the guest's scrolling region,
//! restores the reservation after a terminal reset, and reports whether the
//! stream currently ends on a character boundary. [`StatusBar`] repaints the
//! bar after every output batch that ends on such a boundary, so a bar that
//! cannot be split by injecting a repaint into a partial escape sequence or
//! UTF-8 character stays pinned to the bottom row.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::logs::sanitize_label;

/// Hint on the right side of the bar.
///
/// `^B` is the tmux-style detach key. `^]` is the classic serial-console
/// escape: a nested tmux consumes `^B` as its own prefix before hegn sees it.
pub const DETACH_HINT: &str = " INPUT DIRECT · Ctrl-B/Ctrl-] detach";

/// Whether a key event asks to detach without stopping the VM.
///
/// `Ctrl-B` is the tmux-style key. `Ctrl-]` is the classic serial-console
/// escape (telnet and friends) and is the fallback when hegn runs inside a
/// tmux session, where `Ctrl-B` is consumed as the outer prefix. Crossterm
/// reports the single `0x1d` byte as `Ctrl+5`.
pub fn detach_requested(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('b') | KeyCode::Char('5'))
}

/// The reserved bottom row plus the text painted on it.
#[derive(Debug)]
pub struct StatusBar {
    project: String,
    harness: String,
    pid: u32,
    log: Option<PathBuf>,
    cols: u16,
    rows: u16,
}

impl StatusBar {
    pub fn new(
        project: impl Into<String>,
        harness: impl Into<String>,
        pid: u32,
        log: Option<PathBuf>,
        cols: u16,
        rows: u16,
    ) -> Self {
        Self {
            project: project.into(),
            harness: harness.into(),
            pid,
            log,
            cols,
            rows,
        }
    }

    /// Number of host rows; the bar itself occupies the last one.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// The last row the guest may use.
    fn guest_bottom(&self) -> u16 {
        self.rows.saturating_sub(1).max(1)
    }

    /// The status line, exactly `cols` display columns wide.
    pub fn line(&self) -> String {
        bar_line(
            &self.project,
            &self.harness,
            self.pid,
            self.log.as_deref(),
            self.cols,
        )
    }

    /// Reset SGR and origin mode, then reserve the bottom row.
    pub fn reserve_region(&self, out: &mut impl Write) -> io::Result<()> {
        write!(out, "\x1b[0m\x1b[?6l\x1b[1;{}r\x1b[H", self.guest_bottom())?;
        out.flush()
    }

    /// Re-reserve the bottom row after the terminal was resized. The cursor and
    /// origin mode are preserved; the caller repaints the bar afterwards.
    pub fn resize(&mut self, cols: u16, rows: u16, out: &mut impl Write) -> io::Result<()> {
        self.cols = cols;
        self.rows = rows;
        write!(out, "\x1b7\x1b[?6l\x1b[1;{}r\x1b8", self.guest_bottom())?;
        out.flush()
    }

    /// Paint the bottom row without disturbing the guest cursor.
    ///
    /// DECSC/DECRC also save and restore origin mode and SGR on
    /// xterm-compatible terminals, which keeps the guest's state intact. The
    /// carriage return clears a pending wrap at the right margin before DECRC,
    /// so the guest cursor does not inherit it.
    pub fn paint(&self, out: &mut impl Write) -> io::Result<()> {
        let line = self.line();
        write!(
            out,
            "\x1b7\x1b[?6l\x1b[{};1H\x1b[0m\x1b[7m{line}\x1b[0m\r\x1b8",
            self.rows
        )?;
        out.flush()
    }
}

/// Build a status line exactly `cols` display columns wide.
///
/// The project name is untrusted filesystem data, so control and bidi
/// override characters are replaced before they reach the terminal. The
/// detach hint keeps its space; the left side is truncated first.
fn bar_line(project: &str, harness: &str, pid: u32, log: Option<&Path>, cols: u16) -> String {
    let cols = cols as usize;
    let tail = DETACH_HINT;
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    String,
    StringEsc,
}

/// Sequence-aware filter between the relay socket and the host terminal.
///
/// Ground text is written through immediately. Escape sequences are buffered
/// until their final byte so DECSTBM can be clamped and a reset can re-reserve
/// the bar. [`Self::safe_to_inject`] is false while an escape sequence or
/// multi-byte character is still in flight; the bar must not be painted at
/// such a boundary.
#[derive(Debug, Default)]
pub struct OutputGate {
    state: State,
    seq: Vec<u8>,
    seq_overflow: bool,
    escape_has_intermediate: bool,
    utf8_left: u8,
}

/// Buffered escape sequences are capped. Longer payloads (DCS graphics, very
/// long titles) are flushed and then passed through byte by byte. The bar is
/// not injected while such a sequence is still open, so the flush is safe.
const MAX_SEQ: usize = 8 * 1024;

impl OutputGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed guest bytes, emitting everything that is safe to write now.
    ///
    /// `bar_row` is the reserved row (`Some(rows)`) when the bar is enabled.
    pub fn feed(
        &mut self,
        bytes: &[u8],
        bar_row: Option<u16>,
        out: &mut impl Write,
    ) -> io::Result<()> {
        for &byte in bytes {
            self.advance_utf8(byte);
            self.feed_byte(byte, bar_row, out)?;
        }
        Ok(())
    }

    /// True when a complete escape sequence can be injected without splitting
    /// a guest escape sequence or a UTF-8 character.
    pub fn safe_to_inject(&self) -> bool {
        self.state == State::Ground && self.utf8_left == 0
    }

    /// Emit a held, incomplete escape sequence at the end of the console and
    /// terminate any open string so the detached message is not swallowed.
    pub fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&self.seq)?;
        match self.state {
            State::Osc | State::OscEsc => out.write_all(b"\x07")?,
            State::String | State::StringEsc => out.write_all(b"\x1b\\")?,
            State::Esc | State::Csi => out.write_all(&[0x18])?,
            State::Ground => {}
        }
        self.seq.clear();
        self.seq_overflow = false;
        self.state = State::Ground;
        self.utf8_left = 0;
        out.flush()
    }

    fn feed_byte(
        &mut self,
        byte: u8,
        bar_row: Option<u16>,
        out: &mut impl Write,
    ) -> io::Result<()> {
        match self.state {
            State::Ground => self.ground(byte, out),
            State::Esc => self.escape(byte, bar_row, out),
            State::Csi => self.csi_byte(byte, bar_row, out),
            State::Osc => match byte {
                0x07 | 0x18 | 0x1a | 0x9c => {
                    self.push(byte, out)?;
                    self.state = State::Ground;
                    self.emit(out)
                }
                0x1b => {
                    self.push(byte, out)?;
                    self.state = State::OscEsc;
                    Ok(())
                }
                _ => self.push(byte, out),
            },
            State::OscEsc => match byte {
                b'\\' => {
                    self.push(byte, out)?;
                    self.state = State::Ground;
                    self.emit(out)
                }
                0x1b => self.push(byte, out),
                _ => {
                    self.push(byte, out)?;
                    self.state = State::Osc;
                    Ok(())
                }
            },
            State::String => match byte {
                0x18 | 0x1a | 0x9c => {
                    self.push(byte, out)?;
                    self.state = State::Ground;
                    self.emit(out)
                }
                0x1b => {
                    self.push(byte, out)?;
                    self.state = State::StringEsc;
                    Ok(())
                }
                _ => self.push(byte, out),
            },
            State::StringEsc => match byte {
                b'\\' => {
                    self.push(byte, out)?;
                    self.state = State::Ground;
                    self.emit(out)
                }
                0x1b => self.push(byte, out),
                _ => {
                    self.push(byte, out)?;
                    self.state = State::String;
                    Ok(())
                }
            },
        }
    }

    fn ground(&mut self, byte: u8, out: &mut impl Write) -> io::Result<()> {
        match byte {
            0x1b => {
                self.begin_sequence(byte);
                Ok(())
            }
            0x9b => {
                self.begin_sequence(0x1b);
                self.seq.push(b'[');
                self.state = State::Csi;
                Ok(())
            }
            0x90 | 0x98 | 0x9e | 0x9f => {
                // Normalise 8-bit DCS/SOS/PM/APC introducers to 7-bit form.
                self.begin_sequence(0x1b);
                self.seq.push(byte - 0x40);
                self.state = State::String;
                Ok(())
            }
            0x9d => {
                self.begin_sequence(0x1b);
                self.seq.push(b']');
                self.state = State::Osc;
                Ok(())
            }
            _ => out.write_all(&[byte]),
        }
    }

    fn escape(&mut self, byte: u8, bar_row: Option<u16>, out: &mut impl Write) -> io::Result<()> {
        match byte {
            0x1b => {
                // ESC ESC aborts the current sequence and starts a new one.
                out.write_all(&self.seq)?;
                self.begin_sequence(byte);
                Ok(())
            }
            b'[' => {
                self.push(byte, out)?;
                self.state = State::Csi;
                Ok(())
            }
            b']' => {
                self.push(byte, out)?;
                self.state = State::Osc;
                Ok(())
            }
            b'P' | b'X' | b'^' | b'_' => {
                self.push(byte, out)?;
                self.state = State::String;
                Ok(())
            }
            0x20..=0x2f => {
                self.escape_has_intermediate = true;
                self.push(byte, out)
            }
            // A final byte ends the escape sequence.
            0x30..=0x7e => {
                self.push(byte, out)?;
                self.state = State::Ground;
                let is_ris = byte == b'c' && !self.escape_has_intermediate;
                self.emit(out)?;
                if is_ris {
                    // RIS: margins return to the full screen.
                    self.reserve(bar_row, out)?;
                }
                Ok(())
            }
            // C0 controls are executed without ending the sequence.
            _ => self.push(byte, out),
        }
    }

    fn csi_byte(&mut self, byte: u8, bar_row: Option<u16>, out: &mut impl Write) -> io::Result<()> {
        match byte {
            0x1b => {
                // ESC aborts a CSI sequence; emit it and start a new escape.
                out.write_all(&self.seq)?;
                self.begin_sequence(byte);
                Ok(())
            }
            0x40..=0x7e => self.finish_csi(byte, bar_row, out),
            _ => self.push(byte, out),
        }
    }

    fn finish_csi(
        &mut self,
        final_byte: u8,
        bar_row: Option<u16>,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.state = State::Ground;
        if self.seq_overflow {
            self.seq.clear();
            self.seq_overflow = false;
            return out.write_all(&[final_byte]);
        }
        let params = &self.seq[2..];
        if final_byte == b'r' {
            if let Some(rewritten) = clamp_margins(params, bar_row) {
                self.seq.clear();
                // `clamp_margins` returns a complete replacement sequence.
                return out.write_all(&rewritten);
            }
            return self.emit_final(final_byte, out);
        }
        if final_byte == b'p' && params == b"!" {
            self.emit_final(final_byte, out)?;
            // DECSTR also resets the scrolling region.
            return self.reserve(bar_row, out);
        }
        self.emit_final(final_byte, out)
    }

    /// Buffer one byte, flushing the sequence if it grows past the cap.
    fn push(&mut self, byte: u8, out: &mut impl Write) -> io::Result<()> {
        if !self.seq_overflow && self.seq.len() >= MAX_SEQ {
            out.write_all(&self.seq)?;
            self.seq.clear();
            self.seq_overflow = true;
        }
        if self.seq_overflow {
            out.write_all(&[byte])
        } else {
            self.seq.push(byte);
            Ok(())
        }
    }

    fn begin_sequence(&mut self, byte: u8) {
        self.seq.clear();
        self.seq.push(byte);
        self.seq_overflow = false;
        self.escape_has_intermediate = false;
        self.state = State::Esc;
    }

    fn emit(&mut self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&self.seq)?;
        self.seq.clear();
        self.seq_overflow = false;
        Ok(())
    }

    /// Emit a buffered sequence plus its final byte, which is not in `seq` yet.
    fn emit_final(&mut self, final_byte: u8, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&self.seq)?;
        out.write_all(&[final_byte])?;
        self.seq.clear();
        self.seq_overflow = false;
        Ok(())
    }

    fn reserve(&self, bar_row: Option<u16>, out: &mut impl Write) -> io::Result<()> {
        if let Some(row) = bar_row {
            write!(
                out,
                "\x1b7\x1b[?6l\x1b[1;{}r\x1b8",
                row.saturating_sub(1).max(1)
            )?;
        }
        Ok(())
    }

    /// Track whether the stream ends inside a multi-byte UTF-8 character.
    fn advance_utf8(&mut self, byte: u8) {
        if self.utf8_left > 0 {
            if (0x80..=0xbf).contains(&byte) {
                self.utf8_left -= 1;
                return;
            }
            self.utf8_left = 0;
        }
        self.utf8_left = match byte {
            0xc2..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf4 => 3,
            _ => 0,
        };
    }
}

/// Rewrite a DECSTBM that would put the reserved row inside the guest's
/// scrolling region. Returns `None` when the sequence is not DECSTBM, is
/// already safe, or uses parameters we do not understand (pass through).
fn clamp_margins(params: &[u8], bar_row: Option<u16>) -> Option<Vec<u8>> {
    let bar_row = bar_row?;
    if params
        .iter()
        .any(|byte| !matches!(byte, b'0'..=b'9' | b';'))
    {
        return None;
    }
    let text = std::str::from_utf8(params).ok()?;
    let mut parts = text.split(';');
    let top = parts.next().unwrap_or("");
    let bottom = parts.next();
    if parts.next().is_some() {
        // Extra parameters are undefined for DECSTBM; leave them alone.
        return None;
    }
    if !top.bytes().all(|b| b.is_ascii_digit())
        || bottom.is_some_and(|part| !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let rows = bar_row as u32;
    let last = rows.saturating_sub(1).max(1);
    let top = if top.is_empty() {
        1
    } else {
        top.parse::<u32>().ok()?.max(1)
    };
    let bottom = match bottom {
        None | Some("") => rows,
        Some(part) => {
            let value = part.parse::<u32>().ok()?;
            if value == 0 { rows } else { value }
        }
    };
    if bottom <= last {
        return None;
    }
    let top = top.clamp(1, last);
    Some(format!("\x1b[{top};{last}r").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_feed(gate: &mut OutputGate, input: &[u8], bar_row: Option<u16>) -> String {
        String::from_utf8(gate_feed_raw(gate, input, bar_row)).unwrap()
    }

    fn gate_feed_raw(gate: &mut OutputGate, input: &[u8], bar_row: Option<u16>) -> Vec<u8> {
        let mut out = Vec::new();
        gate.feed(input, bar_row, &mut out).unwrap();
        out
    }

    #[test]
    fn ctrl_b_and_ctrl_bracket_request_detach() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        assert!(detach_requested(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL
        )));
        // Crossterm reports the raw `0x1d` byte (Ctrl-]) as Ctrl+5.
        assert!(detach_requested(KeyEvent::new(
            KeyCode::Char('5'),
            KeyModifiers::CONTROL
        )));
        assert!(!detach_requested(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE
        )));
        assert!(!detach_requested(KeyEvent::new(
            KeyCode::Char('5'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn bar_line_fills_the_terminal_and_keeps_the_detach_hint() {
        let line = bar_line("demo", "OpenCode", 1234, None, 80);
        assert_eq!(line.width(), 80);
        assert!(line.contains("demo"), "{line}");
        assert!(line.contains("OpenCode"), "{line}");
        assert!(line.contains("INPUT DIRECT"), "{line}");
        assert!(line.contains("Ctrl-B/Ctrl-] detach"), "{line}");
    }

    #[test]
    fn bar_line_truncates_the_project_first() {
        let long = "x".repeat(200);
        let line = bar_line(&long, "Codex", 7, Some(Path::new("/x/session-a.log")), 40);
        assert_eq!(line.width(), 40);
        assert!(line.ends_with(DETACH_HINT), "{line}");
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
    fn bar_line_keeps_the_hint_on_a_narrow_terminal() {
        // The hint is the whole bar once the terminal is only as wide as it.
        let hint = DETACH_HINT.width() as u16;
        let line = bar_line("demo", "OpenCode", 1, None, hint);
        assert_eq!(line, DETACH_HINT);

        let line = bar_line("demo", "OpenCode", 1, None, hint + 4);
        assert_eq!(line.width(), usize::from(hint) + 4);
        assert!(line.ends_with(DETACH_HINT), "{line}");

        // Narrower than the hint: it is truncated instead of overflowing.
        let line = bar_line("demo", "OpenCode", 1, None, 20);
        assert_eq!(line.width(), 20);
        assert!(DETACH_HINT.starts_with(line.trim_end()), "{line:?}");
        assert!(!line.contains("demo"), "{line}");
    }

    #[test]
    fn fit_width_handles_wide_characters() {
        assert_eq!(fit_width("日本", 4), "日本");
        assert_eq!(fit_width("日本", 3), "日 ");
        assert_eq!(fit_width("abc", 2), "ab");
        assert_eq!(fit_width("abc", 5), "abc  ");
    }

    #[test]
    fn paint_targets_the_bottom_row_and_restores_the_cursor() {
        let bar = StatusBar::new("demo", "OpenCode", 4242, None, 80, 30);
        let mut out = Vec::new();
        bar.paint(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b7"), "{text:?}");
        assert!(text.contains("\x1b[30;1H"), "{text:?}");
        assert!(text.ends_with("\x1b8"), "{text:?}");
    }

    #[test]
    fn reserve_region_keeps_the_last_row_out_of_the_scroll_region() {
        let bar = StatusBar::new("demo", "OpenCode", 1, None, 80, 30);
        let mut out = Vec::new();
        bar.reserve_region(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[1;29r"), "{text:?}");
    }

    #[test]
    fn gate_passes_plain_text_through_immediately() {
        let mut gate = OutputGate::new();
        assert_eq!(gate_feed(&mut gate, b"hello\n", None), "hello\n");
        assert!(gate.safe_to_inject());
    }

    #[test]
    fn gate_holds_a_csi_split_across_reads() {
        let mut gate = OutputGate::new();
        assert_eq!(gate_feed(&mut gate, b"a\x1b[31", None), "a");
        assert!(!gate.safe_to_inject());
        assert_eq!(gate_feed(&mut gate, b"mred", None), "\x1b[31mred");
        assert!(gate.safe_to_inject());
    }

    #[test]
    fn gate_holds_a_split_utf8_character() {
        let mut gate = OutputGate::new();
        assert_eq!(gate_feed(&mut gate, "é".as_bytes(), None), "é");
        assert!(gate.safe_to_inject());

        let bytes = "日".as_bytes();
        assert_eq!(gate_feed_raw(&mut gate, &bytes[..1], None), &bytes[..1]);
        assert!(!gate.safe_to_inject());
        assert_eq!(gate_feed_raw(&mut gate, &bytes[1..], None), &bytes[1..]);
        assert!(gate.safe_to_inject());
    }

    #[test]
    fn gate_holds_an_osc_title_until_its_terminator() {
        let mut gate = OutputGate::new();
        assert_eq!(gate_feed(&mut gate, b"x\x1b]0;title", None), "x");
        assert!(!gate.safe_to_inject());
        assert_eq!(gate_feed(&mut gate, b"\x07y", None), "\x1b]0;title\x07y");
        assert!(gate.safe_to_inject());
    }

    #[test]
    fn gate_keeps_charset_designators_intact() {
        let mut gate = OutputGate::new();
        assert_eq!(gate_feed(&mut gate, b"a\x1b(", None), "a");
        assert!(!gate.safe_to_inject());
        assert_eq!(gate_feed(&mut gate, b"B", None), "\x1b(B");
        assert!(gate.safe_to_inject());
    }

    #[test]
    fn gate_clamps_margins_that_would_cover_the_bar() {
        let mut gate = OutputGate::new();
        assert_eq!(gate_feed(&mut gate, b"\x1b[r", Some(30)), "\x1b[1;29r");
        assert_eq!(gate_feed(&mut gate, b"\x1b[1;30r", Some(30)), "\x1b[1;29r");
        assert_eq!(gate_feed(&mut gate, b"\x1b[5r", Some(30)), "\x1b[5;29r");
    }

    #[test]
    fn gate_leaves_margins_above_the_bar_alone() {
        let mut gate = OutputGate::new();
        assert_eq!(gate_feed(&mut gate, b"\x1b[1;20r", Some(30)), "\x1b[1;20r");
        assert_eq!(
            gate_feed(&mut gate, b"\x1b[?1049h", Some(30)),
            "\x1b[?1049h"
        );
    }

    #[test]
    fn gate_passes_margins_through_when_the_bar_is_disabled() {
        let mut gate = OutputGate::new();
        assert_eq!(gate_feed(&mut gate, b"\x1b[r", None), "\x1b[r");
    }

    #[test]
    fn gate_restores_the_region_after_a_soft_reset() {
        let mut gate = OutputGate::new();
        let out = gate_feed(&mut gate, b"\x1b[!p", Some(30));
        assert_eq!(out, "\x1b[!p\x1b7\x1b[?6l\x1b[1;29r\x1b8");
    }

    #[test]
    fn gate_restores_the_region_after_a_hard_reset() {
        let mut gate = OutputGate::new();
        let out = gate_feed(&mut gate, b"\x1bc", Some(30));
        assert_eq!(out, "\x1bc\x1b7\x1b[?6l\x1b[1;29r\x1b8");
    }

    #[test]
    fn gate_finish_flushes_a_truncated_csi_and_aborts_it() {
        let mut gate = OutputGate::new();
        let mut out = Vec::new();
        gate.feed(b"\x1b[12", None, &mut out).unwrap();
        gate.finish(&mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[12\x18");
    }

    #[test]
    fn gate_finish_terminates_an_open_osc() {
        let mut gate = OutputGate::new();
        let mut out = Vec::new();
        gate.feed(b"\x1b]0;title", None, &mut out).unwrap();
        gate.finish(&mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b]0;title\x07");
    }
}
