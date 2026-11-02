//! TUI application state and event handling.
//!
//! All state transitions live in this module tree and are covered by unit tests
//! next to the code they exercise; [`crate::tui::ui`] is a pure renderer and
//! [`crate::tui`] owns the terminal loop. This separation makes the dashboard
//! testable with ratatui's `TestBackend`.
//!
//! The split keeps one concern per submodule:
//!
//! * [`types`] holds the data model, including [`App`].
//! * [`state`] loads projects and instances and drives selection and polling.
//! * [`keys`] and [`mouse`] translate input into state transitions.
//! * [`actions`] dispatches footer buttons and clickable hints.
//! * [`project`], [`config`], [`terminal`], [`launch`], [`snapshots`],
//!   [`provision`], [`security`] and [`doctor`] group transitions by concern.
//! * [`util`] contains the small pure helpers.

mod actions;
mod config;
mod doctor;
mod keys;
mod launch;
mod modals;
mod mouse;
mod project;
mod provision;
mod security;
mod snapshots;
mod state;
mod terminal;
#[cfg(test)]
mod test_support;
mod types;
mod util;

pub use self::types::*;
pub use self::util::encode_key;
