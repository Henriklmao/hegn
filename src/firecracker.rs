//! Typed client for the Firecracker REST API over a Unix domain socket.
//!
//! Firecracker exposes a small HTTP/1.1 API. Rather than pulling in a full HTTP
//! stack we speak the protocol directly: every call is a single request with
//! `Connection: close`, which keeps the client easy to test and audit.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Firecracker responses are tiny; bound reads so a stuck or malicious socket
/// peer cannot make hegn buffer unbounded data in memory.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error("firecracker io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("firecracker returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("invalid firecracker response: {0}")]
    Protocol(String),
    #[error("invalid firecracker JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("firecracker request timed out after {0:?}")]
    Timeout(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum VmState {
    #[default]
    #[serde(alias = "Not started")]
    NotStarted,
    Starting,
    Running,
    Paused,
}

impl VmState {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotStarted => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Paused => "paused",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    pub boot_args: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smt: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VhostUserFs {
    pub fs_id: String,
    pub socket: String,
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub host_dev_name: String,
    pub guest_mac: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotCreate {
    pub snapshot_path: String,
    pub mem_file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotLoad {
    pub snapshot_path: String,
    pub mem_file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_vm: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstanceInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub state: VmState,
    #[serde(default)]
    pub vmm_version: String,
    #[serde(default)]
    pub app_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    InstanceStart,
    SendCtrlAltDel,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::InstanceStart => "InstanceStart",
            Self::SendCtrlAltDel => "SendCtrlAltDel",
        }
    }
}

/// A minimal Firecracker API client.
#[derive(Debug, Clone)]
pub struct FirecrackerClient {
    socket_path: PathBuf,
}

impl FirecrackerClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn put_machine_config(&self, config: &MachineConfig) -> Result<()> {
        self.put("/machine-config", config).await
    }

    pub async fn put_boot_source(&self, source: &BootSource) -> Result<()> {
        self.put("/boot-source", source).await
    }

    pub async fn put_drive(&self, drive: &Drive) -> Result<()> {
        // Firecracker addresses resources by ID in the path.
        self.put(&format!("/drives/{}", path_id(&drive.drive_id)?), drive)
            .await
    }

    pub async fn put_vhost_user_fs(&self, fs: &VhostUserFs) -> Result<()> {
        let id = path_id(&fs.fs_id)?;
        match self.put(&format!("/vhost-user-fs/{id}"), fs).await {
            Ok(()) => Ok(()),
            Err(err) if unsupported_path(&err) => {
                // Some virtio-fs-enabled Firecracker forks expose a
                // collection endpoint instead of `/vhost-user-fs/{id}`.
                self.put("/vhost-user-fs", fs).await.with_context(|| {
                    "this Firecracker build does not expose virtio-fs; hegn needs a \
                     virtio-fs-enabled Firecracker build (upstream Firecracker has no \
                     virtio-fs device)"
                })
            }
            Err(err) => Err(err),
        }
    }

    pub async fn put_network_interface(&self, nic: &NetworkInterface) -> Result<()> {
        self.put(
            &format!("/network-interfaces/{}", path_id(&nic.iface_id)?),
            nic,
        )
        .await
    }

    /// `PUT /actions` with `{"action_type": "..."}`.
    pub async fn action(&self, action: Action) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            action_type: &'a str,
        }
        self.put(
            "/actions",
            &Body {
                action_type: action.as_str(),
            },
        )
        .await
    }

    /// `PATCH /vm` with `{"state": "Paused"}`.
    pub async fn pause(&self) -> Result<()> {
        self.set_vm_state("Paused").await
    }

    /// `PATCH /vm` with `{"state": "Resumed"}`.
    pub async fn resume(&self) -> Result<()> {
        self.set_vm_state("Resumed").await
    }

    async fn set_vm_state(&self, state: &str) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            state: &'a str,
        }
        self.patch("/vm", &Body { state }).await
    }

    pub async fn instance_info(&self) -> Result<InstanceInfo> {
        self.get("/").await
    }

    /// Create a full snapshot. The VM should be paused first.
    pub async fn create_snapshot(&self, spec: &SnapshotCreate) -> Result<()> {
        self.put("/snapshot/create", spec).await
    }

    /// Load a snapshot into a freshly started Firecracker process.
    pub async fn load_snapshot(&self, spec: &SnapshotLoad) -> Result<()> {
        self.put("/snapshot/load", spec).await
    }

    async fn put<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        self.send_json("PUT", path, body).await
    }

    async fn patch<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        self.send_json("PATCH", path, body).await
    }

    async fn send_json<B: Serialize>(&self, method: &str, path: &str, body: &B) -> Result<()> {
        let bytes = serde_json::to_vec(body)?;
        let response = self.request(method, path, Some(bytes)).await?;
        if !(200..300).contains(&response.status) {
            return Err(Error::Status {
                status: response.status,
                body: String::from_utf8_lossy(&response.body).into_owned(),
            }
            .into());
        }
        Ok(())
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.request("GET", path, None).await?;
        if !(200..300).contains(&response.status) {
            return Err(Error::Status {
                status: response.status,
                body: String::from_utf8_lossy(&response.body).into_owned(),
            }
            .into());
        }
        Ok(serde_json::from_slice(&response.body)?)
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse> {
        let result = timeout(REQUEST_TIMEOUT, self.request_inner(method, path, body)).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(Error::Timeout(REQUEST_TIMEOUT).into()),
        }
    }

    async fn request_inner(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to Firecracker socket {}",
                    self.socket_path.display()
                )
            })?;

        let body = body.unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n"
        );
        if method != "GET" {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await?;
        if !body.is_empty() {
            stream.write_all(&body).await?;
        }
        stream.flush().await?;

        let raw = read_http_response(&mut stream).await?;
        parse_http_response(&raw)
    }
}

