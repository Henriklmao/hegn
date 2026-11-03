//! Detached serial-console relay.
//!
//! Cloud Hypervisor serves the guest serial port on a Unix socket and accepts
//! exactly one client at a time. hegn needs two things that a direct connection
//! cannot provide:
//!
//! 1. the serial log must keep growing while no terminal is attached, and
//! 2. `hegn attach` must be able to connect later without restarting the VM.
//!
//! A small, detached `hegn` process (`hegn __console-relay`) is therefore the
//! only client of the Cloud Hypervisor serial socket. It appends every byte to
//! the private session log and exposes a second socket for the TUI/CLI. On
//! connect it replays the tail of the log, then forwards input and live output.
//!
//! The relay has no network access, no shell and no project access. Its paths
//! come from the `0700` run directory; the frontend socket is created with mode
//! `0600` and is only removed when it is verifiably a socket.
//!
//! - [`args`] — internal argument parsing and relay process detection
//! - [`server`] — the relay loop, socket setup and log replay

mod args;
mod server;

/// Hidden first argument used to start the relay from [`crate::vm::instance`].
pub const INTERNAL_ARG: &str = "__console-relay";

pub use args::{RelayArgs, process_matches_relay, requested, run_from_current_args};
pub use server::{run, spawn};
