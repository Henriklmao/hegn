//! Pure helpers shared by the TUI state machine.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

pub(super) fn clamp_index(current: usize, delta: isize, max: usize) -> usize {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(max)
    }
}

pub(super) fn rect_contains(rect: Rect, pos: (u16, u16)) -> bool {
    pos.0 >= rect.x
        && pos.0 < rect.x.saturating_add(rect.width)
        && pos.1 >= rect.y
        && pos.1 < rect.y.saturating_add(rect.height)
}

pub(super) use crate::shared::math::cycle_value;

/// Encode a key for raw passthrough mode.
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if c.is_ascii_alphabetic() {
                    vec![c.to_ascii_uppercase() as u8 - 0x40]
                } else {
                    match c {
                        ' ' | '@' => vec![0x00],
                        '[' => vec![0x1b],
                        '\\' => vec![0x1c],
                        ']' => vec![0x1d],
                        '^' => vec![0x1e],
                        '_' => vec![0x1f],
                        _ => return None,
                    }
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                let mut bytes = vec![0x1b];
                bytes.extend(c.to_string().into_bytes());
                bytes
            } else {
                c.to_string().into_bytes()
            }
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => return None,
        },
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn cycle_value_wraps_and_falls_back_to_the_first_entry() {
        let values = [None, Some(Integration::Tmux)];
        assert_eq!(
            cycle_value(&values, &Some(Integration::Tmux), 1),
            None,
            "last value wraps to the first"
        );
        assert_eq!(cycle_value(&values, &None, -1), Some(Integration::Tmux));
        assert_eq!(
            cycle_value(&values, &Some(Integration::External), 1),
            Some(Integration::Tmux),
            "unknown values restart from the first entry"
        );
    }

    #[test]
    fn clamp_index_saturates_on_both_ends() {
        assert_eq!(clamp_index(0, -5, 10), 0);
        assert_eq!(clamp_index(3, -2, 10), 1);
        assert_eq!(clamp_index(9, 4, 10), 10);
        assert_eq!(clamp_index(10, 1, 10), 10);
        assert_eq!(clamp_index(5, 0, 10), 5);
    }

    #[test]
    fn rect_contains_excludes_the_far_edges() {
        let rect = Rect::new(2, 3, 4, 2);
        assert!(rect_contains(rect, (2, 3)));
        assert!(rect_contains(rect, (5, 4)));
        assert!(!rect_contains(rect, (1, 3)));
        assert!(!rect_contains(rect, (6, 3)));
        assert!(!rect_contains(rect, (2, 5)), "height is exclusive");
    }

    #[test]
    fn encode_key_supports_ctrl_and_arrows() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(ctrl_c), Some(vec![0x03]));
        assert_eq!(encode_key(key(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(2))), Some(b"\x1bOQ".to_vec()));
        let alt_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(encode_key(alt_b), Some(vec![0x1b, b'b']));
        // A non-ASCII char must not be truncated into an ASCII control code.
        let ctrl_non_ascii = KeyEvent::new(KeyCode::Char('Ń'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(ctrl_non_ascii), None);
        let ctrl_bracket = KeyEvent::new(KeyCode::Char('['), KeyModifiers::CONTROL);
        assert_eq!(encode_key(ctrl_bracket), Some(vec![0x1b]));
    }
}
