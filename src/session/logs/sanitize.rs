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
}
