//! Persisted session metadata and the status report shown by the CLI/TUI.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Harness;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub project: PathBuf,
    /// Workspace exported to the guest. Empty in records from older versions.
    #[serde(default)]
    pub workspace: PathBuf,
    #[serde(default, alias = "sandbox")]
    pub instance: String,
    pub harness: Harness,
    pub started_at: DateTime<Utc>,
    pub vm_pid: u32,
    #[serde(default)]
    pub vm_binary: Option<PathBuf>,
    #[serde(default)]
    pub relay_pid: u32,
    pub virtiofs_pid: u32,
    /// Second virtiofsd for the profile/skill copy. Zero when the profile is
    /// empty or the VM predates profiles.
    #[serde(default)]
    pub profile_virtiofs_pid: u32,
    pub boot_args: String,
    /// Persistent serial log for this session. Optional so session records
    /// written by older hegn versions still deserialize.
    #[serde(default)]
    pub serial_log: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceReport {
    pub state: String,
    pub backend: String,
    pub instance: String,
    pub project: Option<PathBuf>,
    pub workspace: PathBuf,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub api_socket: PathBuf,
    pub serial_socket: PathBuf,
    pub console_socket: PathBuf,
    pub virtiofs_socket: PathBuf,
    pub virtiofs_running: bool,
    /// Profile/skill share directory exported to the guest, if any.
    pub profile_share: Option<PathBuf>,
    pub profile_virtiofs_pid: Option<u32>,
    pub profile_virtiofs_running: bool,
    pub relay_running: bool,
    pub vm_pid: Option<u32>,
    pub relay_pid: Option<u32>,
    pub virtiofs_pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    /// Persistent serial console log, if the session predates logging or the
    /// log has been removed.
    pub serial_log: Option<PathBuf>,
    /// Guest URLs of the configured local LLM endpoints.
    pub local_endpoints: Vec<String>,
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::instance::args::boot_args;
    use crate::vm::instance::test_support::{config_with_assets, manager};

    #[test]
    fn session_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_assets(dir.path());
        let manager = manager(config, dir.path());
        manager.paths().ensure().unwrap();
        let record = SessionRecord {
            project: dir.path().join("p"),
            workspace: dir.path().join("p"),
            instance: "demo".into(),
            harness: Harness::Codex,
            started_at: Utc::now(),
            vm_pid: 1234,
            vm_binary: None,
            relay_pid: 5678,
            virtiofs_pid: 4321,
            profile_virtiofs_pid: 0,
            boot_args: boot_args("/dev/vda1"),
            serial_log: None,
        };
        std::fs::write(
            manager.paths().session,
            serde_json::to_string_pretty(&record).unwrap(),
        )
        .unwrap();
        let loaded = manager.session().unwrap();
        assert_eq!(loaded.harness, Harness::Codex);
        assert_eq!(loaded.vm_pid, 1234);
        assert_eq!(loaded.relay_pid, 5678);
    }

    #[test]
    fn session_records_from_before_multi_instance_still_load() {
        let legacy = serde_json::json!({
            "project": "/projects/demo",
            "harness": "shell",
            "started_at": "2026-01-01T00:00:00Z",
            "vm_pid": 123,
            "relay_pid": 456,
            "virtiofs_pid": 789,
            "boot_args": "console=ttyS0",
        });
        let record: SessionRecord = serde_json::from_value(legacy).unwrap();
        assert!(record.workspace.as_os_str().is_empty());
        assert!(record.instance.is_empty());
        assert_eq!(record.vm_pid, 123);
    }
}