/// Read one HTTP response from a persistent Firecracker connection.
///
/// `read_to_end` is deliberately not used: Firecracker may keep the socket
/// open even when the request asked for `Connection: close`. Framing the body
/// with `Content-Length` (or chunked encoding) lets the client return as soon
/// as the response is complete instead of waiting for EOF until the timeout.
async fn read_http_response(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    loop {
        let mut buffer = [0u8; 8 * 1024];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(
                Error::Protocol(format!("response exceeds {MAX_RESPONSE_BYTES} bytes")).into(),
            );
        }

        let Some(header_end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&raw[..header_end])
            .map_err(|_| Error::Protocol("headers are not UTF-8".into()))?;
        let status = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        // 204/304 and 1xx responses have no body and end at the header block.
        if status == 204 || status == 304 || (100..200).contains(&status) {
            return Ok(raw);
        }
        if let Some(length) =
            header_value(headers, "content-length").and_then(|value| value.parse::<usize>().ok())
        {
            let needed = header_end
                .checked_add(4)
                .and_then(|offset| offset.checked_add(length))
                .ok_or_else(|| Error::Protocol("content length overflow".into()))?;
            if raw.len() >= needed {
                return Ok(raw);
            }
        } else if header_value(headers, "transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
        {
            // Keep reading until the chunked body (including its terminator)
            // is complete. A transient decode error means "not yet complete".
            if decode_chunked(&raw[header_end + 4..]).is_ok() {
                return Ok(raw);
            }
        } else {
            // No framing: EOF is the only delimiter.
        }
    }
    if raw.is_empty() {
        return Err(Error::Protocol("empty response".into()).into());
    }
    Ok(raw)
}

fn unsupported_path(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Error>().is_some_and(|err| {
        matches!(
            err,
            Error::Status { status: 400 | 404, body }
                if body.contains("Invalid request method and/or path")
                    || body.contains("ID cannot be empty")
        )
    })
}

