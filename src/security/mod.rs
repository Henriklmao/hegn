//! `tirith-core` integration and hegn-specific workspace guardrails.
//!
//! Commands typed in the terminal view are assembled into a line, inspected
//! here *before* they are written to the guest serial port, and either allowed,
//! shown as a warning, or blocked. The TUI can then offer a one-key git
//! checkpoint.
//!
//! - [`severity`] — UI-facing severity levels
//! - [`engine`] — command inspection and the resulting decisions
//! - [`risk`] — hegn's own destructive-workspace guardrails

mod engine;
mod risk;
mod severity;

pub use engine::{Decision, Finding, Inspection, SecurityEngine};
pub use risk::{WorkspaceRisk, detect_workspace_risk};
pub use severity::Severity;
