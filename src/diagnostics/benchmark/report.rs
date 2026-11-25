//! The machine-readable benchmark report and its human-readable rendering.

use std::fmt::Write as _;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One host process that belongs to a running VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCost {
    /// `cloud-hypervisor`, `virtiofsd` or `console-relay`.
    pub role: String,
    pub pid: u32,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
}

/// Disk usage of one hegn directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskCost {
    pub name: String,
    pub path: PathBuf,
    pub exists: bool,
    /// Blocks actually allocated on disk (`st_blocks`, sparse-aware).
    pub allocated_bytes: u64,
    /// Sum of the file sizes, which can be larger for sparse files.
    pub apparent_bytes: u64,
    pub files: u64,
}

/// Disk usage of one active VM below `vms_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmDisk {
    pub name: String,
    pub path: PathBuf,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub files: u64,
}

/// Combined disk usage of v1 instance directories.
///
/// These are never active VMs in v2; the paths are reported so the user can
/// verify and remove them manually.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyInstanceDisk {
    pub paths: Vec<PathBuf>,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub files: u64,
}

/// The host cost of one VM, resolved from its `session.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCost {
    #[serde(rename = "vm", alias = "instance")]
    pub vm: String,
    pub project: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub harness: Option<String>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub guest_memory_bytes: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub uptime_secs: Option<u64>,
    pub processes: Vec<ProcessCost>,
}

/// Machine-readable result of `hegn benchmark`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BenchmarkReport {
    pub generated_at: DateTime<Utc>,
    pub sessions: Vec<SessionCost>,
    /// vCPUs of all measured VMs, or the configured default when none runs.
    pub vcpus: u32,
    /// Guest RAM of all measured VMs, or the configured default when none
    /// runs.
    pub guest_memory_bytes: u64,
    /// Resident memory of every measured session process right now.
    pub process_rss_bytes: u64,
    /// Whether this invocation booted a VM.
    pub started: bool,
    /// Present for `--clean`: what happened to the throwaway project.
    pub cleanup: Option<Cleanup>,
    pub snapshots: u64,
    /// `snapshots * memory_mib`; disk contents are not part of a snapshot.
    pub snapshot_estimate_bytes: u64,
    /// Active hegn directories (images, snapshots, VMs, logs, state).
    pub disk: Vec<DiskCost>,
    /// Allocated bytes across `disk`; not including legacy v1 directories.
    pub disk_allocated_bytes: u64,
    /// Per-VM usage below `vms_dir`, sorted by name.
    pub vm_disk: Vec<VmDisk>,
    /// Sum of `vm_disk`; legacy v1 directories are excluded.
    pub vm_disk_allocated_bytes: u64,
    /// Fingerprint-less v1 instance directories, reported separately.
    pub legacy_instances: Option<LegacyInstanceDisk>,
    pub notes: Vec<String>,
}

/// Outcome of removing a `--clean` project after measuring it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cleanup {
    pub project: PathBuf,
    #[serde(rename = "vm", alias = "instance")]
    pub vm: String,
    #[serde(rename = "vm_stopped", alias = "instance_stopped")]
    pub vm_stopped: bool,
    pub removed: bool,
}

/// Render bytes with binary units and one decimal place.
pub use crate::shared::text::human_bytes;

fn human_uptime(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m {}s", secs % 60)
    }
}

pub(super) fn print_report(report: &BenchmarkReport) {
    print!("{}", render_report(report));
}

