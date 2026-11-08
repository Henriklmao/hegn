//! On-disk snapshot metadata.
//!
//! A snapshot is a directory written by Cloud Hypervisor:
//!
//! ```text
//! ~/.local/share/hegn/snapshots/
//!   .staging/<name>/   # the only path the VMM may write
//!   <name>/
//!     snapshot.json    # hegn metadata rendered by the TUI
//!     config.json      # Cloud Hypervisor VM configuration
//!     state.json       # device and CPU state
//!     memory-ranges    # guest memory
//! ```
//!
//! Finished snapshots are renamed out of `.staging` before metadata is saved,
//! so the VMM's Landlock rule never covers completed checkpoints.
//!
//! `model` holds [`SnapshotMeta`], [`SnapshotPaths`] and name validation;
//! `store` owns staging, finalizing, persistence and listing.

mod model;
mod store;

pub use model::{AUTOSAVE_NOTE, SnapshotMeta, SnapshotPaths, sanitize_name};
pub use store::SnapshotStore;
