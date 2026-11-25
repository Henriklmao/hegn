//! Persisted session metadata and the status report shown by the CLI/TUI.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Harness;

use super::membership::MembershipMember;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Primary project exported by the VM. Empty if the VM had no members,
    /// which cannot happen for a successful boot.
    #[serde(default)]
    pub project: PathBuf,
    /// Compatibility alias of `project` used by older hegn versions.
    #[serde(default)]
    pub workspace: PathBuf,
    #[serde(default, alias = "instance")]
    pub vm: String,
    pub harness: Harness,
    pub started_at: DateTime<Utc>,
    pub vm_pid: u32,
    #[serde(default)]
    pub vm_binary: Option<PathBuf>,
    #[serde(default)]
    pub relay_pid: u32,
    /// One virtiofsd per exported member, in boot-plan order. Older records
    /// have a single pid and an empty list.
    #[serde(default)]
    pub virtiofs_pids: Vec<u32>,
    /// Second virtiofsd for the profile/skill copy. Zero when the profile is
    /// empty or the VM predates profiles.
    #[serde(default)]
    pub profile_virtiofs_pid: u32,
    pub boot_args: String,
    /// Persistent serial log for this session. Optional so session records
    /// written by older hegn versions still deserialize.
    #[serde(default)]
    pub serial_log: Option<PathBuf>,
    /// Membership summary captured at boot.
    #[serde(default)]
    pub membership: Vec<MembershipMember>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmReport {
    pub state: String,
    pub backend: String,
    pub vm: String,
    pub primary: Option<String>,
    /// Exported project members.
    pub members: usize,
    pub project: Option<PathBuf>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub api_socket: PathBuf,
    pub serial_socket: PathBuf,
    pub console_socket: PathBuf,
    pub virtiofs_running: bool,
    /// Pids of every member virtiofsd, in boot-plan order.
    pub virtiofs_pids: Vec<u32>,
    /// Profile/skill share directory exported to the guest, if any.
    pub profile_share: Option<PathBuf>,
    pub profile_virtiofs_pid: Option<u32>,
    pub profile_virtiofs_running: bool,
    pub relay_running: bool,
    pub vm_pid: Option<u32>,
    pub relay_pid: Option<u32>,
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
    use crate::project::ProjectKind;
    use crate::vm::manager::args::boot_args;

    fn record() -> SessionRecord {
        SessionRecord {
            project: PathBuf::from("/projects/parser"),
            workspace: PathBuf::from("/projects/parser"),
            vm: "rust".into(),
            harness: Harness::Codex,
            started_at: Utc::now(),
            vm_pid: 1234,
            vm_binary: None,
            relay_pid: 5678,
            virtiofs_pids: vec![11, 12],
            profile_virtiofs_pid: 0,
            boot_args: boot_args("/dev/vda1"),
            serial_log: None,
            membership: vec![MembershipMember {
                name: "parser".into(),
                path: PathBuf::from("/projects/parser"),
                tag: "hegn-prj-0".into(),
                guest_path: "/projects/parser".into(),
                read_only: false,
                kind: ProjectKind::External,
            }],
        }
    }

    #[test]
    fn session_record_round_trips() {
        let json = serde_json::to_string_pretty(&record()).unwrap();
        let loaded: SessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.vm, "rust");
        assert_eq!(loaded.harness, Harness::Codex);
        assert_eq!(loaded.vm_pid, 1234);
        assert_eq!(loaded.relay_pid, 5678);
        assert_eq!(loaded.virtiofs_pids, vec![11, 12]);
        assert_eq!(loaded.membership.len(), 1);
        assert!(json.contains("\"vm\": \"rust\""), "{json}");
        assert!(!json.contains("\"instance\""), "{json}");
    }

    #[test]
    fn legacy_session_records_still_load() {
        let legacy = serde_json::json!({
            "project": "/projects/demo",
            "instance": "demo",
            "harness": "shell",
            "started_at": "2026-01-01T00:00:00Z",
            "vm_pid": 123,
            "relay_pid": 456,
            "virtiofs_pid": 789,
            "boot_args": "console=ttyS0",
        });
        let record: SessionRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(record.vm, "demo", "the legacy instance field is an alias");
        assert!(record.workspace.as_os_str().is_empty());
        assert!(record.virtiofs_pids.is_empty());
        assert_eq!(record.vm_pid, 123);
    }
}
