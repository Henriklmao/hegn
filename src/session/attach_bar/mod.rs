//! Bottom status bar and guest-output gate for `hegn attach`.
//!
//! The serial console has no window-size channel: the guest writes to the host
//! terminal directly. That breaks a bar painted only once, in two ways:
//!
//! - a full-screen guest assumes it owns the last row and repaints over it;
//! - a guest that resets or replaces the scroll region (`CSI r`, RIS, DECSTR)
//!   lets ordinary line feeds scroll the bar up into the guest output.
//!
//! [`OutputGate`] therefore inspects the guest stream. It rewrites DECSTBM
//! sequences so the reserved row can never enter the guest's scrolling region,
//! restores the reservation after a terminal reset, and reports whether the
//! stream currently ends on a character boundary. [`StatusBar`] repaints the
//! bar after every output batch that ends on such a boundary, so a bar that
//! cannot be split by injecting a repaint into a partial escape sequence or
//! UTF-8 character stays pinned to the bottom row.
//!
//! - [`bar`] — the reserved row and the text painted on it
//! - [`gate`] — the sequence-aware filter in front of the host terminal
//! - [`detach`] — recognition of the detach key bindings

mod bar;
mod detach;
mod gate;

pub use bar::{DETACH_HINT, StatusBar};
pub use detach::detach_requested;
pub use gate::OutputGate;
