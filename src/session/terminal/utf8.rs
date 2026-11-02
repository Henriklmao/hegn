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
