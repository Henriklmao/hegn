//! Sequence-aware filter between the relay socket and the host terminal.

use std::io::{self, Write};

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
