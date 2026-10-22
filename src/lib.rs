//! hegn — Firecracker MicroVM sandbox manager for agentic coding workflows.
//!
//! The crate is split into small, testable modules:
//!
//! - [`config`] — user configuration, paths and defaults
//! - [`project`] — project discovery, fuzzy filtering and git summaries
//! - [`security`] — `tirith-core` integration and workspace guardrails
//! - [`firecracker`] — typed Firecracker REST client over a Unix socket
//! - [`virtiofs`] — hardened `virtiofsd` command construction
//! - [`snapshot`] — on-disk snapshot metadata
//! - [`session`] — PTY-backed local session used by the terminal view
//! - [`app`] — TUI application state and event handling
//! - [`ui`] — ratatui rendering

pub mod app;
pub mod cli;
pub mod config;
pub mod firecracker;
pub mod project;
pub mod sandbox;
pub mod security;
pub mod session;
pub mod snapshot;
pub mod state;
pub mod tui;
pub mod ui;
pub mod virtiofs;

pub use app::App;
pub use config::{BackendKind, Config, Harness, SecurityMode};
