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

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// API responses are small; bound reads so a stuck or malicious socket peer
/// cannot make hegn buffer unbounded data in memory.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const API_PREFIX: &str = "/api/v1";

#[derive(Debug, Error)]
pub enum Error {
    #[error("cloud-hypervisor io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cloud-hypervisor returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("invalid cloud-hypervisor response: {0}")]
    Protocol(String),
    #[error("invalid cloud-hypervisor JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cloud-hypervisor request timed out after {0:?}")]
    Timeout(Duration),
}

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

/// A minimal Cloud Hypervisor API client.
#[derive(Debug, Clone)]
pub struct CloudHypervisorClient {
    socket_path: PathBuf,
}

impl CloudHypervisorClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// `PUT /api/v1/vm.info` -> state, memory and device tree.
    pub async fn info(&self) -> Result<VmInfo> {
        self.get("/vm.info").await
    }

    /// `PUT /api/v1/vm.pause`.
    pub async fn pause(&self) -> Result<()> {
        self.put_empty("/vm.pause").await
    }

    /// `PUT /api/v1/vm.resume`.
    pub async fn resume(&self) -> Result<()> {
        self.put_empty("/vm.resume").await
    }

    /// `PUT /api/v1/vm.snapshot`. The VM must be paused first.
    pub async fn snapshot(&self, spec: &SnapshotConfig) -> Result<()> {
        self.put_json("/vm.snapshot", spec).await
    }

    /// Ask the VMM to shut the guest down. hegn still verifies the process
    /// before sending signals, so this is a graceful first step only.
    pub async fn shutdown(&self) -> Result<()> {
        self.put_empty("/vm.shutdown").await
    }

    async fn put_empty(&self, path: &str) -> Result<()> {
        let response = self.request("PUT", path, Some(Vec::new())).await?;
        check_status(response.status, &response.body)
    }

    async fn put_json<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let bytes = serde_json::to_vec(body)?;
        let response = self.request("PUT", path, Some(bytes)).await?;
        check_status(response.status, &response.body)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.request("GET", path, None).await?;
        check_status(response.status, &response.body)?;
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
                    "failed to connect to Cloud Hypervisor API socket {}",
                    self.socket_path.display()
                )
            })?;

        let body = body.unwrap_or_default();
        let uri = format!("{API_PREFIX}{path}");
        let mut request = format!(
            "{method} {uri} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n"
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

fn check_status(status: u16, body: &[u8]) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    Err(Error::Status {
        status,
        body: String::from_utf8_lossy(body).into_owned(),
    }
    .into())
}

/// Read one HTTP response from a persistent Cloud Hypervisor connection.
///
/// `read_to_end` is deliberately not used: the VMM may keep the socket open
/// even when the request asked for `Connection: close`. Framing the body with
/// `Content-Length` (or chunked encoding) lets the client return as soon as the
/// response is complete instead of waiting for EOF until the timeout.
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

/// Convenience: is the API socket present and does it report a live VM?
pub async fn is_running(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    CloudHypervisorClient::new(socket_path)
        .info()
        .await
        .map(|info| info.state.is_alive())
        .unwrap_or(false)
}

/// Validate that a path is usable as a Cloud Hypervisor socket before spawning.
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
        let path = dir.path().join("cloud-hypervisor.sock");
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
    async fn pauses_and_resumes_via_the_v1_endpoints() {
        let (_dir, path, handle) = spawn_mock(vec![(204, String::new()), (204, String::new())]);
        let client = CloudHypervisorClient::new(&path);
        client.pause().await.unwrap();
        client.resume().await.unwrap();

        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "PUT");
        assert_eq!(requests[0].path, "/api/v1/vm.pause");
        assert!(requests[0].body.is_empty());
        assert_eq!(requests[1].method, "PUT");
        assert_eq!(requests[1].path, "/api/v1/vm.resume");
    }

    #[tokio::test]
    async fn reads_vm_info() {
        let (_dir, path, handle) = spawn_mock(vec![(
            200,
            r#"{"state":"Running","memory_actual_size":1073741824,"config":{"payload":{"kernel":"/img/vmlinux"}}}"#
                .into(),
        )]);
        let info = CloudHypervisorClient::new(&path).info().await.unwrap();
        assert_eq!(info.state, VmState::Running);
        assert_eq!(info.memory_actual_size, Some(1024 * 1024 * 1024));
        let requests = handle.await.unwrap();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/v1/vm.info");
    }

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

    #[tokio::test]
    async fn snapshot_sends_a_file_url() {
        let (_dir, path, handle) = spawn_mock(vec![(204, String::new())]);
        CloudHypervisorClient::new(&path)
            .snapshot(&SnapshotConfig {
                destination_url: "file:///snapshots/demo".into(),
            })
            .await
            .unwrap();

        let requests = handle.await.unwrap();
        assert_eq!(requests[0].path, "/api/v1/vm.snapshot");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["destination_url"], "file:///snapshots/demo");
    }

    #[tokio::test]
    async fn reads_a_framed_response_without_waiting_for_socket_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-hypervisor.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_request(&mut stream).await;
            let body = r#"{"state":"Running"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            // Cloud Hypervisor may keep the connection open; the client must
            // not wait for this sleep/EOF to finish the response.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let info = tokio::time::timeout(
            Duration::from_millis(500),
            CloudHypervisorClient::new(&path).info(),
        )
        .await
        .expect("client waited for EOF instead of Content-Length")
        .unwrap();
        assert_eq!(info.state, VmState::Running);
        server.abort();
    }

    #[tokio::test]
    async fn no_content_responses_do_not_wait_for_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-hypervisor.sock");
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
            CloudHypervisorClient::new(&path).pause(),
        )
        .await
        .expect("client waited for EOF after a bodyless 204")
        .unwrap();
        server.abort();
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
        let (_dir, path, handle) = spawn_mock(vec![(400, r#"{"error":"nope"}"#.into())]);
        let err = CloudHypervisorClient::new(&path).pause().await.unwrap_err();
        assert!(err.to_string().contains("HTTP 400"), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn missing_socket_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = CloudHypervisorClient::new(dir.path().join("missing.sock"))
            .info()
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
    fn socket_path_validation_rejects_missing_parent() {
        let err = validate_socket_path(Path::new("/definitely/missing/cloud-hypervisor.sock"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(validate_socket_path(Path::new("")).is_err());
    }
}