fn render_report(report: &BenchmarkReport) -> String {
    let mut out = String::new();
    writeln!(out, "hegn benchmark").unwrap();

    if report.sessions.is_empty() {
        writeln!(out, "\nVM        none running").unwrap();
        writeln!(
            out,
            "Guest     {} at most ({} vCPU configured)",
            human_bytes(report.guest_memory_bytes),
            report.vcpus
        )
        .unwrap();
    }
    for session in &report.sessions {
        let harness = session.harness.as_deref().unwrap_or("unknown harness");
        writeln!(out, "\nVM        {} ({harness})", session.vm).unwrap();
        if let Some(project) = &session.project {
            writeln!(out, "Project   {}", project.display()).unwrap();
        }
        if let Some(workspace) = session
            .workspace
            .as_ref()
            .filter(|workspace| session.project.as_ref() != Some(workspace))
        {
            writeln!(out, "Workspace {}", workspace.display()).unwrap();
        }
        if let Some(uptime) = session.uptime_secs {
            writeln!(out, "Uptime    {}", human_uptime(uptime)).unwrap();
        }
        writeln!(
            out,
            "Guest     {} vCPU, {} configured (populated on demand)",
            session.vcpus,
            human_bytes(session.guest_memory_bytes)
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "{:<18} {:>7} {:>10} {:>10}",
            "PROCESS", "PID", "RSS", "VIRTUAL"
        )
        .unwrap();
        for process in &session.processes {
            writeln!(
                out,
                "{:<18} {:>7} {:>10} {:>10}",
                process.role,
                process.pid,
                human_bytes(process.rss_bytes),
                human_bytes(process.virtual_bytes)
            )
            .unwrap();
        }
    }

    if !report.sessions.is_empty() {
        writeln!(
            out,
            "\n{:<18} {:>7} {:>10} {:>10}",
            "host total",
            "",
            human_bytes(report.process_rss_bytes),
            ""
        )
        .unwrap();
    }

    if report.started {
        writeln!(out, "\nStarted   a VM for this measurement").unwrap();
    }
    if let Some(cleanup) = &report.cleanup {
        writeln!(
            out,
            "\nCleanup   {} [{}] (VM stopped: {}, project removed: {})",
            cleanup.project.display(),
            cleanup.vm,
            cleanup.vm_stopped,
            cleanup.removed
        )
        .unwrap();
    }

    writeln!(
        out,
        "\n{:<12} {:>11} {:>11} {:>7}  PATH",
        "DISK", "ALLOCATED", "APPARENT", "FILES"
    )
    .unwrap();
    for cost in &report.disk {
        if !cost.exists {
            writeln!(
                out,
                "{:<12} {:>11} {:>11} {:>7}  {}",
                cost.name,
                "-",
                "-",
                "-",
                cost.path.display()
            )
            .unwrap();
            continue;
        }
        writeln!(
            out,
            "{:<12} {:>11} {:>11} {:>7}  {}",
            cost.name,
            human_bytes(cost.allocated_bytes),
            human_bytes(cost.apparent_bytes),
            cost.files,
            cost.path.display()
        )
        .unwrap();
        if cost.name == "VMs" {
            for vm in &report.vm_disk {
                writeln!(
                    out,
                    "  {:<10} {:>11} {:>11} {:>7}  {}",
                    vm.name,
                    human_bytes(vm.allocated_bytes),
                    human_bytes(vm.apparent_bytes),
                    vm.files,
                    vm.path.display()
                )
                .unwrap();
            }
        }
    }
    if let Some(legacy) = &report.legacy_instances {
        let paths = legacy
            .paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            out,
            "{:<12} {:>11} {:>11} {:>7}  {paths}",
            "legacy v1",
            human_bytes(legacy.allocated_bytes),
            human_bytes(legacy.apparent_bytes),
            legacy.files
        )
        .unwrap();
    }
    writeln!(
        out,
        "{:<12} {:>11}",
        "disk total",
        human_bytes(report.disk_allocated_bytes)
    )
    .unwrap();

    if report.snapshots > 0 {
        writeln!(
            out,
            "\nSnapshots  {} (up to {} if every guest page is touched)",
            report.snapshots,
            human_bytes(report.snapshot_estimate_bytes)
        )
        .unwrap();
    }

    if !report.notes.is_empty() {
        writeln!(out, "\nNotes:").unwrap();
        for note in &report.notes {
            writeln!(out, "- {note}").unwrap();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::benchmark::MIB;

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(2048 + 512), "2.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * MIB), "3.0 GiB");
    }

    #[test]
    fn human_uptime_stays_readable() {
        assert_eq!(human_uptime(42), "0m 42s");
        assert_eq!(human_uptime(90), "1m 30s");
        assert_eq!(human_uptime(3 * 3600 + 5 * 60), "3h 5m");
    }

    #[test]
    fn report_serialises_to_json() {
        let report = BenchmarkReport {
            generated_at: Utc::now(),
            sessions: Vec::new(),
            vcpus: 2,
            guest_memory_bytes: 2048 * MIB,
            process_rss_bytes: 0,
            started: false,
            cleanup: None,
            snapshots: 0,
            snapshot_estimate_bytes: 0,
            disk: Vec::new(),
            disk_allocated_bytes: 0,
            vm_disk: Vec::new(),
            vm_disk_allocated_bytes: 0,
            legacy_instances: None,
            notes: Vec::new(),
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["guest_memory_bytes"], 2048 * MIB);
        assert_eq!(json["started"], false);
        assert!(json["cleanup"].is_null());
        assert_eq!(json["sessions"].as_array().unwrap().len(), 0);
        assert_eq!(json["vm_disk"].as_array().unwrap().len(), 0);
        assert!(json["legacy_instances"].is_null());
    }

    #[test]
    fn human_bytes_handles_every_unit_boundary() {
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024 - 1), "1024.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(1024u64.pow(4)), "1.0 TiB");
        // Above TiB the value keeps growing in TiB instead of overflowing.
        assert_eq!(human_bytes(2 * 1024u64.pow(4)), "2.0 TiB");
    }

    #[test]
    fn human_uptime_covers_seconds_minutes_and_days() {
        assert_eq!(human_uptime(0), "0m 0s");
        assert_eq!(human_uptime(59), "0m 59s");
        assert_eq!(human_uptime(60), "1m 0s");
        assert_eq!(human_uptime(3599), "59m 59s");
        assert_eq!(human_uptime(3600), "1h 0m");
        assert_eq!(human_uptime(24 * 3600 + 60), "24h 1m");
    }

    #[test]
    fn report_serialises_nested_session_disk_and_cleanup_fields() {
        let report = BenchmarkReport {
            generated_at: Utc::now(),
            sessions: vec![SessionCost {
                vm: "demo".into(),
                project: Some(PathBuf::from("/workspace/demo")),
                workspace: None,
                harness: Some("OpenCode".into()),
                vcpus: 2,
                memory_mib: 1024,
                guest_memory_bytes: 1024 * MIB,
                started_at: None,
                uptime_secs: Some(90),
                processes: vec![ProcessCost {
                    role: "virtiofsd".into(),
                    pid: 7,
                    rss_bytes: 4096,
                    virtual_bytes: 8192,
                }],
            }],
            vcpus: 2,
            guest_memory_bytes: 1024 * MIB,
            process_rss_bytes: 4096,
            started: true,
            cleanup: Some(Cleanup {
                project: PathBuf::from("/workspace/hegn-benchmark-1"),
                vm: "bench".into(),
                vm_stopped: true,
                removed: true,
            }),
            snapshots: 1,
            snapshot_estimate_bytes: 1024 * MIB,
            disk: vec![DiskCost {
                name: "logs".into(),
                path: PathBuf::from("/state/logs"),
                exists: true,
                allocated_bytes: 4096,
                apparent_bytes: 2048,
                files: 1,
            }],
            disk_allocated_bytes: 4096,
            vm_disk: vec![VmDisk {
                name: "demo".into(),
                path: PathBuf::from("/vms/demo"),
                allocated_bytes: 8192,
                apparent_bytes: 4096,
                files: 2,
            }],
            vm_disk_allocated_bytes: 8192,
            legacy_instances: Some(LegacyInstanceDisk {
                paths: vec![PathBuf::from("/legacy/instances")],
                allocated_bytes: 2048,
                apparent_bytes: 1024,
                files: 1,
            }),
            notes: vec!["measured".into()],
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["sessions"][0]["vm"], "demo");
        assert_eq!(json["sessions"][0]["processes"][0]["role"], "virtiofsd");
        assert_eq!(json["sessions"][0]["uptime_secs"], 90);
        assert_eq!(json["cleanup"]["removed"], true);
        assert_eq!(json["cleanup"]["vm"], "bench");
        assert_eq!(json["cleanup"]["vm_stopped"], true);
        assert_eq!(json["disk"][0]["allocated_bytes"], 4096);
        assert_eq!(json["vm_disk"][0]["name"], "demo");
        assert_eq!(json["vm_disk_allocated_bytes"], 8192);
        assert_eq!(json["legacy_instances"]["allocated_bytes"], 2048);
        assert_eq!(json["legacy_instances"]["paths"][0], "/legacy/instances");
        assert_eq!(json["notes"][0], "measured");
    }

    #[test]
    fn legacy_instance_json_aliases_still_load() {
        let cleanup: Cleanup = serde_json::from_value(serde_json::json!({
            "project": "/workspace/hegn-benchmark-1",
            "instance": "bench",
            "instance_stopped": true,
            "removed": true,
        }))
        .unwrap();
        assert_eq!(cleanup.vm, "bench");
        assert!(cleanup.vm_stopped);

        let session: SessionCost = serde_json::from_value(serde_json::json!({
            "instance": "demo",
            "project": "/workspace/demo",
            "workspace": null,
            "harness": "OpenCode",
            "vcpus": 2,
            "memory_mib": 1024,
            "guest_memory_bytes": 1024 * MIB,
            "started_at": null,
            "uptime_secs": 60,
            "processes": [],
        }))
        .unwrap();
        assert_eq!(session.vm, "demo");
    }

    #[test]
    fn report_renders_vm_terminology_and_legacy_disk_lines() {
        let report = BenchmarkReport {
            generated_at: Utc::now(),
            sessions: vec![SessionCost {
                vm: "demo".into(),
                project: Some(PathBuf::from("/workspace/demo")),
                workspace: None,
                harness: Some("OpenCode".into()),
                vcpus: 2,
                memory_mib: 1024,
                guest_memory_bytes: 1024 * MIB,
                started_at: None,
                uptime_secs: Some(60),
                processes: Vec::new(),
            }],
            vcpus: 2,
            guest_memory_bytes: 1024 * MIB,
            process_rss_bytes: 0,
            started: true,
            cleanup: None,
            snapshots: 0,
            snapshot_estimate_bytes: 0,
            disk: vec![DiskCost {
                name: "VMs".into(),
                path: PathBuf::from("/data/vms"),
                exists: true,
                allocated_bytes: 8192,
                apparent_bytes: 4096,
                files: 2,
            }],
            disk_allocated_bytes: 8192,
            vm_disk: vec![VmDisk {
                name: "demo".into(),
                path: PathBuf::from("/data/vms/demo"),
                allocated_bytes: 8192,
                apparent_bytes: 4096,
                files: 2,
            }],
            vm_disk_allocated_bytes: 8192,
            legacy_instances: Some(LegacyInstanceDisk {
                paths: vec![
                    PathBuf::from("/data/instances"),
                    PathBuf::from("/data/sandboxes"),
                ],
                allocated_bytes: 4096,
                apparent_bytes: 2048,
                files: 2,
            }),
            notes: vec![
                "legacy v1 instance directories are not active VMs and are not included in the \
                 disk total"
                    .into(),
            ],
        };

        let text = render_report(&report);
        assert!(text.contains("VM        demo (OpenCode)"), "{text}");
        assert!(!text.contains("Session"), "{text}");
        assert!(text.contains("VMs"), "{text}");
        assert!(text.contains("  demo"), "{text}");
        assert!(text.contains("legacy v1"), "{text}");
        assert!(text.contains("/data/instances, /data/sandboxes"), "{text}");
        assert!(
            text.contains("not active VMs") && text.contains("not included"),
            "{text}"
        );
        assert!(text.contains("disk total"), "{text}");

        let mut without_legacy = report.clone();
        without_legacy.legacy_instances = None;
        without_legacy.notes.clear();
        let text = render_report(&without_legacy);
        assert!(!text.contains("legacy v1"), "{text}");
    }
}
