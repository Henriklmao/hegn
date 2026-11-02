//! Host diagnostics and measured resource usage.
//!
//! - [`doctor`] — shared host prerequisite checks for CLI and TUI
//! - [`benchmark`] — measured host cost and disk usage

pub mod benchmark;
pub mod doctor;
