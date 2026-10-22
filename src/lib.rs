//! hegn — Firecracker MicroVM sandbox manager for agentic coding workflows.
//!
//! The crate is split into small, testable modules:
//!
//! - [`config`] — user configuration, paths and defaults
//! - [`project`] — project discovery, fuzzy filtering and git summaries
//! - [`security`] — `tirith-core` integration and workspace guardrails
//! - [`firecracker`] — typed Firecracker REST client over a Unix socket
//! - [`virtiofs`] — hardened `virtiofsd` command construction
//! - [`sandbox`] — MicroVM lifecycle orchestration
//! - [`snapshot`] — on-disk snapshot metadata
//! - [`logs`] — private, persistent serial-console logs
//! - [`console`] — Firecracker serial-console transport (no host fallback)
//! - [`terminal`] — ANSI-aware scrollback buffer
//! - [`state`] — persisted recent-project state
//! - [`app`] — TUI application state and event handling
//! - [`ui`] — ratatui rendering
//! - [`tui`] — terminal lifecycle and event loop
//! - [`cli`] — non-interactive command surface

pub mod app;
pub mod cli;
pub mod config;
pub mod console;
pub mod firecracker;
pub mod logs;
pub mod project;
pub mod sandbox;
pub mod security;
pub mod snapshot;
pub mod state;
pub mod terminal;
pub mod tui;
pub mod ui;
pub mod virtiofs;

pub use app::App;
pub use config::{BackendKind, Config, Harness, SecurityMode};
