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
/// Control characters (including BEL, ESC and newlines) and Unicode format
/// characters are replaced with spaces so a title can never inject another
/// terminal escape or reorder the surrounding text.
pub fn title_sequence(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .map(|character| {
            if is_unsafe_title_character(character) {
                ' '
            } else {
                character
            }
        })
        .collect();
    format!("\x1b]0;{sanitized}\x07")
}

/// Whether `character` must not appear in a terminal title.
///
/// [`char::is_control`] covers C0/C1, which includes BEL, ESC, CR and LF. The
/// standard library has no Unicode category API, so the format characters
/// (`Cf`) and the line/paragraph separators (`Zl`, `Zp`) are listed explicitly.
/// The list is the security-relevant subset: bidi controls, zero-width
/// joiners, tag characters and the separators that can break an OSC sequence.
fn is_unsafe_title_character(character: char) -> bool {
    character.is_control()
        || matches!(character,
            '\u{00AD}'                    // soft hyphen
            | '\u{061C}'                  // Arabic letter mark
            | '\u{070F}'                  // Syriac abbreviation mark
            | '\u{180E}'                  // Mongolian vowel separator
            | '\u{200B}'..='\u{200F}'     // zero width space .. right-to-left mark
            | '\u{2028}' | '\u{2029}'     // line / paragraph separator (Zl, Zp)
            | '\u{202A}'..='\u{202E}'     // bidi embedding and override
            | '\u{2060}'..='\u{2064}'     // word joiner .. invisible plus
            | '\u{2066}'..='\u{206F}'     // bidi isolates .. deprecated format controls
            | '\u{FEFF}'                  // zero width no-break space / BOM
            | '\u{FFF9}'..='\u{FFFB}'     // interlinear annotation
            | '\u{110BD}' | '\u{110CD}'   // Kaithi number signs
            | '\u{13430}'..='\u{1343F}'   // Egyptian hieroglyph format controls
            | '\u{1BCA0}'..='\u{1BCA3}'   // shorthand format controls
            | '\u{1D173}'..='\u{1D17A}'   // musical symbol format controls
            | '\u{E0001}'                 // language tag
            | '\u{E0020}'..='\u{E007F}'   // tag characters
        )
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
    fn unicode_format_characters_are_replaced_before_they_reach_the_terminal() {
        let sequence =
            title_sequence("a\u{202E}b\u{2066}c\u{200B}d\u{FEFF}e\u{2028}f\u{2029}g\u{1D173}h");
        assert_eq!(sequence, "\x1b]0;a b c d e f g h\x07");
        assert_eq!(sequence.matches('\x07').count(), 1);
        assert_eq!(sequence.matches('\x1b').count(), 1);
    }

    #[test]
    fn reset_sequence_clears_the_title() {
        assert_eq!(reset_sequence(), "\x1b]0;\x07");
        assert!(reset_sequence().ends_with('\x07'));
    }
}
