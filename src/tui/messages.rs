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
    VmsChanged {
        note: String,
        level: ToastLevel,
    },
    /// A project was assigned to `vm`; its change needs `hegn vm apply`.
    ProjectAssigned {
        vm: String,
        note: String,
        /// True when the VM had no projects before this assignment.
        first: bool,
    },
    /// `hegn vm apply` finished and `vm` is running the current config.
    VmApplied {
        vm: String,
        note: String,
    },
    /// `vm` stopped; any first-boot provisioning progress is stale now.
    VmStopped {
        vm: String,
        note: String,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::provision::ProvisionPlan;
    use std::path::{Path, PathBuf};

    struct FakeConsole {
        project: PathBuf,
    }

    impl TerminalConsole for FakeConsole {
        fn send(&mut self, _bytes: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }

        fn drain(&mut self) -> Vec<Vec<u8>> {
            Vec::new()
        }

        fn is_exited(&mut self) -> bool {
            false
        }

        fn project(&self) -> &Path {
            &self.project
        }

        fn label(&self) -> &str {
            "fake · serial"
        }

        fn vm_name(&self) -> &str {
            "fake"
        }
    }

    #[test]
    fn every_scan_message_variant_can_be_constructed() {
        let plan = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let messages = vec![
            ScanMessage::Projects(vec![project::Project {
                name: "demo".into(),
                path: PathBuf::from("/projects/demo"),
                git: project::GitSummary::default(),
                last_opened: None,
            }]),
            ScanMessage::Started {
                console: Box::new(FakeConsole {
                    project: PathBuf::from("/projects/demo"),
                }),
                branch: Some(Ok("hegn/session-1".into())),
                harness: Harness::Opencode,
                started: true,
            },
            ScanMessage::StartFailed("missing kernel".into()),
            ScanMessage::VmsChanged {
                note: "VMs refreshed".into(),
                level: ToastLevel::Warn,
            },
            ScanMessage::ProjectAssigned {
                vm: "rust".into(),
                note: "assigned demo to VM rust".into(),
                first: false,
            },
            ScanMessage::VmApplied {
                vm: "rust".into(),
                note: "VM rust restarted with the current config".into(),
            },
            ScanMessage::VmStopped {
                vm: "rust".into(),
                note: "VM rust stopped".into(),
            },
            ScanMessage::Error("unreadable state".into()),
            ScanMessage::Doctor {
                report: DoctorReport {
                    ok: true,
                    checks: Vec::new(),
                    assets: Vec::new(),
                    setup: crate::diagnostics::doctor::SetupActions::default(),
                },
                note: Some("all checks passed".into()),
            },
            ScanMessage::DownloadProgress(crate::vm::images::DownloadProgress {
                asset: "kernel",
                received: 512,
                total: Some(1024),
            }),
            ScanMessage::Provision(ProvisionProgress::new(&plan)),
            ScanMessage::ProvisionReady,
            ScanMessage::ProvisionFailed("no network".into()),
            ScanMessage::Error("scan failed".into()),
        ];
        assert_eq!(messages.len(), 14);

        if let ScanMessage::Started {
            console,
            branch,
            harness,
            started,
        } = &messages[1]
        {
            assert_eq!(console.vm_name(), "fake");
            assert_eq!(console.label(), "fake · serial");
            assert!(matches!(branch, Some(Ok(name)) if name == "hegn/session-1"));
            assert_eq!(*harness, Harness::Opencode);
            assert!(*started);
        } else {
            panic!("unexpected second message");
        }

        let variants = [
            matches!(&messages[0], ScanMessage::Projects(_)),
            matches!(&messages[1], ScanMessage::Started { .. }),
            matches!(&messages[2], ScanMessage::StartFailed(_)),
            matches!(&messages[3], ScanMessage::VmsChanged { .. }),
            matches!(&messages[4], ScanMessage::ProjectAssigned { .. }),
            matches!(&messages[5], ScanMessage::VmApplied { .. }),
            matches!(&messages[6], ScanMessage::VmStopped { .. }),
            matches!(&messages[7], ScanMessage::Error(_)),
            matches!(&messages[8], ScanMessage::Doctor { .. }),
            matches!(&messages[9], ScanMessage::DownloadProgress(_)),
            matches!(&messages[10], ScanMessage::Provision(_)),
            matches!(&messages[11], ScanMessage::ProvisionReady),
            matches!(&messages[12], ScanMessage::ProvisionFailed(_)),
            matches!(&messages[13], ScanMessage::Error(_)),
        ];
        assert!(variants.iter().all(|matched| *matched));
    }
}