/// Validate a Firecracker resource ID before putting it in a URL path.
fn path_id(id: &str) -> Result<&str> {
    if id.is_empty() {
        bail!("firecracker resource id cannot be empty");
    }
    if id.contains('/') || id.chars().any(char::is_control) {
        bail!("firecracker resource id contains an unsafe character: '{id}'");
    }
    Ok(id)
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn parse_http_response(raw: &[u8]) -> Result<HttpResponse> {
    if raw.len() > MAX_RESPONSE_BYTES {
        return Err(Error::Protocol(format!("response exceeds {MAX_RESPONSE_BYTES} bytes")).into());
    }
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| Error::Protocol("missing header terminator".into()))?;
    let headers = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| Error::Protocol("headers are not UTF-8".into()))?;
    let mut lines = headers.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| Error::Protocol("empty response".into()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| Error::Protocol(format!("malformed status line: {status_line}")))?
        .parse::<u16>()
        .map_err(|_| Error::Protocol(format!("invalid status code in {status_line}")))?;

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if name == "content-length" {
                content_length = value.parse::<usize>().ok();
            }
        }
    }

    let remainder = &raw[header_end + 4..];
    let body = if chunked {
        decode_chunked(remainder)?
    } else if let Some(length) = content_length {
        if remainder.len() < length {
            return Err(Error::Protocol(format!(
                "body truncated: expected {length} bytes, got {}",
                remainder.len()
            ))
            .into());
        }
        remainder[..length].to_vec()
    } else {
        remainder.to_vec()
    };

    Ok(HttpResponse { status, body })
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| Error::Protocol("chunk size missing CRLF".into()))?;
        let size_line = std::str::from_utf8(&data[..line_end])
            .map_err(|_| Error::Protocol("chunk size is not UTF-8".into()))?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| Error::Protocol(format!("invalid chunk size: {size_line}")))?;
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        let Some(needed) = size.checked_add(2) else {
            return Err(Error::Protocol(format!("chunk size overflow: {size_line}")).into());
        };
        if data.len() < needed {
            return Err(Error::Protocol("chunk truncated".into()).into());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[needed..];
    }
    Ok(out)
}

/// Convenience: is the API socket present and does it report a running VM?
pub async fn is_running(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    FirecrackerClient::new(socket_path)
        .instance_info()
        .await
        .map(|info| info.state == VmState::Running)
        .unwrap_or(false)
}

