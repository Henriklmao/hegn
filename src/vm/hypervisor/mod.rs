//! Typed client for the Cloud Hypervisor REST API over a Unix domain socket.
//!
//! Cloud Hypervisor exposes an HTTP/1.1 API under `/api/v1`. hegn boots the VM
//! through the command line (so the full device configuration stays in one
//! auditable place) and uses this client only for the operations that need a
//! running VMM: status, pause/resume and snapshots.
//!
//! Every request is a single request/response exchange. Responses are framed by
//! `Content-Length` (or chunked encoding) so a VMM that keeps the connection
//! open cannot make hegn wait for EOF.

mod client;
mod protocol;
mod types;

pub use client::{CloudHypervisorClient, is_running, validate_socket_path};
pub use protocol::{API_PREFIX, Error};
pub use types::{SnapshotConfig, VmInfo, VmState};
