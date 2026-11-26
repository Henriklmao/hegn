//! RAII terminal-title guard for interactive SSH sessions.
//!
//! The guard sets the terminal title to a session label and clears it again
//! when dropped, so a crashed session never leaves the tab mislabelled. Titles
//! are only written when stdout is a terminal; piped output stays clean.

use std::io::{self, IsTerminal, Write};

/// Sets an OSC 0 terminal title until dropped.
#[derive(Debug)]
pub struct TitleGuard {
    active: bool,
}

impl TitleGuard {
    /// Set `title` on stdout when it is a terminal, otherwise return a no-op.
    pub fn set(title: &str) -> io::Result<Self> {
        if !io::stdout().is_terminal() {
            return Ok(Self { active: false });
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(title_sequence(title).as_bytes())?;
        stdout.flush()?;
        Ok(Self { active: true })
    }
}

impl Drop for TitleGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut stdout = io::stdout().lock();
        // The terminal may already be gone; a failed reset is not actionable.
        let _ = stdout.write_all(reset_sequence().as_bytes());
        let _ = stdout.flush();
    }
}

/// OSC 0 sequence that sets the terminal title.
///
/// Control characters (including BEL, ESC and newlines) are replaced with
/// spaces so a title can never inject another terminal escape.
pub fn title_sequence(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    format!("\x1b]0;{sanitized}\x07")
}

/// OSC 0 sequence that clears the terminal title.
pub fn reset_sequence() -> &'static str {
    "\x1b]0;\x07"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_sequence_wraps_the_label_in_an_osc_zero() {
        assert_eq!(title_sequence("hegn: rust"), "\x1b]0;hegn: rust\x07");
    }

    #[test]
    fn control_characters_are_replaced_before_they_reach_the_terminal() {
        let sequence = title_sequence("a\x07b\x1bc\nd\re");
        assert_eq!(sequence, "\x1b]0;a b c d e\x07");
        assert_eq!(sequence.matches('\x07').count(), 1);
        assert_eq!(sequence.matches('\x1b').count(), 1);
        assert!(!sequence.contains('\n'));
        assert!(!sequence.contains('\r'));
    }

    #[test]
    fn reset_sequence_clears_the_title() {
        assert_eq!(reset_sequence(), "\x1b]0;\x07");
        assert!(reset_sequence().ends_with('\x07'));
    }
}
