//! hegn — Cloud Hypervisor instance manager for agentic coding workflows.
//!
//! The crate is organised by domain. Every domain owns one directory and
//! re-exports its public surface from its `mod.rs`:
//!
//! - [`shared`] — dependency-free helpers for files, processes, paths and text
//! - [`config`] — user configuration, validation and the settings form
//! - [`project`] — project discovery, metadata, git summaries and lifecycle
//! - [`security`] — `tirith-core` integration and workspace guardrails
//! - [`network`] — host-enforced guarded networking for guests
//! - [`vm`] — Cloud Hypervisor, virtio-fs, images, cloud-init and VM lifecycle
//! - [`agent`] — launch commands, harness profiles and first-boot provisioning
//! - [`session`] — serial transport, relay, logs and persisted session state
//! - [`diagnostics`] — host prerequisite checks and measured resource usage
//! - [`cli`] — non-interactive command surface
//! - [`tui`] — application state, rendering and the terminal event loop

pub mod agent;
pub mod cli;
pub mod config;
pub mod diagnostics;
pub mod network;
pub mod project;
pub mod security;
pub mod session;
pub mod shared;
pub mod tui;
pub mod vm;

pub use config::{BackendKind, Config, Harness, NetworkMode, RootfsFormat, SecurityMode};
pub use tui::App;
