//! JSON payload types reported by the Cloud Hypervisor REST API.
//!
//! These mirror the bodies exchanged with `/api/v1/vm.info` and
//! `/api/v1/vm.snapshot`, independently of the HTTP transport.

use serde::{Deserialize, Serialize};

/// VM state as reported by `GET /api/v1/vm.info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VmState {
    #[default]
    Created,
    Running,
    Shutdown,
    Paused,
    BreakPoint,
}

impl VmState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Shutdown => "stopped",
            Self::Paused => "paused",
            Self::BreakPoint => "paused",
        }
    }

    pub fn is_alive(self) -> bool {
        matches!(self, Self::Running | Self::Paused | Self::BreakPoint)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct VmInfo {
    pub state: VmState,
    #[serde(default)]
    pub memory_actual_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    /// A `file://` URL pointing at an existing directory. Cloud Hypervisor
    /// writes `config.json`, `state.json` and `memory-ranges` into it.
    pub destination_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn maps_every_vm_state() {
        for (raw, expected) in [
            ("Created", VmState::Created),
            ("Running", VmState::Running),
            ("Shutdown", VmState::Shutdown),
            ("Paused", VmState::Paused),
            ("BreakPoint", VmState::BreakPoint),
        ] {
            let json = format!(r#"{{"state":"{raw}"}}"#);
            let info: VmInfo = serde_json::from_str(&json).unwrap();
            assert_eq!(info.state, expected);
        }
    }
}
