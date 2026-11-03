//! Recognition of the detach key bindings.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn other_keys_never_detach() {
        for code in [
            KeyCode::Char('c'),
            KeyCode::Char('d'),
            KeyCode::Char('z'),
            KeyCode::Char('B'),
            KeyCode::Char(']'),
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::F(1),
        ] {
            assert!(
                !detach_requested(KeyEvent::new(code, KeyModifiers::CONTROL)),
                "{code:?} must not detach"
            );
        }
    }

    #[test]
    fn detach_needs_the_control_modifier() {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::ALT, KeyModifiers::SHIFT] {
            assert!(!detach_requested(KeyEvent::new(
                KeyCode::Char('b'),
                modifiers
            )));
            assert!(!detach_requested(KeyEvent::new(
                KeyCode::Char('5'),
                modifiers
            )));
        }
    }
}
