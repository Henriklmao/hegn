//! Session transport, persistent logs and persisted session state.
//!
//! - [`console`] — serial-console transport (no host fallback)
//! - [`relay`] — detached serial-console relay
//! - [`attach_bar`] — bottom status bar and sequence-safe output gate
//! - [`terminal`] — ANSI-aware scrollback buffer
//! - [`logs`] — private, persistent serial-console logs
//! - [`snapshot`] — on-disk snapshot metadata
//! - [`state`] — persisted recent and hidden project state

pub mod attach_bar;
pub mod console;
pub mod logs;
pub mod relay;
pub mod snapshot;
pub mod state;
pub mod terminal;
