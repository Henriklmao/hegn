//! The reserved bottom row and the text painted on it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::session::logs::sanitize_label;

/// Hint on the right side of the bar.
///
/// `^B` is the tmux-style detach key. `^]` is the classic serial-console
/// escape: a nested tmux consumes `^B` as its own prefix before hegn sees it.
pub const DETACH_HINT: &str = " INPUT DIRECT · Ctrl-B/Ctrl-] detach";

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

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

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
}