/// Validate that a path is usable as a Firecracker socket before spawning.
pub fn validate_socket_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("socket path is empty");
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        bail!("socket directory {} does not exist", parent.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Recorded {
        method: String,
        path: String,
        body: Vec<u8>,
    }

    async fn read_request(stream: &mut UnixStream) -> Recorded {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                let length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + length {
                    break;
                }
            }
        }
        let pos = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
        let mut first = headers.lines().next().unwrap().split_whitespace();
        let method = first.next().unwrap().to_string();
        let path = first.next().unwrap().to_string();
        let body = buf[pos + 4..].to_vec();
        Recorded { method, path, body }
    }

    fn spawn_mock(
        responses: Vec<(u16, String)>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        tokio::task::JoinHandle<Vec<Recorded>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded_task = recorded.clone();
        let handle = tokio::spawn(async move {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req = read_request(&mut stream).await;
                recorded_task.lock().await.push(req);
                let reason = if status == 204 { "No Content" } else { "OK" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            recorded.lock().await.clone()
        });
        (dir, path, handle)
    }

    #[tokio::test]
    async fn puts_boot_source_and_machine_config() {
        let (_dir, path, handle) = spawn_mock(vec![
            (204, String::new()),
            (204, String::new()),
            (204, String::new()),
        ]);
        let client = FirecrackerClient::new(&path);
        client
            .put_boot_source(&BootSource {
                kernel_image_path: "/img/vmlinux".into(),
                boot_args: "console=ttyS0".into(),
            })
            .await
            .unwrap();
        client
            .put_machine_config(&MachineConfig {
                vcpu_count: 2,
                mem_size_mib: 1024,
                smt: None,
                track_dirty_pages: Some(true),
            })
            .await
            .unwrap();
        client.action(Action::InstanceStart).await.unwrap();

        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "PUT");
        assert_eq!(requests[0].path, "/boot-source");
        let boot: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(boot["kernel_image_path"], "/img/vmlinux");
        assert_eq!(boot["boot_args"], "console=ttyS0");
        assert_eq!(requests[1].path, "/machine-config");
        let machine: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(machine["vcpu_count"], 2);
        assert_eq!(machine["mem_size_mib"], 1024);
        assert!(machine.get("smt").is_none());
        assert_eq!(requests[2].path, "/actions");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[2].body).unwrap()["action_type"],
            "InstanceStart"
        );
    }

    #[tokio::test]
    async fn puts_drives_and_vhost_user_fs_by_id() {
        let (_dir, path, handle) = spawn_mock(vec![(204, String::new()), (204, String::new())]);
        let client = FirecrackerClient::new(&path);
        client
            .put_drive(&Drive {
                drive_id: "rootfs".into(),
                path_on_host: "/img/rootfs.ext4".into(),
                is_root_device: true,
                is_read_only: false,
            })
            .await
            .unwrap();
        client
            .put_vhost_user_fs(&VhostUserFs {
                fs_id: "workspace".into(),
                socket: "/run/virtiofs.sock".into(),
                tag: "workspace".into(),
            })
            .await
            .unwrap();

        let requests = handle.await.unwrap();
        assert_eq!(requests[0].path, "/drives/rootfs");
        assert_eq!(requests[1].path, "/vhost-user-fs/workspace");
    }

    #[tokio::test]
    async fn falls_back_to_a_collection_vhost_user_fs_endpoint() {
        let (_dir, path, handle) = spawn_mock(vec![
            (
                400,
                r#"{"fault_message":"Invalid request method and/or path: PUT vhost-user-fs."}"#
                    .into(),
            ),
            (204, String::new()),
        ]);
        let client = FirecrackerClient::new(&path);
        client
            .put_vhost_user_fs(&VhostUserFs {
                fs_id: "workspace".into(),
                socket: "/run/virtiofs.sock".into(),
                tag: "workspace".into(),
            })
            .await
            .unwrap();
        let requests = handle.await.unwrap();
        assert_eq!(requests[0].path, "/vhost-user-fs/workspace");
        assert_eq!(requests[1].path, "/vhost-user-fs");
    }

    #[tokio::test]
    async fn unsupported_virtio_fs_reports_the_missing_build_capability() {
        let (_dir, path, _handle) = spawn_mock(vec![
            (
                400,
                r#"{"fault_message":"Invalid request method and/or path: PUT vhost-user-fs."}"#
                    .into(),
            ),
            (
                400,
                r#"{"fault_message":"Invalid request method and/or path: PUT vhost-user-fs."}"#
                    .into(),
            ),
        ]);
        let err = FirecrackerClient::new(&path)
            .put_vhost_user_fs(&VhostUserFs {
                fs_id: "workspace".into(),
                socket: "/run/virtiofs.sock".into(),
                tag: "workspace".into(),
            })
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("virtio-fs-enabled"), "{message}");
    }

    #[test]
    fn rejects_unsafe_resource_ids() {
        assert!(path_id("rootfs").is_ok());
        assert!(path_id("").is_err());
        assert!(path_id("../rootfs").is_err());
        assert!(path_id("root\nfs").is_err());
    }

    #[tokio::test]
    async fn pauses_and_resumes_via_patch_vm() {
        let (_dir, path, handle) = spawn_mock(vec![(204, String::new()), (204, String::new())]);
        let client = FirecrackerClient::new(&path);
        client.pause().await.unwrap();
        client.resume().await.unwrap();

        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "PATCH");
        assert_eq!(requests[0].path, "/vm");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["state"], "Paused");
        assert_eq!(requests[1].method, "PATCH");
        assert_eq!(requests[1].path, "/vm");
        let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(body["state"], "Resumed");
    }

    #[tokio::test]
    async fn reads_instance_info() {
        let (_dir, path, handle) = spawn_mock(vec![(
            200,
            r#"{"id":"hegn","state":"Running","vmm_version":"1.9.0","app_name":"Firecracker"}"#
                .into(),
        )]);
        let info = FirecrackerClient::new(&path).instance_info().await.unwrap();
        assert_eq!(info.state, VmState::Running);
        assert_eq!(info.vmm_version, "1.9.0");
        assert_eq!(handle.await.unwrap()[0].method, "GET");
    }

    #[tokio::test]
    async fn reads_a_framed_response_without_waiting_for_socket_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_request(&mut stream).await;
            let body = r#"{"id":"hegn","state":"Not started","vmm_version":"1.17.0","app_name":"Firecracker"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            // Firecracker may keep the connection open; the client must not
            // wait for this sleep/EOF to finish the response.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let info = tokio::time::timeout(
            Duration::from_millis(500),
            FirecrackerClient::new(&path).instance_info(),
        )
        .await
        .expect("client waited for EOF instead of Content-Length")
        .unwrap();
        assert_eq!(info.state, VmState::NotStarted);
        server.abort();
    }

    #[tokio::test]
    async fn no_content_responses_do_not_wait_for_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        tokio::time::timeout(
            Duration::from_millis(500),
            FirecrackerClient::new(&path).action(Action::InstanceStart),
        )
        .await
        .expect("client waited for EOF after a bodyless 204")
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn reads_stopped_instance_state() {
        // Firecracker spells the state with a space, unlike the PascalCase
        // variant name.
        let (_dir, path, handle) = spawn_mock(vec![(
            200,
            r#"{"id":"hegn","state":"Not started","vmm_version":"1.9.0","app_name":"Firecracker"}"#
                .into(),
        )]);
        let info = FirecrackerClient::new(&path).instance_info().await.unwrap();
        assert_eq!(info.state, VmState::NotStarted);
        handle.await.unwrap();
    }

    #[test]
    fn rejects_oversized_responses() {
        let mut raw = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".to_vec();
        raw.resize(MAX_RESPONSE_BYTES + 1, b'x');
        let err = format!("{:#}", parse_http_response(&raw).unwrap_err());
        assert!(err.contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn surfaces_api_errors() {
        let (_dir, path, handle) = spawn_mock(vec![(400, r#"{"fault_message":"nope"}"#.into())]);
        let err = FirecrackerClient::new(&path)
            .action(Action::InstanceStart)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 400"), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn missing_socket_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = FirecrackerClient::new(dir.path().join("missing.sock"))
            .instance_info()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed to connect"), "{err}");
    }

    #[test]
    fn decodes_chunked_bodies() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n4\r\n:1}\n\r\n0\r\n\r\n";
        let parsed = parse_http_response(raw).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, b"{\"a\":1}\n");
    }

    #[test]
    fn rejects_chunk_size_overflow() {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            usize::MAX
        );
        let err = format!("{:#}", parse_http_response(raw.as_bytes()).unwrap_err());
        assert!(err.contains("chunk size overflow"), "{err}");
    }

    #[test]
    fn rejects_truncated_responses() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort";
        assert!(parse_http_response(raw).is_err());
    }

    #[test]
    fn snapshot_specs_serialize_with_expected_keys() {
        let spec = SnapshotCreate {
            snapshot_path: "/snap/vm.snap".into(),
            mem_file_path: "/snap/vm.mem".into(),
            snapshot_type: Some("Full".into()),
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["snapshot_type"], "Full");
        let load = SnapshotLoad {
            snapshot_path: "/snap/vm.snap".into(),
            mem_file_path: "/snap/vm.mem".into(),
            track_dirty_pages: None,
            resume_vm: Some(true),
        };
        let json = serde_json::to_value(&load).unwrap();
        assert!(json.get("track_dirty_pages").is_none());
        assert_eq!(json["resume_vm"], true);
    }
}
