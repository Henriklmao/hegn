//! Worker messages sent from the background threads back to the event loop.
//!
//! The TUI never blocks on long-running host work. Each job in [`super::jobs`]
//! owns one channel sender and reports progress by sending these messages, which
//! [`super::event_loop`] drains once per iteration.

use crate::agent::provision::ProvisionProgress;
use crate::config::Harness;
use crate::diagnostics::doctor::DoctorReport;
use crate::project;
use crate::session::console::TerminalConsole;
use crate::tui::app::ToastLevel;

pub(super) enum ScanMessage {
    Projects(Vec<project::Project>),
    Started {
        console: Box<dyn TerminalConsole>,
        branch: Option<Result<String, String>>,
        harness: Harness,
        /// True when this action booted the VM instead of only attaching.
        started: bool,
    },
    StartFailed(String),
    InstancesChanged {
        note: String,
        level: ToastLevel,
    },
    InstanceError(String),
    Doctor {
        report: DoctorReport,
        note: Option<String>,
    },
    /// Byte progress for an in-flight image download.
    DownloadProgress(crate::vm::images::DownloadProgress),
    /// Live first-boot harness provisioning progress.
    Provision(ProvisionProgress),
    ProvisionReady,
    ProvisionFailed(String),
    Error(String),
}
