//! ANSI-aware scrollback buffer for the VM serial console.
//!
//! - [`buffer`] — the scrollback lines and their ANSI/control handling
//! - [`utf8`] — decoding of chunks that may split a UTF-8 character

mod buffer;
mod utf8;

pub use buffer::{DEFAULT_SCROLLBACK, MAX_LINE_BYTES, TerminalBuffer};
