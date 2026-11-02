//! The machine-readable benchmark report and its human-readable rendering.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// One host process that belongs to a running instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessCost {
    /// `cloud-hypervisor`, `virtiofsd` or `console-relay`.
    pub role: String,
    pub pid: u32,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
}

/// Disk usage of one hegn directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// The host cost of one instance, resolved from its `session.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCost {
    pub instance: String,
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
    /// vCPUs of all measured instances, or the configured default when none
    /// runs.
    pub vcpus: u32,
    /// Guest RAM of all measured instances, or the configured default when
    /// none runs.
    pub guest_memory_bytes: u64,
    /// Resident memory of every measured session process right now.
    pub process_rss_bytes: u64,
    /// Whether this invocation booted an instance.
    pub started: bool,
    /// Present for `--clean`: what happened to the throwaway project.
    pub cleanup: Option<Cleanup>,
    pub snapshots: u64,
    /// `snapshots * memory_mib`; disk contents are not part of a snapshot.
    pub snapshot_estimate_bytes: u64,
    pub disk: Vec<DiskCost>,
    pub disk_allocated_bytes: u64,
    pub notes: Vec<String>,
}

/// Outcome of removing a `--clean` project after measuring it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cleanup {
    pub project: PathBuf,
    pub instance: String,
    pub instance_stopped: bool,
    pub removed: bool,
}

/// Render bytes with binary units and one decimal place.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

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
    println!("hegn benchmark");

    if report.sessions.is_empty() {
        println!("\nSession   none running");
        println!(
            "Guest     {} at most ({} vCPU configured)",
            human_bytes(report.guest_memory_bytes),
            report.vcpus
        );
    }
    for session in &report.sessions {
        let harness = session.harness.as_deref().unwrap_or("unknown harness");
        println!("\nSession   {} ({harness})", session.instance);
        if let Some(project) = &session.project {
            println!("Project   {}", project.display());
        }
        if let Some(workspace) = session
            .workspace
            .as_ref()
            .filter(|workspace| session.project.as_ref() != Some(workspace))
        {
            println!("Workspace {}", workspace.display());
        }
        if let Some(uptime) = session.uptime_secs {
            println!("Uptime    {}", human_uptime(uptime));
        }
        println!(
            "Guest     {} vCPU, {} configured (populated on demand)",
            session.vcpus,
            human_bytes(session.guest_memory_bytes)
        );
        println!();
        println!(
            "{:<18} {:>7} {:>10} {:>10}",
            "PROCESS", "PID", "RSS", "VIRTUAL"
        );
        for process in &session.processes {
            println!(
                "{:<18} {:>7} {:>10} {:>10}",
                process.role,
                process.pid,
                human_bytes(process.rss_bytes),
                human_bytes(process.virtual_bytes)
            );
        }
    }

    if !report.sessions.is_empty() {
        println!(
            "\n{:<18} {:>7} {:>10} {:>10}",
            "host total",
            "",
            human_bytes(report.process_rss_bytes),
            ""
        );
    }

    if report.started {
        println!("\nStarted   an instance for this measurement");
    }
    if let Some(cleanup) = &report.cleanup {
        println!(
            "\nCleanup   {} [{}] (instance stopped: {}, project removed: {})",
            cleanup.project.display(),
            cleanup.instance,
            cleanup.instance_stopped,
            cleanup.removed
        );
    }

    println!(
        "\n{:<12} {:>11} {:>11} {:>7}  PATH",
        "DISK", "ALLOCATED", "APPARENT", "FILES"
    );
    for cost in &report.disk {
        if !cost.exists {
            println!(
                "{:<12} {:>11} {:>11} {:>7}  {}",
                cost.name,
                "-",
                "-",
                "-",
                cost.path.display()
            );
            continue;
        }
        println!(
            "{:<12} {:>11} {:>11} {:>7}  {}",
            cost.name,
            human_bytes(cost.allocated_bytes),
            human_bytes(cost.apparent_bytes),
            cost.files,
            cost.path.display()
        );
    }
    println!(
        "{:<12} {:>11}",
        "disk total",
        human_bytes(report.disk_allocated_bytes)
    );

    if report.snapshots > 0 {
        println!(
            "\nSnapshots  {} (up to {} if every guest page is touched)",
            report.snapshots,
            human_bytes(report.snapshot_estimate_bytes)
        );
    }

    if !report.notes.is_empty() {
        println!("\nNotes:");
        for note in &report.notes {
            println!("- {note}");
        }
    }
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
            notes: Vec::new(),
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["guest_memory_bytes"], 2048 * MIB);
        assert_eq!(json["started"], false);
        assert!(json["cleanup"].is_null());
        assert_eq!(json["sessions"].as_array().unwrap().len(), 0);
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
                instance: "demo".into(),
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
                instance: "bench".into(),
                instance_stopped: true,
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
            notes: vec!["measured".into()],
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["sessions"][0]["instance"], "demo");
        assert_eq!(json["sessions"][0]["processes"][0]["role"], "virtiofsd");
        assert_eq!(json["sessions"][0]["uptime_secs"], 90);
        assert_eq!(json["cleanup"]["removed"], true);
        assert_eq!(json["cleanup"]["instance_stopped"], true);
        assert_eq!(json["disk"][0]["allocated_bytes"], 4096);
        assert_eq!(json["notes"][0], "measured");
    }
}
