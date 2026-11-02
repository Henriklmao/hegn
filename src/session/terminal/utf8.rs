//! Decoding of serial chunks that may split a UTF-8 character.

/// Append `bytes` to `pending` and decode everything that is complete.
///
/// An incomplete sequence at the end is kept in `pending` for the next read;
/// invalid bytes are rendered as replacement characters rather than stalling
/// forever on them.
pub(super) fn feed(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    let decoded = {
        match std::str::from_utf8(pending) {
            Ok(text) => Ok(text.to_string()),
            Err(err) => Err((err.valid_up_to(), err.error_len().is_none())),
        }
    };
    match decoded {
        Ok(text) => {
            pending.clear();
            text
        }
        Err((valid, true)) => {
            // Incomplete sequence at the end: keep it for the next read.
            let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
            pending.drain(..valid);
            text
        }
        Err((_, false)) => {
            // Invalid bytes: render replacement characters rather than
            // stalling forever on them.
            let text = String::from_utf8_lossy(pending).into_owned();
            pending.clear();
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_reassembles_characters_split_at_every_boundary() {
        for text in ["é", "日", "😀"] {
            let bytes = text.as_bytes();
            for split in 1..bytes.len() {
                let mut pending = Vec::new();
                assert_eq!(feed(&mut pending, &bytes[..split]), "", "{text} {split}");
                assert_eq!(pending.as_slice(), &bytes[..split]);
                assert_eq!(feed(&mut pending, &bytes[split..]), text);
                assert!(pending.is_empty(), "{text} {split}");
            }
        }
    }

    #[test]
    fn feed_handles_one_byte_per_read() {
        let text = "héllo 日本 😀";
        let mut pending = Vec::new();
        let mut out = String::new();
        for byte in text.as_bytes() {
            out.push_str(&feed(&mut pending, &[*byte]));
        }
        assert_eq!(out, text);
        assert!(pending.is_empty());
    }

    #[test]
    fn feed_never_stalls_on_invalid_bytes() {
        let mut pending = Vec::new();
        assert_eq!(feed(&mut pending, b"ok \xff"), "ok \u{fffd}");
        assert!(pending.is_empty());

        assert_eq!(feed(&mut pending, &[0xe6, 0xff]), "\u{fffd}\u{fffd}");
        assert!(pending.is_empty());
    }

    #[test]
    fn feed_keeps_only_a_genuinely_incomplete_tail() {
        let mut pending = Vec::new();
        // ASCII, including ESC, is complete on its own.
        assert_eq!(feed(&mut pending, b"abc\x1b"), "abc\x1b");
        assert!(pending.is_empty());

        assert_eq!(feed(&mut pending, &[0xe6, 0x97]), "");
        assert_eq!(pending.as_slice(), [0xe6, 0x97].as_slice());
        assert_eq!(feed(&mut pending, &[0xa5]), "日");
        assert!(pending.is_empty());
    }

    #[test]
    fn feed_ignores_empty_input() {
        let mut pending = Vec::new();
        assert_eq!(feed(&mut pending, b""), "");
        assert!(pending.is_empty());
        pending.push(0xe6);
        assert_eq!(feed(&mut pending, b""), "");
        assert_eq!(pending.as_slice(), [0xe6].as_slice());
    }
}
