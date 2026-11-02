//! Safe labels and path segments derived from untrusted names.

/// Remove terminal control and bidi-override characters from untrusted text
/// before it is written into a status bar or log name.
pub fn sanitize_label(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_control() || is_bidi_control(ch) {
                '?'
            } else {
                ch
            }
        })
        .collect()
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch as u32,
        0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
    )
}

/// Turn arbitrary project names into a safe single path segment.
pub fn sanitize_segment(input: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        let allowed = ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.');
        if allowed {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= max_bytes {
            break;
        }
    }
    let trimmed = out.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_control_characters_and_project_segments() {
        assert_eq!(sanitize_label("a\x1b[31mb\u{202e}"), "a?[31mb?");
        assert_eq!(sanitize_segment("My Project/../x", 64), "My-Project-..-x");
        assert_eq!(sanitize_segment("///", 64), "project");
        assert_eq!(sanitize_segment("abcdef", 3), "abc");
    }

    #[test]
    fn sanitize_label_replaces_every_bidi_control() {
        for ch in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            assert_eq!(sanitize_label(&ch.to_string()), "?", "{ch:?}");
        }
    }

    #[test]
    fn sanitize_label_keeps_printable_unicode_and_drops_c1_controls() {
        assert_eq!(sanitize_label("héllo 日本"), "héllo 日本");
        // U+009B is the 8-bit CSI introducer; it must not reach a terminal.
        assert_eq!(sanitize_label("\u{9b}31mred"), "?31mred");
        assert_eq!(sanitize_label("a\nb\tc\r"), "a?b?c?");
        assert_eq!(sanitize_label(""), "");
    }

    #[test]
    fn sanitize_segment_removes_path_separators_and_traversal() {
        assert_eq!(sanitize_segment("../../etc/passwd", 64), "etc-passwd");
        assert_eq!(sanitize_segment("/abs/path", 64), "abs-path");
        assert_eq!(sanitize_segment("a\\b", 64), "a-b");
        assert_eq!(sanitize_segment("a b\tc", 64), "a-b-c");
        assert_eq!(sanitize_segment(".../...", 64), "project");
        assert_eq!(sanitize_segment("\0\0", 64), "project");
    }

    #[test]
    fn sanitize_segment_is_ascii_and_bounded() {
        for input in ["Ünïcode", "日本語", "a\u{202e}b", "  ", "---"] {
            let out = sanitize_segment(input, 64);
            assert!(out.is_ascii(), "{input:?} -> {out:?}");
            assert!(!out.is_empty(), "{input:?}");
            assert!(!out.starts_with('.'), "{input:?} -> {out:?}");
            assert!(!out.contains('/') && !out.contains('\\'), "{out:?}");
        }
        assert_eq!(sanitize_segment("abcdef", 1), "a");
        assert_eq!(sanitize_segment("abcdef", 0), "a");
    }
}
