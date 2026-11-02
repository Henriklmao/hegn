//! Modular launch integrations for the agent or maintenance command.
//!
//! The embedded project terminal is a scrollback, not a full-screen terminal
//! emulator. Agent TUIs therefore work best in a real terminal: hegn can hand
//! the serial console to `tmux`, `herdr`, or the current terminal, or keep the
//! command inside the embedded TUI terminal.
//!
//! The guest command itself is always executed inside the VM. Host-side
//! integrations only decide where the serial console is displayed; they never
//! run an agent on the host.
//!
//! Adding an integration means adding an [`Integration`] variant, mapping it to
//! a [`LaunchAction`] in [`action`], and executing that action in the TUI/CLI.
//! The command, configuration and validation stay shared.

mod command;
mod config;
mod integration;

pub use command::{LaunchSpec, resolve};
pub use config::{HARNESS, LaunchCommand, LaunchConfig, SHELL};
pub use integration::{Integration, LaunchAction, action, attach_shell_command, spawn_herdr};
