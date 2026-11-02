//! Display-width aware text helpers shared by every renderer.

use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Last `max` display columns of `text`, so an inline editor keeps the cursor
/// visible in long paths.
pub(crate) fn tail(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let char_width = ch.width().unwrap_or(0);
        if width + char_width > max {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.chars().rev().collect()
}

/// Word-wrap plain text to `max` display columns, hard-splitting words (for
/// long paths) when a single word cannot fit.
pub(crate) fn wrap_text(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = word.width();
        if current_width > 0 && current_width + 1 + word_width > max {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if current_width > 0 {
            current.push(' ');
            current_width += 1;
        }
        for ch in word.chars() {
            let char_width = ch.width().unwrap_or(0);
            if current_width + char_width > max {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += char_width;
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

/// Ellipsize a styled line to `max` display columns.
pub(crate) fn fit_line(line: Line<'static>, max: usize) -> Line<'static> {
    if line.width() <= max {
        return line;
    }
    if max == 0 {
        return Line::default();
    }
    let style = line.style;
    let mut spans = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        let remaining = max.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let width = span.content.width();
        if width <= remaining {
            used += width;
            spans.push(span);
        } else {
            spans.push(Span::styled(truncate(&span.content, remaining), span.style));
            break;
        }
    }
    Line::from(spans).style(style)
}

pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = ch.width().unwrap_or(0);
        // Reserve one column for the ellipsis.
        if width + char_width + 1 > max {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abc", 0), "");
        let wide = truncate("日本語", 4);
        assert!(wide.ends_with('…'), "{wide}");
        assert!(wide.width() <= 4, "{wide:?} has width {}", wide.width());
        let narrow = truncate("abcdef", 4);
        assert_eq!(narrow, "abc…");
    }

    #[test]
    fn wrap_text_wraps_words_and_hard_splits_long_paths() {
        assert_eq!(wrap_text("hello world", 6), vec!["hello", "world"]);
        assert_eq!(wrap_text("", 5), vec![String::new()]);
        let path = "/very/long/path";
        let lines = wrap_text(path, 5);
        assert!(lines.iter().all(|line| line.width() <= 5), "{lines:?}");
        assert_eq!(lines.concat(), path);
    }
}
