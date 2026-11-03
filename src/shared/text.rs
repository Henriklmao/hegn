//! Text formatting and POSIX shell quoting.

use std::ffi::OsStr;

/// Render bytes with binary units and one decimal place.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Quote one word for a POSIX shell.
///
/// Safe words are left untouched; everything else is wrapped in single quotes
/// with `'` escaped as `'"'"'`.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@%+,".contains(ch))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

/// [`shell_quote`] for an [`OsStr`].
pub fn shell_quote_os(value: &OsStr) -> String {
    shell_quote(&value.to_string_lossy())
}

/// Always wrap `value` in single quotes, even when it is a safe word.
///
/// Used for generated environment files where a stable visual form matters.
pub fn shell_quote_always(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(2048 + 512), "2.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024 * 1024), "1.0 TiB");
    }

    #[test]
    fn shell_quote_handles_hostile_words() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), r#"'a'\''b'"#);
        assert_eq!(shell_quote("$(id)"), "'$(id)'");
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_always_quotes_safe_words() {
        assert_eq!(shell_quote_always("plain"), "'plain'");
        assert_eq!(shell_quote_always("a'b"), r#"'a'\''b'"#);
    }
}
