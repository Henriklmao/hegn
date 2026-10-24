//! hegn — Cloud Hypervisor sandbox manager for agentic coding workflows.
//!
//! The crate is split into small, testable modules:
//!
//! - [`config`] — user configuration, paths and defaults
//! - [`project`] — project discovery, fuzzy filtering and git summaries
//! - [`security`] — `tirith-core` integration and workspace guardrails
//! - [`cloud_hypervisor`] — typed Cloud Hypervisor REST client
//! - [`cloud_init`] — NoCloud seed for the official cloud images
//! - [`virtiofs`] — hardened `virtiofsd` command construction
//! - [`sandbox`] — MicroVM lifecycle orchestration
//! - [`relay`] — detached serial-console relay and persistent logging
//! - [`snapshot`] — on-disk snapshot metadata
//! - [`logs`] — private, persistent serial-console logs
//! - [`images`] — pinned upstream VM assets, downloads and checksum checks
//! - [`doctor`] — shared host prerequisite checks for CLI and TUI
//! - [`console`] — Cloud Hypervisor serial-console transport (no host fallback)
//! - [`attach_bar`] — bottom status bar and sequence-safe output gate for attach
//! - [`terminal`] — ANSI-aware scrollback buffer
//! - [`state`] — persisted recent-project state
//! - [`app`] — TUI application state and event handling
//! - [`ui`] — ratatui rendering
//! - [`tui`] — terminal lifecycle and event loop
//! - [`benchmark`] — measured host cost and disk usage
//! - [`cli`] — non-interactive command surface

pub mod app;
pub mod attach_bar;
pub mod benchmark;
pub mod cli;
pub mod cloud_hypervisor;
pub mod cloud_init;
pub mod config;
pub mod console;
pub mod doctor;
pub mod images;
pub mod logs;
pub mod project;
pub mod relay;
pub mod sandbox;
pub mod sandbox_config;
pub mod security;
pub mod snapshot;
pub mod state;
pub mod terminal;
pub mod tui;
pub mod ui;
pub mod virtiofs;

pub use app::App;
pub use config::{BackendKind, Config, Harness, RootfsFormat, SecurityMode};
