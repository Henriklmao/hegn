//! The console abstraction shared by the TUI and the attach loop.

use std::path::Path;

use anyhow::Result;

/// Transport used by the terminal view. Implementations must never pretend to
/// be an instance when no VM is running.
pub trait TerminalConsole: Send {
    /// Send bytes to the guest serial port.
    fn send(&mut self, bytes: &[u8]) -> Result<()>;
    /// Drain newly available serial output.
    fn drain(&mut self) -> Vec<Vec<u8>>;
    /// Whether the backing VM has exited.
    fn is_exited(&mut self) -> bool;
    fn project(&self) -> &Path;
    fn label(&self) -> &str;
    fn instance_name(&self) -> &str;
}
