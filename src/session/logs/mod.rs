//! Persistent, private serial-console logs.
//!
//! The detached console relay appends the guest UART to a log in
//! `<state>/logs/`, so the full boot and session output survives `hegn instance
//! stop` even while no terminal is attached.
//!
//! Log files can contain whatever an agent printed, including secrets or
//! terminal escape sequences. They are therefore created with mode `0600`
//! inside a `0700` directory, opened with `O_NOFOLLOW`, never resolved through
//! symlinks, and only its own `session-*.log` files are ever pruned.
//!
//! - [`store`] — the log directory, its entries and name resolution
//! - [`tail`] — bounded, symlink-safe reads of the end of a log
//! - [`prune`] — retention of the newest logs, never unlinking live ones
//! - [`sanitize`] — safe labels and path segments for untrusted names

mod prune;
mod sanitize;
mod store;
mod tail;

pub use sanitize::{sanitize_label, sanitize_segment};
pub use store::{DEFAULT_KEEP, LOG_SUFFIX, LogEntry, LogStore, SESSION_PREFIX, is_log_name};
pub use tail::tail_bytes;
