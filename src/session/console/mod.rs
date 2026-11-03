//! Cloud Hypervisor serial-console transport.
//!
//! Cloud Hypervisor exposes the guest serial port on a Unix socket. hegn's
//! detached relay is that socket's only client: it keeps the persistent log
//! growing and re-exposes the console on `run/console.sock` for the TUI and
//! `hegn attach`.
//!
//! There is deliberately no local-shell fallback: a console is only available
//! while a Cloud Hypervisor VM is actually running.
//!
//! - [`transport`] — the [`TerminalConsole`] trait used by the TUI and CLI
//! - [`vm_console`] — a console attached to one running Cloud Hypervisor VM
//! - [`attach`] — the interactive attach loop and terminal restoration

mod attach;
mod transport;
mod vm_console;

pub use attach::{run_interactive, run_interactive_with_command};
pub use transport::TerminalConsole;
pub use vm_console::VmConsole;
